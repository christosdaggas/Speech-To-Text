// Copyright (C) 2026 Christos A. Daggas
// SPDX-License-Identifier: MIT

//! Optional Parakeet TDT v3 INT8 using the pinned sherpa-onnx CPU runtime.
//! A private worker retains model weights between clips; cancellation kills
//! and reaps it. Each clip has a fresh decoder stream, with no Python or TCP.

use super::engine::{TranscriptionResult, TranscriptionSegment};
use crate::{
    cancel::CancelToken,
    config::AppConfig,
    error::{AppError, AppResult},
};
use futures::StreamExt;
#[cfg(test)]
use std::io::Write;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

const RUNTIME_ROOT: &str = "sherpa-onnx-v1.13.8-linux-x64-shared-no-tts";
const RUNTIME_URL: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/v1.13.8/sherpa-onnx-v1.13.8-linux-x64-shared-no-tts.tar.bz2";
const RUNTIME_SHA: &str = "d0f96c8b65c6cd0974fada22737e337de81bc8cd2abbec2e39caf358b1eec5fc";
const MODEL_REPO: &str = "csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8";
const MODEL_REV: &str = "2bda32ec70b097a55adaa07d9a7173915b43cc78";
static WORKER: super::parakeet_worker::WarmWorker = super::parakeet_worker::WarmWorker::new();
const FILES: &[(&str, u64, &str)] = &[
    (
        "encoder.int8.onnx",
        652184281,
        "acfc2b4456377e15d04f0243af540b7fe7c992f8d898d751cf134c3a55fd2247",
    ),
    (
        "decoder.int8.onnx",
        11845275,
        "179e50c43d1a9de79c8a24149a2f9bac6eb5981823f2a2ed88d655b24248db4e",
    ),
    (
        "joiner.int8.onnx",
        6355277,
        "3164c13fc2821009440d20fcb5fdc78bff28b4db2f8d0f0b329101719c0948b3",
    ),
    (
        "tokens.txt",
        93939,
        "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d",
    ),
];

fn runtime_dir() -> PathBuf {
    AppConfig::data_dir().join("parakeet-runtime")
}
fn model_dir() -> PathBuf {
    AppConfig::data_dir().join("parakeet-tdt-v3-int8")
}
fn binary() -> PathBuf {
    runtime_dir()
        .join(RUNTIME_ROOT)
        .join("bin/sherpa-onnx-offline")
}
pub fn is_runtime_installed() -> bool {
    let root = runtime_dir().join(RUNTIME_ROOT);
    binary().is_file()
        && root.join("lib/libsherpa-onnx-c-api.so").is_file()
        && root.join("lib/libonnxruntime.so").is_file()
}
pub fn is_model_downloaded() -> bool {
    let dir = model_dir();
    dir.join(".download_complete").is_file()
        && FILES.iter().all(|(name, size, _)| {
            dir.join(name)
                .metadata()
                .is_ok_and(|m| m.is_file() && m.len() == *size)
        })
}
pub fn parakeet_ready() -> bool {
    is_runtime_installed() && is_model_downloaded()
}
pub fn delete_runtime() -> AppResult<()> {
    shutdown();
    if runtime_dir().exists() {
        std::fs::remove_dir_all(runtime_dir())?;
    }
    Ok(())
}
pub fn delete_model() -> AppResult<()> {
    shutdown();
    if model_dir().exists() {
        std::fs::remove_dir_all(model_dir())?;
    }
    Ok(())
}

async fn download(
    url: &str,
    dest: &Path,
    sha: &str,
    progress: &impl Fn(u64, u64),
) -> AppResult<()> {
    let response = super::download_client()
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| AppError::ModelDownloadFailed(e.to_string()))?;
    let total = response.content_length().unwrap_or(0);
    let temporary = tempfile::NamedTempFile::new_in(dest.parent().unwrap_or(Path::new(".")))?;
    let mut file = tokio::fs::File::from_std(temporary.reopen()?);
    let mut stream = response.bytes_stream();
    let mut received = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| AppError::ModelDownloadFailed(e.to_string()))?;
        received += chunk.len() as u64;
        if received > crate::limits::MAX_DOWNLOAD_BYTES {
            return Err(AppError::ModelDownloadFailed(
                "Download size limit exceeded".into(),
            ));
        }
        file.write_all(&chunk).await?;
        progress(received, total);
    }
    file.flush().await?;
    drop(file);
    let path = temporary.path().to_path_buf();
    let expected = sha.to_owned();
    tokio::task::spawn_blocking(move || super::verify::verify_file(&path, &expected))
        .await
        .map_err(|e| AppError::ModelDownloadFailed(e.to_string()))??;
    temporary
        .persist(dest)
        .map_err(|e| AppError::ModelDownloadFailed(e.to_string()))?;
    Ok(())
}

