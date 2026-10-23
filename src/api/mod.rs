// Copyright (C) 2026 Christos A. Daggas
// SPDX-License-Identifier: MIT

//! Opt-in, localhost-only HTTP API so other local apps (and local web pages)
//! can POST audio files for transcription and optional translation.
//!
//! Security posture (see also `THREATMODEL.md`):
//! - Binds `127.0.0.1` ONLY — never `0.0.0.0`. Enforced in code, not config.
//! - Requires a bearer token by default (random, stored in the system keyring,
//!   never written to the config or any log).
//! - Rejects non-loopback `Host` headers (DNS-rebinding guard).
//! - Caps the upload size before decoding, bounds the inference queue (429 on
//!   overflow), and times out a stuck request.
//! - Never logs the transcript, the audio, the temp path, or the token.
//! - Does NOT persist results to the in-app History.

mod handlers;
mod server;

use std::net::Ipv4Addr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::Response;

use crate::recording::{DictationOutcome, DictationParams};
use crate::transcription::ModelCatalog;

type Resp = Response<Full<Bytes>>;

/// Max bytes accepted for one upload before decoding (256 MiB). Far below the
/// 2 GiB file-drop ceiling — API audio is modest and we buffer it in memory.
pub const MAX_API_UPLOAD_BYTES: usize = 256 * 1024 * 1024;

/// Overall per-request timeout (10 min): covers a cold model load plus a long
/// clip, without letting a stuck client pin a worker forever.
pub const REQUEST_TIMEOUT_SECS: u64 = 600;

/// Maximum idle time between request-body frames. Local uploads should make
/// steady progress; this prevents slow clients from holding admission slots.
pub const BODY_FRAME_TIMEOUT_SECS: u64 = 30;

/// Inference queue depth. The single worker runs one job at a time (matching the
/// engine Mutex); excess concurrent requests get 429.
const JOB_QUEUE_DEPTH: usize = 2;

/// Limit expensive body buffering before requests reach the inference queue.
const MAX_CONCURRENT_UPLOADS: usize = 2;

/// Bound keep-alive connections and their Tokio tasks.
const MAX_API_CONNECTIONS: usize = 16;

struct Job {
    queued_at: std::time::Instant,
    /// Uploaded audio, written to a private temp file (unlinked when dropped).
    audio: tempfile::TempPath,
    params: DictationParams,
    reply: async_channel::Sender<Result<DictationOutcome, String>>,
    cancel: crate::cancel::CancelToken,
}

#[derive(Clone)]
struct ServerState {
    timeouts: Timeouts,
    cancel: crate::cancel::CancelToken,
    /// Bearer token required on requests, or `None` when token auth is disabled.
    token: Option<Arc<String>>,
    /// Bounded queue to the inference worker.
    jobs: async_channel::Sender<Job>,
    /// Admission control acquired before an upload body is read into memory.
    uploads: Arc<tokio::sync::Semaphore>,
    /// Connection limit shared by the accept loop.
    connections: Arc<tokio::sync::Semaphore>,
    catalog: Arc<ModelCatalog>,
}

#[derive(Clone, Copy)]
struct Timeouts {
    headers: std::time::Duration,
    upload: std::time::Duration,
    inference: std::time::Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            headers: std::time::Duration::from_secs(15),
            upload: std::time::Duration::from_secs(120),
            inference: std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS),
        }
    }
}

/// Handle to a running server. Dropping it (or calling [`ApiServerHandle::stop`])
/// signals a graceful shutdown and closes the port.
pub struct ApiServerHandle {
    cancel: crate::cancel::CancelToken,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    port: u16,
}

impl ApiServerHandle {
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn stop(mut self) {
        self.signal_stop();
    }

