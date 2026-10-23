// Copyright (C) 2026 Christos A. Daggas
// SPDX-License-Identifier: MIT

//! Trim only outer silence; removing interior silence would shift timestamps.
//! whisper_full_with_state() bypasses whisper.cpp's built-in VAD flag, so this
//! pre-pass is explicit. Model loading and inference share one serialized cache.

use super::file_identity::FileIdentity;
use crate::{
    cancel::CancelToken,
    error::{AppError, AppResult},
};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, TryLockError};
use tracing::{debug, warn};
use whisper_rs::{WhisperVadContext, WhisperVadContextParams, WhisperVadParams};

const SAMPLE_RATE: usize = 16_000;

/// Preserve soft onsets and trailing consonants outside Silero's segment padding.
const EDGE_PAD_SAMPLES: usize = SAMPLE_RATE / 2; // 500 ms

#[derive(Default)]
struct VadCache {
    loaded: Option<(FileIdentity, WhisperVadContext)>,
    #[cfg(test)]
    loads: usize,
}

static CACHE: Mutex<VadCache> = Mutex::new(VadCache {
    loaded: None,
    #[cfg(test)]
    loads: 0,
});

/// The bundled VAD model, or None when it isn't installed. Debug builds read
/// it from the source tree (same pattern as the translation catalogues);
/// release builds from the packaged data directory.
pub fn model_path() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = option_env!("VAD_DIR") {
        candidates.push(PathBuf::from(dir).join("ggml-silero-v5.1.2.bin"));
    }
    candidates.push(PathBuf::from(
        "/usr/share/speech-to-text/ggml-silero-v5.1.2.bin",
    ));
    candidates.into_iter().find(|p| p.exists())
}

/// Fail open to full-clip decoding when VAD is unavailable; only cancellation
/// propagates as an error. Speech ranges use half-open sample indices.
pub fn speech_span(audio: &[f32], cancel: Option<&CancelToken>) -> AppResult<SpeechSpan> {
    if cancel.is_some_and(CancelToken::is_aborted) {
        return Err(AppError::Cancelled);
    }
    if audio.is_empty() {
        return Ok(SpeechSpan::NoSpeech);
    }
    let mut cache = lock_cache(cancel)?;
    let Some(model) = model_path() else {
        cache.loaded = None;
        return Ok(SpeechSpan::Unavailable);
    };
    let span = cache.speech_span(&model, audio);
    if cancel.is_some_and(CancelToken::is_aborted) {
        return Err(AppError::Cancelled);
    }
    Ok(span)
}

fn lock_cache(cancel: Option<&CancelToken>) -> AppResult<MutexGuard<'static, VadCache>> {
    loop {
        if cancel.is_some_and(CancelToken::is_aborted) {
            return Err(AppError::Cancelled);
        }
        match CACHE.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(error)) => {
                let mut guard = error.into_inner();
                guard.loaded = None;
                CACHE.clear_poison();
                return Ok(guard);
            }
            Err(TryLockError::WouldBlock) => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
        }
    }
}

impl VadCache {
    fn speech_span(&mut self, model: &Path, audio: &[f32]) -> SpeechSpan {
        if audio.is_empty() {
            return SpeechSpan::NoSpeech;
        }
        if audio.len() > i32::MAX as usize || audio.iter().any(|s| !s.is_finite()) {
            return SpeechSpan::Unavailable;
        }
        let identity = match FileIdentity::read(model) {
            Ok(identity) => identity,
            Err(error) => {
                self.loaded = None;
                warn!("VAD model unavailable, skipping VAD: {error}");
                return SpeechSpan::Unavailable;
            }
        };
        if self.loaded.as_ref().is_none_or(|(old, _)| *old != identity) {
            self.loaded = None;
            let Some(path) = model.to_str().filter(|p| !p.contains('\0')) else {
                return SpeechSpan::Unavailable;
            };
            let mut params = WhisperVadContextParams::default();
            params.set_n_threads(4);
            params.set_use_gpu(false);
            match WhisperVadContext::new(path, params) {
                Ok(context) => {
                    self.loaded = Some((identity, context));
                    #[cfg(test)]
                    {
                        self.loads += 1;
                    }
                }
                Err(error) => {
                    warn!("VAD model failed to load, skipping VAD: {error}");
                    return SpeechSpan::Unavailable;
                }
            }
        }
        let Some((_, context)) = self.loaded.as_mut() else {
            return SpeechSpan::Unavailable;
        };
        // whisper_vad_detect_speech resets hidden/cell states and probabilities
        // on every call. The mutex also covers inference, not just model loading.
        let segments = match context.segments_from_samples(WhisperVadParams::new(), audio) {
            Ok(segments) => segments,
            Err(error) => {
                self.loaded = None;
                warn!("VAD run failed, discarding cached context: {error}");
                return SpeechSpan::Unavailable;
            }
        };
        span_from_segments(segments, audio.len())
    }
}