pub async fn download_runtime(progress: impl Fn(u64, u64)) -> AppResult<()> {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return Err(AppError::ModelDownloadFailed(
            "Parakeet runtime requires Linux x86_64".into(),
        ));
    }
    let dir = runtime_dir();
    tokio::fs::create_dir_all(&dir).await?;
    let archive = dir.join("runtime.tar.bz2");
    download(RUNTIME_URL, &archive, RUNTIME_SHA, &progress).await?;
    tokio::task::spawn_blocking(move || -> AppResult<()> {
        // Fresh staging directory prevents writes through existing symlinks.
        let stage = tempfile::tempdir_in(&dir)?;
        super::archive::safe_extract_tar_bz2(&archive, stage.path())?;
        let root = stage.path().join(RUNTIME_ROOT);
        if !root.join("bin/sherpa-onnx-offline").is_file()
            || !root.join("lib/libsherpa-onnx-c-api.so").is_file()
            || !root.join("lib/libonnxruntime.so").is_file()
        {
            return Err(AppError::ModelDownloadFailed(
                "Runtime binary missing".into(),
            ));
        }
        let target = dir.join(RUNTIME_ROOT);
        shutdown();
        if target.exists() {
            std::fs::remove_dir_all(&target)?;
        }
        std::fs::rename(root, target)?;
        std::fs::remove_file(archive)?;
        Ok(())
    })
    .await
    .map_err(|e| AppError::ModelDownloadFailed(e.to_string()))?
}

pub async fn download_model(progress: impl Fn(u64, u64)) -> AppResult<()> {
    shutdown();
    let dir = model_dir();
    tokio::fs::create_dir_all(&dir).await?;
    let _ = tokio::fs::remove_file(dir.join(".download_complete")).await;
    let total: u64 = FILES.iter().map(|(_, size, _)| size).sum();
    let mut complete = 0;
    for &(name, size, sha) in FILES {
        let dest = dir.join(name);
        let existing = dest.clone();
        let valid = tokio::task::spawn_blocking(move || {
            super::verify::sha256_file(&existing).is_ok_and(|h| h == sha)
        })
        .await
        .map_err(|e| AppError::ModelDownloadFailed(e.to_string()))?;
        if !valid {
            download(
                &format!("https://huggingface.co/{MODEL_REPO}/resolve/{MODEL_REV}/{name}"),
                &dest,
                sha,
                &|dl, _| progress(complete + dl, total),
            )
            .await?;
        }
        complete += size;
        progress(complete, total);
    }
    tokio::fs::write(dir.join(".download_complete"), MODEL_REV).await?;
    Ok(())
}

pub fn transcribe(
    audio: &[f32],
    threads: u32,
    cancel: Option<CancelToken>,
) -> AppResult<TranscriptionResult> {
    if !parakeet_ready() {
        shutdown();
        return Err(AppError::Transcription(
            "Download the Parakeet runtime and model in Settings → Model.".into(),
        ));
    }
    let seconds = audio.len() as f64 / 16000.0;
    if cancel.as_ref().is_some_and(CancelToken::is_aborted) {
        return Err(AppError::Cancelled);
    }
    if audio.is_empty() {
        return parse_result(br#"{"text":""}"#, 0.0);
    }
    let spec = match super::parakeet_worker::WorkerSpec::new(
        &runtime_dir().join(RUNTIME_ROOT),
        &model_dir(),
        threads,
    ) {
        Ok(spec) => spec,
        Err(error) => {
            shutdown();
            return Err(error);
        }
    };
    let response = WORKER.transcribe(
        &spec,
        audio,
        cancel.as_ref(),
        std::time::Instant::now() + super::sidecar_server::request_deadline(seconds),
    )?;
    let result = parse_result(&response, seconds);
    if result.is_err() {
        shutdown();
    }
    result
}

pub fn shutdown() {
    WORKER.shutdown();
}

pub(crate) fn run_worker(arguments: &[std::ffi::OsString]) -> AppResult<()> {
    if arguments.len() != 3 {
        return Err(AppError::Transcription(
            "Invalid Parakeet worker arguments".into(),
        ));
    }
    let threads = arguments[2]
        .to_str()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| AppError::Transcription("Invalid Parakeet thread count".into()))?;
    super::parakeet_worker::run_worker(Path::new(&arguments[0]), Path::new(&arguments[1]), threads)
}