    fn signal_stop(&mut self) {
        self.cancel.abort();
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for ApiServerHandle {
    fn drop(&mut self) {
        self.signal_stop();
    }
}

/// Generate a fresh 256-bit token (two UUIDv4s, hex) suitable as a bearer token.
pub fn generate_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Constant-time byte comparison (no early-exit timing leak). The length check
/// can leak length, which is fine for fixed-length tokens.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Start the API server bound to `127.0.0.1:port`. Binds synchronously (so a
/// "port in use" error can be surfaced to the UI), then spawns the hyper accept
/// loop on the global Tokio runtime and a dedicated OS thread for blocking
/// inference. `token` is the bearer token to require, or `None` to disable auth.
pub fn start(
    engine: crate::recording::SharedEngine,
    catalog: Arc<ModelCatalog>,
    port: u16,
    token: Option<String>,
) -> Result<ApiServerHandle, String> {
    let std_listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .map_err(|e| format!("Could not bind 127.0.0.1:{port}: {e}"))?;
    std_listener
        .set_nonblocking(true)
        .map_err(|e| format!("Could not configure listener: {e}"))?;
    let port = std_listener.local_addr().map_err(|e| e.to_string())?.port();

    let (job_tx, job_rx) = async_channel::bounded::<Job>(JOB_QUEUE_DEPTH);
    let worker = std::thread::Builder::new()
        .name("api-transcribe".into())
        .spawn(move || worker_loop(engine, job_rx))
        .map_err(|e| format!("Could not start worker thread: {e}"))?;

    let state = ServerState {
        timeouts: Timeouts::default(),
        cancel: crate::cancel::CancelToken::new(),
        token: token.map(Arc::new),
        jobs: job_tx,
        uploads: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_UPLOADS)),
        connections: Arc::new(tokio::sync::Semaphore::new(MAX_API_CONNECTIONS)),
        catalog,
    };

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let cancel = state.cancel.clone();
    crate::application::tokio_runtime().spawn(async move {
        server::serve(std_listener, state, shutdown_rx).await;
        if let Err(error) = tokio::task::spawn_blocking(move || worker.join()).await {
            tracing::warn!("API worker join task failed: {error}");
        }
    });

    tracing::info!("API server listening on 127.0.0.1:{port}");
    Ok(ApiServerHandle {
        cancel,
        shutdown: Some(shutdown_tx),
        port,
    })
}

/// Dedicated inference worker: pulls jobs off the bounded queue and runs the
/// blocking decode + transcription, serialized on the shared engine. Exits when
/// the queue's senders are all dropped (i.e. the server stopped).
fn worker_loop(engine: crate::recording::SharedEngine, jobs: async_channel::Receiver<Job>) {
    while let Ok(job) = jobs.recv_blocking() {
        if job.cancel.is_aborted() {
            continue;
        }
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_job(&engine, &job)))
                .unwrap_or_else(|_| {
                    Err("Transcription worker recovered from an invalid request.".into())
                });
        let _ = job.reply.send_blocking(result);
        // `job.audio` (TempPath) drops here → the temp file is unlinked.
    }
    tracing::debug!("API inference worker stopped");
}

