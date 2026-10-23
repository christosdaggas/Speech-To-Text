// Copyright (C) 2026 Christos A. Daggas
// SPDX-License-Identifier: MIT

//! Shared transcription result types and Whisper implementation.

use std::path::{Path, PathBuf};
use tracing::info;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::error::{AppError, AppResult};

/// A single transcription segment with optional timing and confidence.
#[derive(Debug, Clone)]
pub struct TranscriptionSegment {
    /// Start time in milliseconds (None if the backend doesn't provide timestamps).
    pub start_ms: Option<i64>,
    /// End time in milliseconds (None if the backend doesn't provide timestamps).
    pub end_ms: Option<i64>,
    pub text: String,
    /// Confidence probability (0.0 - 1.0), None if unavailable.
    pub confidence: Option<f32>,
}

#[derive(Debug, Clone)]
pub struct TranscriptionResult {
    pub segments: Vec<TranscriptionSegment>,
    pub text: String,
    /// Average confidence across all segments (None if unavailable).
    pub average_confidence: Option<f32>,
    /// Detected language (if auto-detect was used).
    pub detected_language: Option<String>,
}

pub struct TranscriptionEngine {
    ctx: WhisperContext,
    model_id: String,
    model_path: PathBuf,
    /// Whether this context was created with GPU acceleration. Used to decide
    /// whether a transcription failure is worth retrying on CPU.
    use_gpu: bool,
    pub requested_gpu: bool,
}

/// `user_data` must be null or point to a live, shared CancelToken for the
/// complete synchronous decode (including its native worker threads).
unsafe extern "C" fn whisper_should_abort(user_data: *mut std::ffi::c_void) -> bool {
    if user_data.is_null() {
        return false;
    }
    // SAFETY: the caller lends a correctly aligned CancelToken until the native
    // state is dropped. Only shared reads/atomic loads are performed, including
    // when cancellation is requested from another thread.
    let token = unsafe { &*user_data.cast::<crate::cancel::CancelToken>() };
    token.is_aborted()
}

unsafe extern "C" fn whisper_should_encode(
    _context: *mut whisper_rs_sys::whisper_context,
    _state: *mut whisper_rs_sys::whisper_state,
    user_data: *mut std::ffi::c_void,
) -> bool {
    // SAFETY: the same borrowed token/lifetime contract as whisper_should_abort;
    // context and state are deliberately not accessed. This hook returns true to proceed.
    !unsafe { whisper_should_abort(user_data) }
}

impl TranscriptionEngine {
    pub fn load_model(model_path: &Path, model_id: &str) -> AppResult<Self> {
        Self::load_model_with_gpu(model_path, model_id, false)
    }

    /// Load a Whisper model with optional GPU acceleration.
    pub fn load_model_with_gpu(
        model_path: &Path,
        model_id: &str,
        use_gpu: bool,
    ) -> AppResult<Self> {
        if use_gpu && !cfg!(any(feature = "vulkan", feature = "cuda", feature = "metal")) {
            return Err(AppError::ModelLoadFailed(
                "This build has no GPU backend".into(),
            ));
        }
        info!(
            "Loading Whisper model from {:?} (GPU: {})",
            model_path, use_gpu
        );

        if !model_path.exists() {
            return Err(AppError::ModelNotFound(format!(
                "Model file not found: {:?}",
                model_path
            )));
        }

        let mut params = WhisperContextParameters::default();
        params.use_gpu(use_gpu);
        // Flash attention: measured 20–25% faster decoding on the Vulkan
        // backend (ggml with KHR_coopmat), identical output. GPU-only — on
        // CPU the fused kernel brings nothing. DTW token timestamps (which it
        // would disable) are not used anywhere in this app.
        params.flash_attn(use_gpu);

        let ctx = WhisperContext::new_with_params(
            model_path
                .to_str()
                .ok_or_else(|| AppError::ModelLoadFailed("Invalid model path encoding".into()))?,
            params,
        )
        .map_err(|e| AppError::ModelLoadFailed(format!("Failed to load model: {}", e)))?;

        info!(
            "Whisper model '{}' loaded successfully (GPU: {})",
            model_id, use_gpu
        );

        Ok(Self {
            ctx,
            model_id: model_id.to_string(),
            model_path: model_path.to_path_buf(),
            use_gpu,
            requested_gpu: use_gpu,
        })
    }

    /// Whether this context uses GPU acceleration (worth a CPU retry on failure).
    pub fn uses_gpu(&self) -> bool {
        self.use_gpu
    }

