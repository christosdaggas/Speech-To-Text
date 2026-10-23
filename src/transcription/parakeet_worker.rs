// Copyright (C) 2026 Christos A. Daggas
// SPDX-License-Identifier: MIT

//! One recognizer per private worker, with one fresh native stream per clip.
//! Anonymous Unix sockets carry bounded PCM/results; no ports or audio files.

use super::file_identity::FileIdentity;
use crate::cancel::CancelToken;
use crate::error::{AppError, AppResult};
use std::ffi::{CString, OsString};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const READY: &[u8; 4] = b"STP1";
const IO_POLL: Duration = Duration::from_millis(20);
const MAX_RESULT: usize = 8 * 1024 * 1024;
const WORKER_FD: i32 = 3;

pub(super) struct WorkerSpec {
    program: PathBuf,
    arguments: Vec<OsString>,
    identity: Vec<FileIdentity>,
    threads: u32,
    #[cfg(test)]
    fixture_mode: Option<&'static str>,
}

impl WorkerSpec {
    pub fn new(runtime: &Path, model: &Path, threads: u32) -> AppResult<Self> {
        let library = runtime.join("lib/libsherpa-onnx-c-api.so");
        let threads = threads.clamp(1, 32);
        let mut paths = vec![library.clone(), runtime.join("lib/libonnxruntime.so")];
        paths.extend(
            [
                "encoder.int8.onnx",
                "decoder.int8.onnx",
                "joiner.int8.onnx",
                "tokens.txt",
            ]
            .map(|file| model.join(file)),
        );
        let identity = paths
            .iter()
            .map(|path| FileIdentity::read(path))
            .collect::<std::io::Result<_>>()?;
        Ok(Self {
            program: std::env::current_exe()?,
            arguments: vec![
                "--parakeet-worker".into(),
                library.into_os_string(),
                model.as_os_str().to_owned(),
                threads.to_string().into(),
            ],
            identity,
            threads,
            #[cfg(test)]
            fixture_mode: None,
        })
    }
}

struct Running {
    child: Child,
    stream: UnixStream,
    identity: Vec<FileIdentity>,
    threads: u32,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(super) struct WarmWorker {
    running: Mutex<Option<Running>>,
    request: Mutex<()>,
    epoch: AtomicU64,
}

impl WarmWorker {
    pub const fn new() -> Self {
        Self {
            running: Mutex::new(None),
            request: Mutex::new(()),
            epoch: AtomicU64::new(0),
        }
    }

    pub fn shutdown(&self) {
        // Bump before taking the slot so a concurrent startup cannot republish it.
        self.epoch.fetch_add(1, Ordering::SeqCst);
        let old = self
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        drop(old);
    }