fn span_from_segments(
    segments: impl Iterator<Item = whisper_rs::WhisperVadSegment>,
    samples: usize,
) -> SpeechSpan {
    let mut first_cs = f32::MAX;
    let mut last_cs = 0.0f32;
    let mut count = 0usize;
    for seg in segments {
        first_cs = first_cs.min(seg.start);
        last_cs = last_cs.max(seg.end);
        count += 1;
    }
    if count == 0 {
        debug!("VAD: no speech detected in {samples} samples");
        return SpeechSpan::NoSpeech;
    }

    // Segment timestamps are in centiseconds.
    let to_sample = |cs: f32| ((cs / 100.0) * SAMPLE_RATE as f32) as usize;
    let start = to_sample(first_cs).saturating_sub(EDGE_PAD_SAMPLES);
    let end = to_sample(last_cs)
        .saturating_add(EDGE_PAD_SAMPLES)
        .min(samples);
    debug!(
        "VAD: {} speech segments, span {:.2}s–{:.2}s of {:.2}s",
        count,
        start as f32 / SAMPLE_RATE as f32,
        end as f32 / SAMPLE_RATE as f32,
        samples as f32 / SAMPLE_RATE as f32,
    );
    SpeechSpan::Speech { start, end }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SpeechSpan {
    /// Speech found: decode `audio[start..end]` and add the start offset back
    /// onto every segment timestamp.
    Speech { start: usize, end: usize },
    /// The whole clip is silence/noise — skip decoding entirely.
    NoSpeech,
    /// VAD could not run (model missing or failed) — decode everything.
    Unavailable,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data/vad/ggml-silero-v5.1.2.bin")
    }

    #[test]
    fn cached_context_matches_fresh_contexts_across_independent_clips() {
        let mut reused = VadCache::default();
        let clips = [
            vec![0.0; SAMPLE_RATE],
            (0..SAMPLE_RATE * 2)
                .map(|i| (i as f32 * 0.083).sin() * 0.3)
                .collect(),
            vec![0.0; SAMPLE_RATE / 2],
        ];
        for clip in &clips {
            let expected = VadCache::default().speech_span(&model(), clip);
            assert_ne!(expected, SpeechSpan::Unavailable);
            assert_eq!(reused.speech_span(&model(), clip), expected);
        }
        assert_eq!(reused.loads, 1);
    }

    #[test]
    fn replacement_missing_model_and_load_failure_do_not_reuse_stale_contexts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vad.bin");
        let silence = vec![0.0; SAMPLE_RATE / 2];
        std::fs::copy(model(), &path).unwrap();
        let mut cache = VadCache::default();
        assert_eq!(cache.speech_span(&path, &silence), SpeechSpan::NoSpeech);
        let replacement = dir.path().join("replacement");
        std::fs::copy(model(), &replacement).unwrap();
        std::fs::rename(replacement, &path).unwrap();
        assert_eq!(cache.speech_span(&path, &silence), SpeechSpan::NoSpeech);
        assert_eq!(cache.loads, 2);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(cache.speech_span(&path, &silence), SpeechSpan::Unavailable);
        assert!(cache.loaded.is_none());
        std::fs::write(&path, b"not a VAD model").unwrap();
        assert_eq!(cache.speech_span(&path, &silence), SpeechSpan::Unavailable);
        std::fs::copy(model(), &path).unwrap();
        assert_eq!(cache.speech_span(&path, &silence), SpeechSpan::NoSpeech);
        assert_eq!(cache.loads, 3);
    }

    #[test]
    fn empty_or_invalid_audio_does_not_load_a_model() {
        let mut cache = VadCache::default();
        assert_eq!(cache.speech_span(&model(), &[]), SpeechSpan::NoSpeech);
        assert_eq!(
            cache.speech_span(&model(), &[f32::NAN]),
            SpeechSpan::Unavailable
        );
        assert_eq!(cache.loads, 0);
    }

    #[test]
    fn cancelled_job_does_not_wait_for_another_clips_vad() {
        let _guard = CACHE.lock().unwrap();
        let token = CancelToken::new();
        let worker_token = token.clone();
        let worker = std::thread::spawn(move || {
            matches!(lock_cache(Some(&worker_token)), Err(AppError::Cancelled))
        });
        std::thread::sleep(std::time::Duration::from_millis(40));
        token.abort();
        assert!(worker.join().unwrap());
    }

    #[test]
    fn concurrent_clips_share_one_serialized_context() {
        let cache = std::sync::Arc::new(Mutex::new(VadCache::default()));
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let cache = cache.clone();
                std::thread::spawn(move || {
                    cache
                        .lock()
                        .unwrap()
                        .speech_span(&model(), &vec![0.0; SAMPLE_RATE / 2])
                })
            })
            .collect();
        for worker in workers {
            assert_eq!(worker.join().unwrap(), SpeechSpan::NoSpeech);
        }
        assert_eq!(cache.lock().unwrap().loads, 1);
    }
}
