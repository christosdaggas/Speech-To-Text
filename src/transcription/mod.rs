// Copyright (C) 2026 Christos A. Daggas
// SPDX-License-Identifier: MIT

pub mod archive;
pub mod capabilities;
pub mod engine;
mod file_identity;
pub mod model;
pub mod parakeet;
mod parakeet_worker;
pub mod postprocess;
pub mod qwen;
pub mod safe_path;
pub mod sidecar_server;
pub mod summary;
pub mod vad;
pub mod verify;

pub use engine::TranscriptionEngine;
pub use model::{download_model, ModelCatalog};

/// Shared HTTP client for model/runtime downloads. It sets a connect timeout
/// and an idle read timeout so a stalled server can't hang a download forever,
/// but deliberately has **no** overall request timeout — model archives are
/// large and legitimately slow. Falls back to a default client if the builder
/// fails (should never happen with rustls compiled in).
pub fn download_client() -> reqwest::Client {
    use std::time::Duration;
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(120))
        // Providers legitimately redirect to CDNs, but never follow a
        // downgrade to cleartext (an on-path attacker could bounce a download
        // toward an internal or plaintext endpoint) and bound the chain.
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("too many redirects")
            } else if attempt.url().scheme() != "https" {
                attempt.error("redirect to a non-https target refused")
            } else {
                attempt.follow()
            }
        }))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Hard cap for provider metadata bodies (release listings, HF tree JSON).
/// Real listings are well under this; without a cap a compromised or on-path
/// server with a valid certificate could stream an unbounded JSON body into
/// memory before any hash verification runs.
pub(crate) const MAX_METADATA_BYTES: u64 = 8 * 1024 * 1024;

/// Read a JSON response body enforcing [`MAX_METADATA_BYTES`]. `None` on
/// oversize, transport error, or malformed JSON.
pub(crate) async fn read_json_capped(resp: reqwest::Response) -> Option<serde_json::Value> {
    use futures::StreamExt;
    if resp
        .content_length()
        .is_some_and(|l| l > MAX_METADATA_BYTES)
    {
        tracing::warn!("Metadata response advertises over {MAX_METADATA_BYTES} bytes; refusing");
        return None;
    }
    let mut bytes: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        if (bytes.len() + chunk.len()) as u64 > MAX_METADATA_BYTES {
            tracing::warn!("Metadata response exceeded {MAX_METADATA_BYTES} bytes; refusing");
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).ok()
}

/// Encode mono f32 PCM samples as a 16-bit WAV file in memory. Shared by the
/// Qwen requests and the legacy CLI regression baseline.
pub(crate) fn encode_wav_16bit(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let num_samples = samples.len();
    let data_size = (num_samples * 2) as u32;
    let file_size = 36 + data_size;

    let mut buf = Vec::with_capacity(file_size as usize + 8);

    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&file_size.to_le_bytes());
    buf.extend_from_slice(b"WAVE");

    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&1u16.to_le_bytes()); // mono
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    buf.extend_from_slice(&2u16.to_le_bytes()); // block align
    buf.extend_from_slice(&16u16.to_le_bytes()); // bits per sample

    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_size.to_le_bytes());

    for &sample in samples {
        let clamped = sample.clamp(-1.0, 1.0);
        let val = (clamped * 32767.0) as i16;
        buf.extend_from_slice(&val.to_le_bytes());
    }

    buf
}

/// Run a native sidecar with a hard deadline. `Command::output()` can otherwise
/// block the only inference worker forever when a downloaded runtime wedges.
pub(crate) fn run_command_with_timeout(
    command: &mut std::process::Command,
    timeout: std::time::Duration,
    cancel: Option<&crate::cancel::CancelToken>,
) -> crate::error::AppResult<std::process::Output> {
    if cancel.is_some_and(|t| t.is_aborted()) {
        return Err(crate::error::AppError::Cancelled);
    }
    let stdout_file = tempfile::NamedTempFile::new().map_err(|e| {
        crate::error::AppError::Transcription(format!("Failed to create sidecar output file: {e}"))
    })?;
    let stderr_file = tempfile::NamedTempFile::new().map_err(|e| {
        crate::error::AppError::Transcription(format!("Failed to create sidecar error file: {e}"))
    })?;
    command
        .stdout(std::process::Stdio::from(stdout_file.reopen()?))
        .stderr(std::process::Stdio::from(stderr_file.reopen()?));
    let mut child = command.spawn().map_err(|e| {
        crate::error::AppError::Transcription(format!("Failed to start sidecar: {e}"))
    })?;
    let started = std::time::Instant::now();
    loop {
        if cancel.is_some_and(|t| t.is_aborted()) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(crate::error::AppError::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                return Ok(std::process::Output {
                    status,
                    stdout: std::fs::read(stdout_file.path())?,
                    stderr: std::fs::read(stderr_file.path())?,
                });
            }
            Ok(None) if started.elapsed() < timeout => {
                // 10ms keeps the average exit-detection latency ~5ms; at 100ms
                // every sidecar transcription paid ~50ms of pure tail wait.
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(crate::error::AppError::DeadlineExceeded);
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(crate::error::AppError::Transcription(format!(
                    "Failed while waiting for sidecar: {e}"
                )));
            }
        }
    }
}

#[cfg(test)]
mod process_tests {
    use super::*;
    use crate::{cancel::CancelToken, error::AppError};
    use std::time::Duration;

    #[test]
    fn subprocess_is_killed_and_reaped_on_cancel_and_deadline() {
        for cancelled in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let pid_file = dir.path().join("pid");
            let mut cmd = std::process::Command::new("sh");
            cmd.args(["-c", "echo $$ > \"$1\"; exec sleep 30", "test"])
                .arg(&pid_file);
            let token = CancelToken::new();
            let abort = token.clone();
            let signal = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                if cancelled {
                    abort.abort();
                }
            });
            let result =
                run_command_with_timeout(&mut cmd, Duration::from_millis(300), Some(&token));
            signal.join().unwrap();
            if cancelled {
                assert!(matches!(result, Err(AppError::Cancelled)));
            } else {
                assert!(matches!(result, Err(AppError::DeadlineExceeded)));
            }
            let pid = std::fs::read_to_string(pid_file).unwrap();
            assert!(!std::path::Path::new(&format!("/proc/{}", pid.trim())).exists());
        }
    }
}