    fn check(&self, epoch: u64, cancel: Option<&CancelToken>, deadline: Instant) -> AppResult<()> {
        if self.epoch.load(Ordering::SeqCst) != epoch || cancel.is_some_and(CancelToken::is_aborted)
        {
            return Err(AppError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(AppError::DeadlineExceeded);
        }
        Ok(())
    }

    pub fn transcribe(
        &self,
        spec: &WorkerSpec,
        audio: &[f32],
        cancel: Option<&CancelToken>,
        deadline: Instant,
    ) -> AppResult<Vec<u8>> {
        if audio.len() > crate::limits::MAX_DECODED_SAMPLES || audio.len() > i32::MAX as usize {
            return Err(AppError::Transcription(
                "Parakeet audio exceeds the sample limit".into(),
            ));
        }
        let epoch = self.epoch.load(Ordering::SeqCst);
        let _request = loop {
            self.check(epoch, cancel, deadline)?;
            match self.request.try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::Poisoned(error)) => break error.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => std::thread::sleep(IO_POLL),
            }
        };
        let result = (|| {
            let mut stream = self.connection(spec, epoch, cancel, deadline)?;
            self.write(
                &mut stream,
                &(audio.len() as u32).to_le_bytes(),
                epoch,
                cancel,
                deadline,
            )?;
            for samples in audio.chunks(4096) {
                let mut bytes = [0u8; 8192];
                for (sample, output) in samples.iter().zip(bytes.as_chunks_mut::<2>().0.iter_mut())
                {
                    // Preserve the previous WAV/CLI path's PCM quantization.
                    output.copy_from_slice(
                        &((sample.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes(),
                    );
                }
                self.write(
                    &mut stream,
                    &bytes[..samples.len() * 2],
                    epoch,
                    cancel,
                    deadline,
                )?;
            }
            let mut size = [0u8; 4];
            self.read(&mut stream, &mut size, epoch, cancel, deadline)?;
            let size = u32::from_le_bytes(size) as usize;
            if size == 0 || size > MAX_RESULT {
                return Err(AppError::Transcription(
                    "Invalid Parakeet worker response size".into(),
                ));
            }
            let mut result = vec![0u8; size];
            self.read(&mut stream, &mut result, epoch, cancel, deadline)?;
            self.check(epoch, cancel, deadline)?;
            Ok(result)
        })();
        if result.is_err() {
            let old = self
                .running
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            drop(old);
        }
        result
    }

    fn connection(
        &self,
        spec: &WorkerSpec,
        epoch: u64,
        cancel: Option<&CancelToken>,
        deadline: Instant,
    ) -> AppResult<UnixStream> {
        let stale = {
            let mut slot = self.running.lock().unwrap_or_else(|e| e.into_inner());
            self.check(epoch, cancel, deadline)?;
            if let Some(running) = slot.as_mut() {
                if running.identity == spec.identity
                    && running.threads == spec.threads
                    && matches!(running.child.try_wait(), Ok(None))
                {
                    return Ok(running.stream.try_clone()?);
                }
            }
            slot.take()
        };
        drop(stale);
        let (stream, child_stream) = UnixStream::pair()?;
        stream.set_read_timeout(Some(IO_POLL))?;
        stream.set_write_timeout(Some(IO_POLL))?;
        let fd = child_stream.as_raw_fd();
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.arguments)
            .env_clear()
            .env("LANG", "C.UTF-8")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(test)]
        if let Some(mode) = spec.fixture_mode {
            command.env("STT_PARAKEET_FIXTURE", mode);
        }
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: pre_exec calls only async-signal-safe syscalls. The source
            // socket stays owned/open until spawn completes. FD 3 is child-only.
            unsafe {
                command.pre_exec(move || {
                    let parent = libc::getppid();
                    if libc::dup2(fd, WORKER_FD) < 0
                        || libc::fcntl(WORKER_FD, libc::F_SETFD, 0) < 0
                        || libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) < 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    if parent == 1 || libc::getppid() != parent {
                        return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
                    }
                    Ok(())
                });
            }
        }
        let child = super::sidecar_server::spawn_on_dedicated_thread(command)?;
        drop(child_stream);
        let mut running = Running {
            child,
            stream,
            identity: spec.identity.clone(),
            threads: spec.threads,
        };
        let mut ready = [0u8; 4];
        self.read(&mut running.stream, &mut ready, epoch, cancel, deadline)?;
        if &ready != READY {
            return Err(AppError::Transcription(
                "Invalid Parakeet worker handshake".into(),
            ));
        }
        let stream = running.stream.try_clone()?;
        let mut slot = self.running.lock().unwrap_or_else(|e| e.into_inner());
        self.check(epoch, cancel, deadline)?;
        *slot = Some(running);
        Ok(stream)
    }

    fn read(
        &self,
        stream: &mut UnixStream,
        mut bytes: &mut [u8],
        epoch: u64,
        cancel: Option<&CancelToken>,
        deadline: Instant,
    ) -> AppResult<()> {
        while !bytes.is_empty() {
            self.check(epoch, cancel, deadline)?;
            match stream.read(bytes) {
                Ok(0) => {
                    self.check(epoch, cancel, deadline)?;
                    return Err(AppError::Transcription(
                        "Parakeet worker disconnected".into(),
                    ));
                }
                Ok(count) => {
                    bytes = &mut bytes[count..];
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => {
                    self.check(epoch, cancel, deadline)?;
                    return Err(error.into());
                }
            }
        }
        Ok(())
    }

    fn write(
        &self,
        stream: &mut UnixStream,
        mut bytes: &[u8],
        epoch: u64,
        cancel: Option<&CancelToken>,
        deadline: Instant,
    ) -> AppResult<()> {
        while !bytes.is_empty() {
            self.check(epoch, cancel, deadline)?;
            match stream.write(bytes) {
                Ok(0) => {
                    self.check(epoch, cancel, deadline)?;
                    return Err(AppError::Transcription(
                        "Parakeet worker disconnected".into(),
                    ));
                }
                Ok(count) => {
                    bytes = &bytes[count..];
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => {
                    self.check(epoch, cancel, deadline)?;
                    return Err(error.into());
                }
            }
        }
        Ok(())
    }
}

impl Drop for WarmWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

unsafe extern "C" {
    fn stt_parakeet_create(
        library: *const libc::c_char,
        encoder: *const libc::c_char,
        decoder: *const libc::c_char,
        joiner: *const libc::c_char,
        tokens: *const libc::c_char,
        threads: i32,
    ) -> *mut libc::c_void;
    fn stt_parakeet_destroy(handle: *mut libc::c_void);
    fn stt_parakeet_decode(
        handle: *mut libc::c_void,
        samples: *const f32,
        count: i32,
    ) -> *const libc::c_char;
    fn stt_parakeet_free_result(handle: *mut libc::c_void, json: *const libc::c_char);
}

struct Recognizer(std::ptr::NonNull<libc::c_void>);

impl Recognizer {
    fn new(library: &Path, model: &Path, threads: u32) -> AppResult<Self> {
        let paths = [
            library.to_owned(),
            model.join("encoder.int8.onnx"),
            model.join("decoder.int8.onnx"),
            model.join("joiner.int8.onnx"),
            model.join("tokens.txt"),
        ];
        let paths = paths
            .iter()
            .map(|p| CString::new(p.as_os_str().as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| AppError::Transcription("Invalid Parakeet path".into()))?;
        // SAFETY: all path strings are NUL-terminated and alive for the call;
        // the C shim copies configuration into an owned recognizer before returning.
        let handle = unsafe {
            stt_parakeet_create(
                paths[0].as_ptr(),
                paths[1].as_ptr(),
                paths[2].as_ptr(),
                paths[3].as_ptr(),
                paths[4].as_ptr(),
                threads.clamp(1, 32) as i32,
            )
        };
        std::ptr::NonNull::new(handle).map(Self).ok_or_else(|| {
            AppError::Transcription("Failed to load the pinned Parakeet recognizer".into())
        })
    }

    fn decode(&mut self, samples: &[f32]) -> AppResult<Vec<u8>> {
        if samples.is_empty() {
            return Ok(br#"{"text":""}"#.to_vec());
        }
        let count = i32::try_from(samples.len()).map_err(|_| {
            AppError::Transcription("Parakeet audio exceeds the sample limit".into())
        })?;
        // SAFETY: exclusive access to a live recognizer, and the complete PCM
        // slice remains valid through this synchronous call. Result ownership is
        // transferred by the shim and released with the matching runtime function.
        let json = unsafe { stt_parakeet_decode(self.0.as_ptr(), samples.as_ptr(), count) };
        if json.is_null() {
            return Err(AppError::Transcription("Parakeet decode failed".into()));
        }
        // SAFETY: the pinned C API returns an owned, NUL-terminated JSON string.
        let bytes = unsafe { std::ffi::CStr::from_ptr(json) }.to_bytes();
        let result = if bytes.len() <= MAX_RESULT {
            Ok(bytes.to_vec())
        } else {
            Err(AppError::Transcription(
                "Parakeet result exceeds the size limit".into(),
            ))
        };
        // SAFETY: json belongs to this recognizer's loaded library, is not used
        // after this call and has not been freed previously.
        unsafe {
            stt_parakeet_free_result(self.0.as_ptr(), json);
        }
        result
    }
}

impl Drop for Recognizer {
    fn drop(&mut self) {
        // SAFETY: this handle is uniquely owned; streams/results have already
        // been destroyed before unloading their library.
        unsafe {
            stt_parakeet_destroy(self.0.as_ptr());
        }
    }
}

pub(super) fn run_worker(library: &Path, model: &Path, threads: u32) -> AppResult<()> {
    let socket = inherited_socket()?;
    let mut recognizer = Recognizer::new(library, model, threads)?;
    serve(socket, |samples| recognizer.decode(samples))
}

fn inherited_socket() -> std::io::Result<UnixStream> {
    let mut kind: libc::c_int = 0;
    let mut length = std::mem::size_of_val(&kind) as libc::socklen_t;
    // SAFETY: both output pointers refer to live, correctly-sized stack values.
    // getsockopt rejects a missing/non-socket FD without assuming ownership.
    let result = unsafe {
        libc::getsockopt(
            WORKER_FD,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut kind as *mut libc::c_int).cast(),
            &mut length,
        )
    };
    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if kind != libc::SOCK_STREAM {
        return Err(std::io::Error::other("Invalid worker socket"));
    }
    // SAFETY: the launcher gives this process exclusive ownership of FD 3,
    // validated above. This entrypoint runs before GTK/Tokio or other FD owners.
    let socket = unsafe { UnixStream::from_raw_fd(WORKER_FD) };
    socket.peer_addr()?;
    Ok(socket)
}

fn serve(
    mut socket: UnixStream,
    mut decode: impl FnMut(&[f32]) -> AppResult<Vec<u8>>,
) -> AppResult<()> {
    socket.write_all(READY)?;
    loop {
        let mut header = [0u8; 4];
        match socket.read(&mut header[..1]) {
            Ok(0) => return Ok(()),
            Ok(_) => socket.read_exact(&mut header[1..])?,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
        let count = u32::from_le_bytes(header) as usize;
        if count > crate::limits::MAX_DECODED_SAMPLES || count > i32::MAX as usize {
            return Err(AppError::Transcription(
                "Parakeet audio exceeds the sample limit".into(),
            ));
        }
        let mut samples = Vec::new();
        samples
            .try_reserve_exact(count)
            .map_err(|_| AppError::Transcription("Not enough memory for Parakeet audio".into()))?;
        let mut buffer = [0u8; 8192];
        while samples.len() < count {
            let size = ((count - samples.len()) * 2).min(buffer.len());
            socket.read_exact(&mut buffer[..size])?;
            samples.extend(
                buffer[..size]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0),
            );
        }
        let result = decode(&samples)?;
        if result.is_empty() || result.len() > MAX_RESULT {
            return Err(AppError::Transcription(
                "Invalid Parakeet result size".into(),
            ));
        }
        socket.write_all(&(result.len() as u32).to_le_bytes())?;
        socket.write_all(&result)?;
        socket.flush()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(dir: &Path, mode: &'static str) -> WorkerSpec {
        let path = dir.join("model");
        if !path.exists() {
            std::fs::write(&path, b"synthetic model identity").unwrap();
        }
        WorkerSpec {
            program: std::env::current_exe().unwrap(),
            arguments: [
                "--exact",
                "transcription::parakeet_worker::tests::worker_fixture",
                "--ignored",
                "--nocapture",
            ]
            .map(OsString::from)
            .to_vec(),
            identity: vec![FileIdentity::read(&path).unwrap()],
            threads: 2,
            fixture_mode: Some(mode),
        }
    }

    fn request(cache: &WarmWorker, spec: &WorkerSpec) -> AppResult<Vec<u8>> {
        cache.transcribe(
            spec,
            &[0.25; 100],
            None,
            Instant::now() + Duration::from_secs(5),
        )
    }

    fn pid(cache: &WarmWorker) -> u32 {
        cache.running.lock().unwrap().as_ref().unwrap().child.id()
    }

    fn assert_reaped(pid: u32) {
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "worker was not reaped"
        );
    }

    #[test]
    #[ignore = "private subprocess fixture, invoked only by worker lifecycle tests"]
    fn worker_fixture() {
        let mode = std::env::var("STT_PARAKEET_FIXTURE").expect("fixture mode");
        let mut socket = inherited_socket().unwrap();
        if mode == "startup-stall" {
            std::thread::sleep(Duration::from_secs(30));
            return;
        }
        if mode == "bad-handshake" {
            socket.write_all(b"NOPE").unwrap();
            return;
        }
        let mut count = 0;
        let _ = serve(socket, |samples| {
            if mode == "decode-stall" {
                std::thread::sleep(Duration::from_secs(30));
            }
            count += 1;
            assert!(samples.iter().all(|value| (*value - 0.25).abs() < 0.001));
            Ok(serde_json::json!({"text": format!("clip {count}")})
                .to_string()
                .into_bytes())
        });
    }

    #[test]
    fn reuses_worker_and_replaces_it_for_changed_models_threads_or_exit() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WarmWorker::new();
        let mut configuration = spec(dir.path(), "normal");
        let first = request(&cache, &configuration).unwrap();
        let first_pid = pid(&cache);
        let second = request(&cache, &configuration).unwrap();
        assert_eq!(pid(&cache), first_pid);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&first).unwrap()["text"],
            "clip 1"
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&second).unwrap()["text"],
            "clip 2"
        );
        configuration.threads = 3;
        request(&cache, &configuration).unwrap();
        assert_reaped(first_pid);
        let second_pid = pid(&cache);
        let replacement = dir.path().join("replacement");
        std::fs::write(&replacement, b"synthetic model identity").unwrap();
        std::fs::rename(replacement, dir.path().join("model")).unwrap();
        configuration.identity = vec![FileIdentity::read(&dir.path().join("model")).unwrap()];
        request(&cache, &configuration).unwrap();
        assert_reaped(second_pid);
        let third_pid = pid(&cache);
        {
            let mut slot = cache.running.lock().unwrap();
            slot.as_mut().unwrap().child.kill().unwrap();
            slot.as_mut().unwrap().child.wait().unwrap();
        }
        request(&cache, &configuration).unwrap();
        assert_reaped(third_pid);
        let final_pid = pid(&cache);
        cache.shutdown();
        assert_reaped(final_pid);
    }

    #[test]
    fn cancellation_and_shutdown_interrupt_running_inference_and_allow_next_take() {
        for shutdown in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let cache = std::sync::Arc::new(WarmWorker::new());
            let normal = spec(dir.path(), "normal");
            request(&cache, &normal).unwrap();
            // A new worker's handshake completes before its simulated decode stalls.
            cache.shutdown();
            let stalled = spec(dir.path(), "decode-stall");
            let token = CancelToken::new();
            let (work_cache, work_token) = (cache.clone(), token.clone());
            let worker = std::thread::spawn(move || {
                work_cache.transcribe(
                    &stalled,
                    &[0.25; 100],
                    Some(&work_token),
                    Instant::now() + Duration::from_secs(5),
                )
            });
            let limit = Instant::now() + Duration::from_secs(3);
            let worker_pid = loop {
                if cache.running.lock().unwrap().is_some() {
                    break pid(&cache);
                }
                assert!(Instant::now() < limit, "worker never became ready");
                std::thread::sleep(IO_POLL);
            };
            if shutdown {
                cache.shutdown();
            } else {
                token.abort();
            }
            assert!(matches!(worker.join().unwrap(), Err(AppError::Cancelled)));
            assert_reaped(worker_pid);
            request(&cache, &normal).unwrap();
        }
    }

    #[test]
    fn deadlines_handshake_failure_and_lock_wait_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let cache = WarmWorker::new();
        for mode in ["startup-stall", "decode-stall", "bad-handshake"] {
            let result = cache.transcribe(
                &spec(dir.path(), mode),
                &[0.25; 100],
                None,
                Instant::now() + Duration::from_millis(300),
            );
            assert!(result.is_err());
            assert!(cache.running.lock().unwrap().is_none());
        }
        let guard = cache.request.lock().unwrap();
        let token = CancelToken::new();
        token.abort();
        assert!(matches!(
            cache.transcribe(
                &spec(dir.path(), "normal"),
                &[],
                Some(&token),
                Instant::now() + Duration::from_secs(1)
            ),
            Err(AppError::Cancelled)
        ));
        drop(guard);
        request(&cache, &spec(dir.path(), "normal")).unwrap();
    }

    #[test]
    fn protocol_rejects_oversized_audio_before_decoding() {
        let (mut client, worker) = UnixStream::pair().unwrap();
        let handle =
            std::thread::spawn(move || serve(worker, |_| panic!("invalid request reached decode")));
        let mut ready = [0u8; 4];
        client.read_exact(&mut ready).unwrap();
        assert_eq!(&ready, READY);
        client.write_all(&u32::MAX.to_le_bytes()).unwrap();
        assert!(handle.join().unwrap().is_err());
    }

    #[test]
    #[ignore = "requires the pinned runtime/model, synthetic WAV and a built application; no downloads"]
    fn real_warm_recognizer_smoke() {
        let runtime =
            PathBuf::from(std::env::var_os("STT_PARAKEET_RUNTIME").expect("runtime fixture"));
        let model = PathBuf::from(std::env::var_os("STT_PARAKEET_MODEL").expect("model fixture"));
        let wav =
            PathBuf::from(std::env::var_os("STT_PARAKEET_WAV").expect("synthetic speech fixture"));
        let mut configuration = WorkerSpec::new(&runtime, &model, 4).unwrap();
        configuration.program =
            PathBuf::from(std::env::var_os("STT_PARAKEET_EXE").expect("built application"));
        let audio = crate::audio::file_decoder::decode_audio_file(&wav).unwrap();
        let baseline = super::super::parakeet::decode(
            &runtime.join("bin/sherpa-onnx-offline"),
            &model,
            &audio.samples,
            4,
            None,
        )
        .unwrap()
        .text;
        let cache = WarmWorker::new();
        let mut times = vec![];
        let mut texts = vec![];
        let mut worker_pid = None;
        for _ in 0..3 {
            let started = Instant::now();
            let result = cache
                .transcribe(
                    &configuration,
                    &audio.samples,
                    None,
                    Instant::now() + Duration::from_secs(120),
                )
                .unwrap();
            times.push(started.elapsed().as_millis());
            let json: serde_json::Value = serde_json::from_slice(&result).unwrap();
            let text = json["text"].as_str().unwrap().trim().to_owned();
            assert!(!text.is_empty(), "synthetic speech returned no transcript");
            assert_eq!(
                text,
                baseline.trim(),
                "warm inference differs from the previous CLI path"
            );
            texts.push(text);
            match worker_pid {
                None => worker_pid = Some(pid(&cache)),
                Some(expected) => assert_eq!(pid(&cache), expected),
            }
        }
        assert!(texts.windows(2).all(|pair| pair[0] == pair[1]));
        cache.shutdown();
        assert_reaped(worker_pid.unwrap());
        println!(
            "PARAKEET_WARM_REUSE_PASSED cold_ms={} warm_ms={},{}",
            times[0], times[1], times[2]
        );
    }
}