#[cfg(test)]
pub(super) fn decode(
    binary: &Path,
    models: &Path,
    audio: &[f32],
    threads: u32,
    cancel: Option<CancelToken>,
) -> AppResult<TranscriptionResult> {
    if audio.is_empty() {
        return parse_result(br#"{"text":""}"#, 0.0);
    }
    let mut wav = tempfile::Builder::new()
        .prefix("stt-parakeet-")
        .suffix(".wav")
        .tempfile()?;
    wav.write_all(&super::encode_wav_16bit(audio, 16000))?;
    wav.flush()?;
    let mut cmd = std::process::Command::new(binary);
    cmd.args([
        "--provider=cpu",
        "--model-type=nemo_transducer",
        "--decoding-method=greedy_search",
        "--print-args=false",
    ]);
    cmd.arg(format!("--num-threads={}", threads.clamp(1, 32)));
    for (flag, name) in [
        ("encoder", "encoder.int8.onnx"),
        ("decoder", "decoder.int8.onnx"),
        ("joiner", "joiner.int8.onnx"),
        ("tokens", "tokens.txt"),
    ] {
        cmd.arg(format!("--{flag}={}", models.join(name).display()));
    }
    cmd.arg(wav.path());
    let seconds = audio.len() as f64 / 16000.0;
    let output = super::run_command_with_timeout(
        &mut cmd,
        super::sidecar_server::request_deadline(seconds),
        cancel.as_ref(),
    )?;
    if !output.status.success() {
        return Err(AppError::Transcription(format!(
            "Parakeet exited with {}: {}",
            output.status,
            crate::error::redact_secrets(&String::from_utf8_lossy(&output.stderr))
                .chars()
                .take(1000)
                .collect::<String>()
        )));
    }
    parse_result(&output.stdout, seconds)
}

fn parse_result(bytes: &[u8], seconds: f64) -> AppResult<TranscriptionResult> {
    #[derive(serde::Deserialize)]
    struct ResultText {
        text: String,
    }
    let parsed: ResultText = serde_json::from_slice(bytes)
        .map_err(|e| AppError::Transcription(format!("Invalid Parakeet response: {e}")))?;
    let text = parsed.text.trim().to_owned();
    // Whole-clip timing is intentional; token times are not word/segment ends.
    let segments = if text.is_empty() {
        vec![]
    } else {
        vec![TranscriptionSegment {
            start_ms: Some(0),
            end_ms: Some((seconds * 1000.0) as i64),
            text: text.clone(),
            confidence: None,
        }]
    };
    Ok(TranscriptionResult {
        text,
        segments,
        average_confidence: None,
        detected_language: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_unicode_and_rejects_missing_text() {
        let r = parse_result("{\"text\":\" Γεια σου \"}".as_bytes(), 2.5).unwrap();
        assert_eq!(r.text, "Γεια σου");
        assert_eq!(r.segments[0].end_ms, Some(2500));
        assert!(parse_result(b"{}", 1.0).is_err());
        assert!(parse_result(b"log output", 1.0).is_err());
        assert!(parse_result(br#"{"text":""}"#, 1.0)
            .unwrap()
            .segments
            .is_empty());
    }
    #[test]
    #[ignore = "requires downloaded runtime/model and a public speech fixture; no network in test"]
    fn real_audio_smoke() {
        let root = PathBuf::from(std::env::var("PARAKEET_SMOKE_ROOT").expect("fixture root"));
        assert_eq!(
            super::super::verify::sha256_file(&root.join("runtime.tar.bz2")).unwrap(),
            RUNTIME_SHA
        );
        for &(name, _, sha) in FILES {
            assert_eq!(
                super::super::verify::sha256_file(&root.join("model").join(name)).unwrap(),
                sha
            );
        }
        let extracted = tempfile::tempdir().unwrap();
        super::super::archive::safe_extract_tar_bz2(
            &root.join("runtime.tar.bz2"),
            extracted.path(),
        )
        .unwrap();
        let audio =
            crate::audio::file_decoder::decode_audio_file(&root.join("speech.wav")).unwrap();
        let r = decode(
            &extracted
                .path()
                .join(RUNTIME_ROOT)
                .join("bin/sherpa-onnx-offline"),
            &root.join("model"),
            &audio.samples,
            4,
            None,
        )
        .unwrap();
        assert!(
            r.text.to_lowercase().contains("fellow americans"),
            "{}",
            r.text
        );
    }
}