fn run_job(engine: &crate::recording::SharedEngine, job: &Job) -> Result<DictationOutcome, String> {
    let queue_ms = crate::diagnostics::milliseconds(job.queued_at);
    let checkpoint = || {
        if job.cancel.is_aborted() {
            Err("Transcription cancelled".to_string())
        } else {
            Ok(())
        }
    };
    checkpoint()?;
    let prepare_start = std::time::Instant::now();
    let audio = crate::audio::file_decoder::decode_audio_file_cancellable(
        job.audio.as_ref(),
        Some(&job.cancel),
    )
    .map_err(|e| e.user_message())?;
    let prepare_ms = crate::diagnostics::milliseconds(prepare_start);
    checkpoint()?;
    crate::recording::run_transcription(
        engine,
        &audio.samples,
        &job.params,
        audio.source_duration_secs,
        Some(job.cancel.clone()),
    )
    .map(|mut outcome| {
        outcome.diagnostics.queue_ms = queue_ms;
        outcome.diagnostics.prepare_ms = prepare_ms;
        crate::diagnostics::record(&outcome.diagnostics);
        outcome.shift_timestamps(audio.leading_offset_ms);
        outcome
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::Mutex;

    #[test]
    fn constant_time_eq_matches_only_equal_slices() {
        assert!(constant_time_eq(b"abc123", b"abc123"));
        assert!(!constant_time_eq(b"abc123", b"abc124"));
        assert!(!constant_time_eq(b"abc", b"abcd")); // different length
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn generated_token_is_256_bit_hex() {
        let t = generate_token();
        assert_eq!(t.len(), 64, "two UUIDv4 simple hex = 64 chars");
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(generate_token(), generate_token());
    }

    #[test]
    fn disconnected_request_cancels_inference_and_shutdown_closes_queue() {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let (jobs, received) = async_channel::bounded(2);
        let cancel = crate::cancel::CancelToken::new();
        let state = ServerState {
            timeouts: Timeouts::default(),
            token: None,
            jobs,
            cancel: cancel.clone(),
            uploads: Arc::new(tokio::sync::Semaphore::new(2)),
            connections: Arc::new(tokio::sync::Semaphore::new(16)),
            catalog: Arc::new(ModelCatalog::new()),
        };
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let runtime = crate::application::tokio_runtime();
        let server = runtime.spawn(server::serve(listener, state, stopped));
        let mut stream = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        stream
            .set_write_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        stream.write_all(b"POST /v1/transcribe HTTP/1.1\r\nHost: localhost\r\nContent-Length: 7\r\n\r\nfixture").unwrap();
        let job = runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(3), received.recv())
                .await
                .unwrap()
                .unwrap()
        });
        drop(stream);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !job.cancel.is_aborted() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            job.cancel.is_aborted(),
            "disconnect must cancel active inference"
        );
        stop.send(()).unwrap();
        runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(3), server)
                .await
                .unwrap()
                .unwrap();
        });
        assert!(cancel.is_aborted());
        assert!(received.is_closed());
    }

    #[test]
    fn header_upload_and_inference_deadlines_release_resources() {
        use std::time::Duration;
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let (jobs, received) = async_channel::bounded(2);
        let uploads = Arc::new(tokio::sync::Semaphore::new(1));
        let state = ServerState {
            timeouts: Timeouts {
                headers: Duration::from_millis(100),
                upload: Duration::from_millis(100),
                inference: Duration::from_millis(100),
            },
            token: None,
            jobs,
            cancel: crate::cancel::CancelToken::new(),
            uploads: uploads.clone(),
            connections: Arc::new(tokio::sync::Semaphore::new(2)),
            catalog: Arc::new(ModelCatalog::new()),
        };
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let runtime = crate::application::tokio_runtime();
        let server = runtime.spawn(server::serve(listener, state, stopped));
        let connect = || {
            let stream = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream
        };
        // An incomplete header cannot pin a connection indefinitely.
        let mut header = connect();
        header
            .write_all(b"GET /v1/health HTTP/1.1\r\nHost:")
            .unwrap();
        let mut response = String::new();
        header.read_to_string(&mut response).unwrap();
        assert!(response.is_empty() || response.contains("408"));
        // Keep-alive is explicitly disabled, even if the client asks for it.
        let mut idle = connect();
        idle.write_all(
            b"GET /v1/health HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n",
        )
        .unwrap();
        response.clear();
        idle.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200"));
        // Progress within the per-frame budget cannot extend the total budget.
        let mut upload = connect();
        upload
            .write_all(
                b"POST /v1/transcribe HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\n\r\na",
            )
            .unwrap();
        let mut trickle = upload.try_clone().unwrap();
        let writer = std::thread::spawn(move || {
            for _ in 0..10 {
                std::thread::sleep(Duration::from_millis(20));
                if trickle.write_all(b"b").is_err() {
                    break;
                }
            }
        });
        response.clear();
        upload.read_to_string(&mut response).unwrap();
        writer.join().unwrap();
        assert!(response.starts_with("HTTP/1.1 408"), "{response}");
        assert_eq!(uploads.available_permits(), 1);
        let mut inference = connect();
        inference
            .write_all(
                b"POST /v1/transcribe HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1\r\n\r\na",
            )
            .unwrap();
        let job = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(3), received.recv())
                .await
                .unwrap()
                .unwrap()
        });
        response.clear();
        inference.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 504"), "{response}");
        assert!(job.cancel.is_aborted());
        assert_eq!(uploads.available_permits(), 1);
        stop.send(()).unwrap();
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(3), server)
                .await
                .unwrap()
                .unwrap()
        });
    }

    /// Drive the real hyper server over a real socket. Exercises auth, routing,
    /// CORS, the Host (DNS-rebinding) guard, and the no-audio guard — none of
    /// which need a loaded model or the GUI.
    #[test]
    fn http_layer_end_to_end() {
        let engine = Arc::new(Mutex::new(None));
        let catalog = Arc::new(ModelCatalog::new());
        let port = 0;
        let token = "secrettoken123".to_string();
        let handle = start(engine, catalog, port, Some(token.clone())).expect("server should bind");
        let port = handle.port();

        // Use a minimal raw TCP client (below) for full control over headers.

        let (status, _h, body) = raw_get(port, "/v1/health", &[]);
        assert_eq!(status, 200, "health body: {body}");
        assert!(body.contains("\"status\":\"ok\""), "health body: {body}");

        // 2. Protected route without a token → 401.
        let (status, _h, _b) = raw_get(port, "/v1/models", &[]);
        assert_eq!(status, 401);

        // 3. Wrong token → 401.
        let (status, _h, _b) = raw_get(port, "/v1/models", &[("Authorization", "Bearer nope")]);
        assert_eq!(status, 401);

        // 4. Correct token → 200.
        let auth = format!("Bearer {token}");
        let (status, _h, body) = raw_get(port, "/v1/models", &[("Authorization", &auth)]);
        assert_eq!(status, 200, "models body: {body}");

        let (status, _h, _b) = raw_get(port, "/v1/nope", &[("Authorization", &auth)]);
        assert_eq!(status, 404);

        // 6. DNS-rebinding guard: a non-loopback Host is rejected with 400.
        let (status, _h, _b) = raw_request(
            port,
            "GET",
            "/v1/health",
            &[("Host", "evil.example.com")],
            b"",
        );
        assert_eq!(status, 400);

        // 7. CORS preflight reflects the Origin.
        let (status, headers, _b) = raw_request(
            port,
            "OPTIONS",
            "/v1/transcribe",
            &[("Origin", "http://localhost:3000")],
            b"",
        );
        assert_eq!(status, 204);
        assert!(
            headers
                .to_lowercase()
                .contains("access-control-allow-origin: http://localhost:3000"),
            "preflight headers: {headers}"
        );

        // 8. transcribe with a valid token but an empty body → 422 (no audio),
        //    proving auth passed and the body path ran without touching a model.
        let (status, _h, body) = raw_request(
            port,
            "POST",
            "/v1/transcribe",
            &[("Authorization", &auth)],
            b"",
        );
        assert_eq!(status, 422, "transcribe body: {body}");
        assert!(body.contains("no_audio"), "transcribe body: {body}");

        drop(handle); // signals shutdown
    }

    /// Minimal raw HTTP/1.1 client so the test fully controls headers (including
    /// Host) without depending on a higher-level client's behaviour.
    fn raw_get(port: u16, path: &str, headers: &[(&str, &str)]) -> (u16, String, String) {
        raw_request(port, "GET", path, headers, b"")
    }

    fn raw_request(
        port: u16,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> (u16, String, String) {
        let mut stream =
            std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect to API server");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut have_host = false;
        let mut req = format!("{method} {path} HTTP/1.1\r\n");
        for (k, v) in headers {
            if k.eq_ignore_ascii_case("host") {
                have_host = true;
            }
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        if !have_host {
            req.push_str(&format!("Host: 127.0.0.1:{port}\r\n"));
        }
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
        req.push_str("Connection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        stream.flush().unwrap();

        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        let text = String::from_utf8_lossy(&raw).to_string();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
        let status = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        (status, head.to_string(), body.to_string())
    }
}