    /// The model file this engine was loaded from (for a CPU reload on failure).
    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    /// Transcribe audio data.
    ///
    /// `audio` must be mono 16kHz f32 PCM.
    /// `language` is an ISO 639-1 code (e.g., "en", "el") or None for auto-detect.
    /// `n_threads` is the number of CPU threads to use.
    /// `translate` if true, translates the output to English.
    /// `beam_size` controls beam search width (1 = greedy).
    /// `temperature` controls sampling randomness (0.0 = deterministic).
    /// `initial_prompt` optional prompt with domain-specific vocabulary.
    #[allow(clippy::too_many_arguments)]
    pub fn transcribe(
        &self,
        audio: &[f32],
        language: Option<&str>,
        n_threads: u32,
        translate: bool,
        beam_size: u32,
        temperature: f32,
        initial_prompt: Option<&str>,
        cancel: Option<crate::cancel::CancelToken>,
    ) -> AppResult<TranscriptionResult> {
        if cancel.as_ref().is_some_and(|t| t.is_aborted()) {
            return Err(AppError::Cancelled);
        }
        if audio.is_empty() {
            return Ok(TranscriptionResult {
                segments: Vec::new(),
                text: String::new(),
                average_confidence: None,
                detected_language: None,
            });
        }

        info!(
            "Transcribing {:.1}s of audio with model '{}' ({} threads)",
            audio.len() as f32 / 16000.0,
            self.model_id,
            n_threads
        );

        // VAD pre-pass: skip silent clips outright and trim leading/trailing
        // silence, so the decoder never manufactures text out of dead air.
        // Interior pauses stay (cutting them would corrupt the timestamps).
        let (audio, offset_ms) = match super::vad::speech_span(audio, cancel.as_ref())? {
            super::vad::SpeechSpan::NoSpeech => {
                info!("VAD found no speech — skipping decode");
                return Ok(TranscriptionResult {
                    segments: Vec::new(),
                    text: String::new(),
                    average_confidence: None,
                    detected_language: None,
                });
            }
            super::vad::SpeechSpan::Speech { start, end } => {
                (&audio[start..end], (start as i64 * 1000) / 16_000)
            }
            super::vad::SpeechSpan::Unavailable => (audio, 0),
        };

        let beam_size = beam_size.min(8); // whisper.cpp supports max 8 decoders
        let mut params = if beam_size > 1 {
            FullParams::new(SamplingStrategy::BeamSearch {
                beam_size: beam_size as i32,
                patience: -1.0,
            })
        } else {
            FullParams::new(SamplingStrategy::Greedy { best_of: 1 })
        };

        params.set_n_threads(n_threads as i32);
        params.set_translate(translate);
        params.set_no_context(true);
        params.set_no_timestamps(false);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        // Token-level timestamps are never consumed (only segment t0/t1 and
        // decoder-provided token probabilities are read), so skip whisper.cpp's
        // per-token timestamp pass — pure decode-time savings.
        params.set_token_timestamps(false);
        params.set_suppress_blank(true);
        params.set_suppress_nst(true);
        params.set_temperature(temperature);
        // Allow internal temperature retries when a decoded segment trips the
        // entropy/logprob thresholds. Encoder failures (-6) are separate.
        params.set_temperature_inc(0.2);
        params.set_entropy_thold(2.2);
        params.set_logprob_thold(-0.8);

        if let Some(lang) = language {
            params.set_language(Some(lang));
        } else {
            // Auto-detect the language AND transcribe. Note: `set_detect_language(true)`
            // makes whisper.cpp detect the language and then STOP without
            // transcribing (0 segments), so we must pass the special "auto"
            // language instead. The detected language is still read back below via
            // `full_lang_id_from_state()`.
            params.set_language(Some("auto"));
        }

        if let Some(prompt) = initial_prompt {
            params.set_initial_prompt(prompt);
        }

        let mut state = self
            .ctx
            .create_state()
            .map_err(|e| AppError::Transcription(format!("Failed to create state: {}", e)))?;

        if let Some(token) = cancel.as_ref() {
            // whisper-rs 0.16.0's "safe" setter boxes a trait object but casts
            // its address back to the concrete closure type in the trampoline.
            // That layout mismatch causes false aborts (-6) and invalid reads.
            // SAFETY: token is borrowed from this function's owned `cancel`,
            // which is neither moved nor dropped until after `state`. `full`
            // is synchronous; its worker threads finish before it returns.
            // The callback only reads this Sync token and does not unwind or
            // touch the Whisper context. No callback allocation is leaked.
            unsafe {
                params.set_abort_callback(Some(whisper_should_abort));
                params.set_abort_callback_user_data(std::ptr::from_ref(token).cast_mut().cast());
                // Check before auto-detection/encoding too: the native abort
                // callback may otherwise run only after an expensive CPU graph.
                params.set_start_encoder_callback(Some(whisper_should_encode));
                params.set_start_encoder_callback_user_data(
                    std::ptr::from_ref(token).cast_mut().cast(),
                );
            }
        }
        let decoded = state.full(params, audio);
        if cancel.as_ref().is_some_and(|t| t.is_aborted()) {
            return Err(AppError::Cancelled);
        }
        decoded.map_err(|e| AppError::Transcription(format!("Transcription failed: {}", e)))?;

        // When auto-detecting, surface the language Whisper picked (ISO 639-1 code).
        let detected_language = if language.is_none() {
            whisper_rs::get_lang_str(state.full_lang_id_from_state()).map(|s| s.to_string())
        } else {
            None
        };

        // Collect segments. Timestamps are shifted back by the VAD trim
        // offset so they stay relative to the original, untrimmed audio.
        let num_segments = state.full_n_segments();

        let mut segments = Vec::with_capacity(num_segments as usize);
        let mut full_text = String::new();
        let mut total_confidence = 0.0f32;

        for i in 0..num_segments {
            let segment = state
                .get_segment(i)
                .ok_or_else(|| AppError::Transcription(format!("Failed to get segment {}", i)))?;
            // whisper timestamps are in centiseconds
            let start_ms = segment.start_timestamp() * 10 + offset_ms;
            let end_ms = segment.end_timestamp() * 10 + offset_ms;
            let text = segment
                .to_str()
                .map_err(|e| AppError::Transcription(format!("Failed to get segment text: {}", e)))?
                .to_string();

            // Calculate average token probability for this segment
            let n_tokens = segment.n_tokens();
            let confidence = if n_tokens > 0 {
                let mut sum = 0.0f32;
                for t in 0..n_tokens {
                    sum += segment
                        .get_token(t)
                        .map(|tok| tok.token_probability())
                        .unwrap_or(0.0);
                }
                sum / n_tokens as f32
            } else {
                0.0
            };

            full_text.push_str(&text);
            total_confidence += confidence;

            segments.push(TranscriptionSegment {
                start_ms: Some(start_ms),
                end_ms: Some(end_ms),
                text,
                confidence: Some(confidence),
            });
        }

        let average_confidence = if segments.is_empty() {
            None
        } else {
            Some(total_confidence / segments.len() as f32)
        };

        info!(
            "Transcription complete: {} segments, avg confidence {:.1}%",
            segments.len(),
            average_confidence.unwrap_or(0.0) * 100.0
        );

        Ok(TranscriptionResult {
            segments,
            text: full_text.trim().to_string(),
            average_confidence,
            detected_language,
        })
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_engine_matches_the_linked_native_version() {
        assert_eq!(whisper_rs::WHISPER_CPP_VERSION, "1.9.5");
        assert_eq!(whisper_rs::get_whisper_version(), "1.9.5");
    }

    #[test]
    fn native_abort_callback_observes_only_requested_cancellation() {
        let parent = crate::cancel::CancelToken::new();
        let token = parent.child();
        let pointer = std::ptr::from_ref(&token).cast_mut().cast();
        // SAFETY: token stays alive and stationary throughout these calls.
        unsafe {
            assert!(!whisper_should_abort(std::ptr::null_mut()));
            assert!(!whisper_should_abort(pointer));
            assert!(whisper_should_encode(
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                pointer
            ));
            let request = token.clone();
            std::thread::spawn(move || request.abort()).join().unwrap();
            assert!(whisper_should_abort(pointer));
            assert!(!whisper_should_encode(
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                pointer
            ));
        }
        let sibling = parent.child();
        let pointer = std::ptr::from_ref(&sibling).cast_mut().cast();
        // SAFETY: sibling stays alive and stationary throughout these calls.
        unsafe {
            assert!(!whisper_should_abort(pointer));
            parent.abort();
            assert!(whisper_should_abort(pointer));
        }
    }

    /// Local regression probe: no downloads or user recordings. Supply an
    /// installed model and a generated speech WAV (see benchmarks/README.md).
    #[test]
    #[ignore = "requires STT_TEST_MODEL and STT_TEST_WAV; optional STT_TEST_GPU=1"]
    fn real_audio_with_cancellation_token() {
        let model = std::env::var("STT_TEST_MODEL").expect("STT_TEST_MODEL");
        let wav = std::env::var("STT_TEST_WAV").expect("STT_TEST_WAV");
        let gpu = std::env::var("STT_TEST_GPU").as_deref() == Ok("1");
        let audio = crate::audio::file_decoder::decode_audio_file(Path::new(&wav)).unwrap();
        let engine =
            TranscriptionEngine::load_model_with_gpu(Path::new(&model), "regression", gpu).unwrap();
        // The control proves the fixture/model works without a callback.
        let control = engine
            .transcribe(&audio.samples, None, 4, false, 2, 0.0, None, None)
            .unwrap();
        assert!(!control.text.trim().is_empty());
        for _ in 0..2 {
            let token = crate::cancel::CancelToken::new();
            let result = engine
                .transcribe(
                    &audio.samples,
                    None,
                    4,
                    false,
                    2,
                    0.0,
                    None,
                    Some(token.clone()),
                )
                .unwrap();
            assert!(!token.is_aborted());
            assert!(!result.text.trim().is_empty());
            token.abort();
            assert!(matches!(
                engine.transcribe(&audio.samples, None, 4, false, 2, 0.0, None, Some(token)),
                Err(AppError::Cancelled)
            ));
        }
        let token = crate::cancel::CancelToken::new();
        let abort = token.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            abort.abort();
        });
        let started = std::time::Instant::now();
        let result = engine.transcribe(
            &audio.samples.repeat(10),
            None,
            4,
            false,
            2,
            0.0,
            None,
            Some(token),
        );
        canceller.join().unwrap();
        assert!(matches!(result, Err(AppError::Cancelled)));
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }
}
