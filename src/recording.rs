// Copyright (C) 2026 Christos A. Daggas
// SPDX-License-Identifier: MIT

//! Shared, UI-agnostic recording + transcription controller.
//!
//! Owns the single [`AudioCapture`] (one cpal stream) and the single Whisper
//! [`TranscriptionEngine`], so the main window, the mini panel, and the global
//! dictation shortcut all drive the *same* capture and engine instead of
//! fighting over duplicate instances. Capture start/stop must run on the glib
//! main thread (the cpal `Stream` is `!Send`); only transcription is offloaded
//! to a worker thread, exactly as the main window already did.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use crate::audio::buffer::RawAudioSnapshot;
use crate::audio::capture::RecordingState;
use crate::audio::AudioCapture;
use crate::error::AppResult;
use crate::transcription::engine::TranscriptionResult;
use crate::transcription::{postprocess, ModelCatalog, TranscriptionEngine};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingOwner {
    None,
    Main,
    Mini,
    /// A nested short capture for the "Voice edit" feature (spoken instruction).
    VoiceEdit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DictationMode {
    #[default]
    Plain,
    Message,
    Email,
    Note,
    CodePrompt,
}

impl DictationMode {
    pub fn from_config_str(s: &str) -> Self {
        match s {
            "message" => Self::Message,
            "email" => Self::Email,
            "note" => Self::Note,
            "code_prompt" => Self::CodePrompt,
            _ => Self::Plain,
        }
    }

    pub fn as_config_str(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Message => "message",
            Self::Email => "email",
            Self::Note => "note",
            Self::CodePrompt => "code_prompt",
        }
    }
}

/// Apply mode-specific formatting to already-sanitized transcript text.
///
/// Deterministic (no external LLM): each mode reshapes the cleaned transcript
/// for a different destination.
pub fn apply_mode(text: &str, mode: DictationMode) -> String {
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    match mode {
        // Clean prose, exactly as sanitized.
        DictationMode::Plain => text.to_string(),
        DictationMode::Message => ensure_terminal_punctuation(&collapse_whitespace(text)),
        DictationMode::Email => format_email(text),
        DictationMode::Note => format_note(text),
        // Concise instruction: drop speech fillers, single paragraph, punctuated.
        DictationMode::CodePrompt => ensure_terminal_punctuation(&capitalize_first(
            &strip_fillers(&collapse_whitespace(text)),
        )),
    }
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn ensure_terminal_punctuation(text: &str) -> String {
    let t = text.trim_end();
    if t.is_empty() {
        return String::new();
    }
    match t.chars().last() {
        Some('.') | Some('!') | Some('?') | Some(':') | Some(';') | Some('…') => t.to_string(),
        _ => format!("{t}."),
    }
}

fn capitalize_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn split_sentences(text: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        current.push(ch);
        if matches!(ch, '.' | '!' | '?' | '…') {
            let s = current.trim().to_string();
            if !s.is_empty() {
                sentences.push(s);
            }
            current.clear();
        }
    }
    let rest = current.trim();
    if !rest.is_empty() {
        sentences.push(rest.to_string());
    }
    sentences
}

fn format_note(text: &str) -> String {
    let sentences = split_sentences(&collapse_whitespace(text));
    if sentences.is_empty() {
        return String::new();
    }
    sentences
        .iter()
        .map(|s| format!("• {s}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_email(text: &str) -> String {
    let body = collapse_whitespace(text);
    format!("Hi,\n\n{body}\n\nBest regards,")
}

const FILLERS: &[&str] = &["um", "umm", "uh", "uhh", "er", "erm", "ah", "hmm", "mhm"];

/// Drop standalone filler words (case-insensitive), preserving everything else.
fn strip_fillers(text: &str) -> String {
    let kept: Vec<&str> = text
        .split_whitespace()
        .filter(|w| {
            let core = w
                .trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase();
            !FILLERS.contains(&core.as_str())
        })
        .collect();
    collapse_whitespace(&kept.join(" "))
}

/// A snapshot of the inference parameters for one dictation run. Deliberately
/// widget-free: the main window builds this from its settings widgets, while
/// the global-shortcut path builds it from `AppConfig` (the main window may be
/// closed).
#[derive(Debug, Clone)]
pub struct DictationParams {
    pub model_config: Arc<crate::config::AppConfig>,
    pub backend: String,
    pub language_code: Option<String>,
    pub n_threads: u32,
    pub beam_size: u32,
    pub temperature: f32,
    pub translate: bool,
    pub initial_prompt: Option<String>,
    pub selected_microphone: Option<String>,
    pub mode: DictationMode,
    /// Personal-dictionary replacement rules applied after sanitization.
    pub replacements: Vec<crate::config::DictReplacement>,
}

impl DictationParams {
    /// Build a parameter snapshot from configuration alone (no widgets). Used by
    /// the global dictation path, which may run with the main window closed.
    pub fn from_config(config: &crate::config::AppConfig) -> Self {
        let language_code = if config.auto_detect_language || config.backend == "parakeet" {
            None
        } else {
            config.language.clone()
        };
        Self {
            backend: config.backend.clone(),
            model_config: Arc::new(config.clone()),
            language_code,
            n_threads: config.effective_threads(),
            beam_size: config.beam_size,
            temperature: config.temperature,
            translate: config.translate_to_english,
            initial_prompt: config.effective_initial_prompt(),
            selected_microphone: config.selected_microphone.clone(),
            mode: DictationMode::from_config_str(&config.dictation_mode),
            replacements: if config.dictionary_enabled {
                config.dictionary_replacements.clone()
            } else {
                Vec::new()
            },
        }
    }
}

/// The result of a completed transcription, already sanitized + mode-formatted.
#[derive(Debug, Clone)]
pub struct DictationOutcome {
    pub id: String,
    pub diagnostics: crate::diagnostics::RunDiagnostics,
    /// A recovered prefix, never a complete transcript eligible for auto-paste.
    pub incomplete: Option<String>,
    /// Raw text straight from the engine (before sanitize/mode).
    pub raw_text: String,
    /// Sanitized + mode-formatted text — what should be shown/pasted/copied.
    pub cleaned_text: String,
    pub confidence: f32,
    pub confidence_available: bool,
    /// (start_ms, end_ms, text) segments for SRT export.
    pub segments: Vec<(i64, i64, String)>,
    /// Language Whisper picked, when auto-detect was used.
    pub detected_language: Option<String>,
    /// Recording/clip length in seconds (mic time, or decoded length for files).
    /// Used for words-per-minute stats and history.
    pub duration_secs: f32,
}

impl DictationOutcome {
    pub fn shift_timestamps(&mut self, leading_offset_ms: f64) {
        let shift = leading_offset_ms.round() as i64;
        for segment in &mut self.segments {
            segment.0 += shift;
            segment.1 += shift;
        }
    }
}

/// Run the transcription core synchronously. Intended to be called from a
/// worker thread (it blocks on the engine). Handles both the Whisper and
/// sidecar backends and applies sanitize + mode formatting. Cancellation reaches
/// subprocesses, warm-server requests and Whisper's cooperative abort callback.
pub fn run_transcription(
    engine: &SharedEngine,
    audio: &[f32],
    params: &DictationParams,
    duration_secs: f32,
    cancel: Option<crate::cancel::CancelToken>,
) -> Result<DictationOutcome, String> {
    let started = std::time::Instant::now();
    let mut outcome = run_transcription_inner(engine, audio, params, duration_secs, cancel)?;
    outcome.diagnostics.inference_ms =
        (crate::diagnostics::milliseconds(started) - outcome.diagnostics.model_load_ms).max(0.0);
    crate::diagnostics::record(&outcome.diagnostics);
    Ok(outcome)
}

fn run_transcription_inner(
    engine: &SharedEngine,
    audio: &[f32],
    params: &DictationParams,
    duration_secs: f32,
    cancel: Option<crate::cancel::CancelToken>,
) -> Result<DictationOutcome, String> {
    if cancel.as_ref().is_some_and(|t| t.is_aborted()) {
        return Err(crate::error::AppError::Cancelled.to_string());
    }
    if params.backend == "parakeet" {
        if !crate::transcription::parakeet::parakeet_ready() {
            return Err(
                "Parakeet is not set up. Go to Settings → Model to download the runtime and model."
                    .to_string(),
            );
        }
        match crate::transcription::parakeet::transcribe(audio, params.n_threads, cancel) {
            Ok(r) => Ok(build_outcome(r, params, duration_secs)),
            Err(e) => Err(format!("{}", e)),
        }
    } else if params.backend == "qwen" {
        if !crate::transcription::qwen::qwen_ready() {
            return Err("Qwen3-ASR is not set up. Go to Settings → Model to download the runtime and model.".to_string());
        }
        match crate::transcription::qwen::transcribe(
            audio,
            params.language_code.as_deref(),
            &params.model_config.qwen_model_size,
            cancel,
        ) {
            Ok(r) => Ok(build_outcome(r, params, duration_secs)),
            Err(e) => Err(format!("{}", e)),
        }
    } else {
        // Take a handle to the loaded engine and RELEASE the mutex before
        // decoding: the GTK thread locks this same mutex to switch backends,
        // delete models and swap freshly-loaded engines, and a decode can hold
        // it for minutes. Concurrent decodes are safe — each transcribe call
        // creates its own whisper state over the shared immutable context. A
        // poisoned lock is recovered with into_inner (the slot is a plain
        // replaceable Option) instead of permanently failing all transcription.
        let load_start = std::time::Instant::now();
        let eng = load_engine(engine, &params.model_config, cancel.as_ref())?;
        let load_ms = crate::diagnostics::milliseconds(load_start);
        if cancel.as_ref().is_some_and(|t| t.is_aborted()) {
            return Err(crate::error::AppError::Cancelled.to_string());
        }
        // First attempt on the loaded engine (GPU if it was loaded with it).
        let gpu_reload = eng
            .uses_gpu()
            .then(|| (eng.model_path().to_path_buf(), eng.model_id().to_string()));

        let run = |model: &TranscriptionEngine| {
            model
                .transcribe(
                    audio,
                    params.language_code.as_deref(),
                    params.n_threads,
                    params.translate,
                    params.beam_size,
                    params.temperature,
                    params.initial_prompt.as_deref(),
                    cancel.clone(),
                )
                .map(|result| {
                    let mut outcome = build_outcome(result, params, duration_secs);
                    outcome.diagnostics.model = model.model_id().to_string();
                    outcome.diagnostics.compute = if model.uses_gpu() {
                        "GPU-enabled context"
                    } else {
                        "CPU"
                    }
                    .into();
                    outcome.diagnostics.model_load_ms = load_ms;
                    outcome
                })
        };

        match run(eng.as_ref()) {
            Ok(outcome) => Ok(outcome),
            Err(crate::error::AppError::Cancelled) => {
                Err(crate::error::AppError::Cancelled.to_string())
            }
            Err(first_err) => {
                // A GPU encode can fail transiently on some drivers (whisper
                // returns "failed to encode" / -6), especially under GPU-memory
                // pressure. One failure is not proof that the GPU is unusable:
                // retry the same audio once (a fresh decode state over the same
                // context) before paying the CPU fallback cost and loading a
                // second model.
                let Some((model_path, model_id)) = gpu_reload else {
                    return Err(format!("Transcription failed: {}", first_err));
                };
                tracing::warn!("GPU transcription failed ({first_err}); retrying once on GPU");
                match run(eng.as_ref()) {
                    Ok(outcome) => Ok(outcome),
                    Err(crate::error::AppError::Cancelled) => {
                        Err(crate::error::AppError::Cancelled.to_string())
                    }
                    Err(second_err) => {
                        if !params.model_config.cpu_fallback {
                            return Err(format!("GPU transcription failed: {second_err}"));
                        }
                        if cancel.as_ref().is_some_and(|token| token.is_aborted()) {
                            return Err(crate::error::AppError::Cancelled.to_string());
                        }
                        // Two consecutive failures: treat the GPU path as
                        // unusable for this session. Reload on CPU once, swap
                        // it into the shared slot so later transcriptions skip
                        // the doomed GPU path, and retry — the user gets a
                        // transcript instead of an error. Non-GPU failures are
                        // real, so surface them.
                        tracing::warn!(
                            "GPU transcription failed twice ({second_err}); reloading \
                             '{model_id}' on CPU and retrying"
                        );
                        match TranscriptionEngine::load_model_with_gpu(
                            &model_path,
                            &model_id,
                            false,
                        ) {
                            Ok(mut cpu_eng) => {
                                cpu_eng.requested_gpu = eng.requested_gpu;
                                let cpu_eng = Arc::new(cpu_eng);
                                let retry = run(cpu_eng.as_ref());
                                // Swap the CPU engine into the shared slot so
                                // later transcriptions skip the doomed GPU path
                                // — but ONLY if the slot still holds the engine
                                // this decode started from. During the (long)
                                // failed decode + reload, the GTK thread may
                                // have switched models, switched backends
                                // (slot = None to free memory), or deleted the
                                // model; clobbering that would silently
                                // resurrect a multi-GB engine the user just
                                // discarded.
                                crate::config::publish_for_selection(&params.model_config, || {
                                    let mut guard =
                                        engine.lock().unwrap_or_else(|e| e.into_inner());
                                    if guard.as_ref().is_some_and(|cur| Arc::ptr_eq(cur, &eng)) {
                                        *guard = Some(cpu_eng);
                                    }
                                });
                                retry.map_err(|e| format!("Transcription failed: {}", e))
                            }
                            Err(e) => Err(format!("Transcription failed (GPU then CPU): {}", e)),
                        }
                    }
                }
            }
        }
    }
}

/// Ensure a Whisper engine is loaded in the shared slot, loading the configured
/// model on first use. No-op if an engine is already loaded — so the API server
/// and the GUI share the one instance (whoever loads first wins). Honors
/// `config.use_gpu` with a CPU fallback when `cpu_fallback` is set, mirroring the
/// main window's loader. Blocking: call from a worker thread, not the UI thread.
///
/// Only relevant for the Whisper backend; Parakeet/Qwen run via sidecars and
/// need no preloaded engine.
pub fn ensure_engine_loaded(
    engine: &SharedEngine,
    config: &crate::config::AppConfig,
) -> Result<Arc<TranscriptionEngine>, String> {
    load_engine(engine, config, None)
}

fn load_engine(
    engine: &SharedEngine,
    config: &crate::config::AppConfig,
    cancel: Option<&crate::cancel::CancelToken>,
) -> Result<Arc<TranscriptionEngine>, String> {
    static LOAD_LOCK: Mutex<()> = Mutex::new(());
    let checkpoint = || {
        if cancel.is_some_and(|token| token.is_aborted()) {
            Err(crate::error::AppError::Cancelled.to_string())
        } else {
            Ok(())
        }
    };
    let _loading = loop {
        checkpoint()?;
        match LOAD_LOCK.try_lock() {
            Ok(guard) => break guard,
            Err(std::sync::TryLockError::Poisoned(error)) => break error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
        }
    };
    if let Some(loaded) = engine
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .filter(|loaded| {
            loaded.model_id() == config.selected_model
                && loaded.requested_gpu == config.use_gpu
                && (loaded.uses_gpu() || !config.use_gpu || config.cpu_fallback)
        })
        .cloned()
    {
        return Ok(loaded);
    }

    // Resolve stale base IDs (e.g. "tiny" stored while only "tiny-q5_1" is on
    // disk) the way MainWindow::load_selected_model does, so window-less
    // sessions with older configs still load the intended model.
    let mut model_id = config.selected_model.clone();
    if !ModelCatalog::is_downloaded(&model_id) {
        let base = ModelCatalog::base_model_id(&model_id).to_string();
        let resolved = ModelCatalog::resolve_model(&base, config.use_quantized);
        if ModelCatalog::is_downloaded(&resolved) {
            tracing::info!(
                "Selected model '{}' missing; loading downloaded variant '{}'",
                model_id,
                resolved
            );
            model_id = resolved;
        } else {
            return Err(format!(
                "Model '{}' is not downloaded. Open the app and download it in Settings → Model.",
                model_id
            ));
        }
    }

    // The resolved variant may itself already be loaded.
    if let Some(loaded) = engine
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .filter(|loaded| {
            loaded.model_id() == model_id
                && loaded.requested_gpu == config.use_gpu
                && (loaded.uses_gpu() || !config.use_gpu || config.cpu_fallback)
        })
        .cloned()
    {
        return Ok(loaded);
    }

    let model_path = ModelCatalog::model_path(&model_id);
    checkpoint()?;

    let mut loaded =
        match TranscriptionEngine::load_model_with_gpu(&model_path, &model_id, config.use_gpu) {
            Ok(e) => e,
            Err(e) if config.use_gpu && config.cpu_fallback => {
                checkpoint()?;
                tracing::warn!("API: GPU model load failed ({e}); retrying on CPU");
                TranscriptionEngine::load_model_with_gpu(&model_path, &model_id, false)
                    .map_err(|e2| format!("Failed to load model on GPU and CPU: {e2}"))?
            }
            Err(e) => return Err(format!("Failed to load model: {e}")),
        };

    // Re-check under the lock: another thread may have loaded it meanwhile.
    loaded.requested_gpu = config.use_gpu;
    checkpoint()?;
    let loaded = Arc::new(loaded);
    crate::config::publish_for_selection(config, || {
        *engine.lock().unwrap_or_else(|e| e.into_inner()) = Some(loaded.clone());
    });
    Ok(loaded)
}

fn build_outcome(
    result: TranscriptionResult,
    params: &DictationParams,
    duration_secs: f32,
) -> DictationOutcome {
    let confidence = result.average_confidence.unwrap_or(0.0);
    let segments: Vec<(i64, i64, String)> = result
        .segments
        .iter()
        .map(|s| {
            (
                s.start_ms.unwrap_or(0),
                s.end_ms.unwrap_or(0),
                s.text.clone(),
            )
        })
        .collect();
    let cleaned_text = polish_transcript(&result.text, result.average_confidence, params);
    DictationOutcome {
        id: uuid::Uuid::new_v4().to_string(),
        diagnostics: crate::diagnostics::RunDiagnostics {
            backend: params.backend.clone(),
            model: match params.backend.as_str() {
                "parakeet" => "parakeet-tdt-0.6b-v3".into(),
                "qwen" => format!("qwen3-asr-{}", params.model_config.qwen_model_size),
                _ => params.model_config.selected_model.clone(),
            },
            compute: if params.backend == "parakeet" {
                "CPU"
            } else {
                "Runtime-managed (not reported)"
            }
            .into(),
            duration_secs,
            ..Default::default()
        },
        incomplete: None,
        raw_text: result.text,
        cleaned_text,
        confidence,
        confidence_available: result.average_confidence.is_some(),
        segments,
        detected_language: result.detected_language,
        duration_secs,
    }
}

/// The single polish pipeline applied to raw engine text — sanitize,
/// personal-dictionary replacements, mode formatting. Shared by the one-shot
/// and chunked paths so both produce identical output for the same raw text.
fn polish_transcript(raw: &str, confidence: Option<f32>, params: &DictationParams) -> String {
    let sanitized = if params.model_config.filter_hallucinations {
        postprocess::sanitize_transcript(raw, confidence)
    } else {
        postprocess::cleanup_transcript(raw)
    };
    // Apply personal-dictionary "heard → correct" replacements on the raw ASR
    // text before mode formatting (LLM variants are left untouched).
    let corrected = if params.replacements.is_empty() {
        sanitized
    } else {
        postprocess::apply_dictionary_replacements(&sanitized, &params.replacements)
    };
    apply_mode(&corrected, params.mode)
}

/// Combine while-recording chunk outcomes and the final tail outcome into one
/// polished [`DictationOutcome`], as if the whole take had been decoded at
/// once: RAW texts joined in order, the polish pipeline applied ONCE over the
/// combined text (so end-of-take hallucination stripping and mode formatting
/// see the full transcript — stripping per chunk would eat phrases that only
/// LOOK like endings), duration-weighted confidence, and segments concatenated
/// with cumulative time offsets.
pub fn combine_chunked_outcomes(
    chunks: Vec<DictationOutcome>,
    tail: Option<DictationOutcome>,
    params: &DictationParams,
    total_duration_secs: f32,
) -> DictationOutcome {
    let mut parts = chunks;
    if let Some(t) = tail {
        parts.push(t);
    }
    let raw_text = parts
        .iter()
        .map(|o| o.raw_text.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let mut segments: Vec<(i64, i64, String)> = Vec::new();
    let mut offset_ms: i64 = 0;
    let mut conf_sum = 0.0f32;
    let mut weight = 0.0f32;
    for o in &parts {
        for (s, e, t) in &o.segments {
            segments.push((s + offset_ms, e + offset_ms, t.clone()));
        }
        offset_ms += (o.duration_secs * 1000.0).round() as i64;
        let w = o.duration_secs.max(0.1);
        if o.confidence_available {
            conf_sum += o.confidence * w;
            weight += w;
        }
    }
    let confidence = if weight > 0.0 { conf_sum / weight } else { 0.0 };
    let detected_language = parts.iter().find_map(|o| o.detected_language.clone());
    let confidence_available = !parts.is_empty()
        && parts
            .iter()
            .all(|part| part.raw_text.is_empty() || part.confidence_available);
    let cleaned_text = polish_transcript(
        &raw_text,
        confidence_available.then_some(confidence),
        params,
    );
    DictationOutcome {
        id: parts
            .first()
            .map(|part| part.id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        diagnostics: crate::diagnostics::RunDiagnostics {
            duration_secs: total_duration_secs,
            inference_ms: parts.iter().map(|part| part.diagnostics.inference_ms).sum(),
            prepare_ms: parts.iter().map(|part| part.diagnostics.prepare_ms).sum(),
            model_load_ms: parts
                .iter()
                .map(|part| part.diagnostics.model_load_ms)
                .sum(),
            queue_ms: parts.iter().map(|part| part.diagnostics.queue_ms).sum(),
            ..parts
                .last()
                .map(|part| part.diagnostics.clone())
                .unwrap_or_default()
        },
        incomplete: parts.iter().find_map(|part| part.incomplete.clone()),
        raw_text,
        cleaned_text,
        confidence,
        confidence_available,
        segments,
        detected_language,
        duration_secs: total_duration_secs,
    }
}

/// Preserve committed speech on a failed tail, but retain its failure state.
pub fn finish_chunked_take(
    chunks: Vec<DictationOutcome>,
    tail: Result<DictationOutcome, String>,
    params: &DictationParams,
    duration: f32,
) -> Result<DictationOutcome, String> {
    if tail.as_ref().is_err_and(|error| {
        error == &crate::error::AppError::Cancelled.to_string() || error == "Dictation cancelled"
    }) {
        return tail;
    }
    if chunks.is_empty() {
        return tail;
    }
    match tail {
        Ok(tail) => Ok(combine_chunked_outcomes(
            chunks,
            Some(tail),
            params,
            duration,
        )),
        Err(error) => {
            let mut outcome = combine_chunked_outcomes(chunks, None, params, duration);
            if !error.starts_with("No clear speech") {
                outcome.incomplete = Some(error);
            }
            Ok(outcome)
        }
    }
}

/// Split a decoded chunk outcome at a cut that may have sliced a word in half.
/// Returns the outcome to commit — its text holds only committed words — plus
/// the snapshot-relative watermark (ms): everything from there was decoded but
/// is held back, and the next decode (or the final tail decode) re-covers it
/// whole. For a verified-quiet cut, or a decoder without segment timings, the
/// outcome is committed unchanged and the watermark is the cut itself.
pub fn split_chunk_outcome(
    outcome: DictationOutcome,
    cut_ms: f64,
    hold_back: bool,
    params: &DictationParams,
) -> (DictationOutcome, f64) {
    let (segments, watermark_ms) =
        crate::audio::chunking::split_commit(&outcome.segments, cut_ms, hold_back);
    if segments.len() == outcome.segments.len() {
        return (outcome, watermark_ms);
    }
    // Rebuild the raw text from the committed segments and re-run the polish
    // pipeline, so the outcome keeps its invariant cleaned = polish(raw). The
    // join is the same trim-and-space join the chunk combiner uses.
    let raw_text = segments
        .iter()
        .map(|(_, _, text)| text.trim())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let cleaned_text = polish_transcript(
        &raw_text,
        outcome.confidence_available.then_some(outcome.confidence),
        params,
    );
    tracing::debug!(
        "Holding back {} of {} chunk segments past the cut",
        outcome.segments.len() - segments.len(),
        outcome.segments.len()
    );
    (
        DictationOutcome {
            raw_text,
            cleaned_text,
            segments,
            duration_secs: (watermark_ms / 1000.0) as f32,
            ..outcome
        },
        watermark_ms,
    )
}

/// The shared engine slot: an `Arc<TranscriptionEngine>` inside so workers can
/// clone the handle out and decode WITHOUT holding the mutex — the lock is only
/// ever held for pointer swaps and metadata reads, never across a decode.
pub type SharedEngine = Arc<Mutex<Option<Arc<TranscriptionEngine>>>>;

/// Shared recording + transcription controller, owned by the `Application` and
/// `Rc`-shared with each UI. Lives entirely on the glib main thread.
pub struct RecordingController {
    /// Not shared: `AudioCapture` owns a cpal `Stream`, which is `!Send`, so the
    /// capture is main-thread-only and every caller goes through the methods
    /// below. The controller itself is what gets `Rc`-shared, so a plain
    /// `Mutex` field is enough for interior mutability behind `&self`.
    audio: Mutex<AudioCapture>,
    engine: SharedEngine,
    model_catalog: Arc<ModelCatalog>,
    owner: Cell<RecordingOwner>,
    inference_tx: SyncSender<InferenceJob>,
}

enum InferenceAudio {
    Prepared(Vec<f32>),
    Raw(RawAudioSnapshot),
}

struct InferenceJob {
    queued_at: std::time::Instant,
    audio: InferenceAudio,
    params: DictationParams,
    duration_secs: f32,
    reply: async_channel::Sender<Result<DictationOutcome, String>>,
    /// Aborted when the dictation that queued this job is cancelled: the job
    /// is then dropped before it starts (or its sidecar request mid-flight),
    /// never silently run to completion for a result nobody wants.
    cancel: crate::cancel::CancelToken,
}

impl RecordingController {
    pub fn new() -> Rc<Self> {
        let engine = Arc::new(Mutex::new(None));
        // Capacity 4: while-recording chunk decodes are steady-state load now
        // (≤1 in flight + its queued successor) and a stop's TAIL job must
        // never be rejected — the tail owns the take's only remaining audio
        // (the capture buffer was already drained into it), so a full-queue
        // drop there would destroy speech irrecoverably.
        let (inference_tx, inference_rx) = std::sync::mpsc::sync_channel::<InferenceJob>(4);
        let worker_engine = engine.clone();
        std::thread::Builder::new()
            .name("gui-transcribe".into())
            .spawn(move || {
                while let Ok(job) = inference_rx.recv() {
                    let InferenceJob {
                        queued_at,
                        audio,
                        params,
                        duration_secs,
                        reply,
                        cancel,
                    } = job;
                    // A cancelled dictation's queued jobs are dropped before
                    // they start — no minutes-long decode for a result the
                    // generation checks would discard anyway.
                    if cancel.is_aborted() {
                        let _ = reply.send_blocking(Err("Dictation cancelled".into()));
                        continue;
                    }
                    // One panicking job (a whisper edge case, bad audio) must
                    // not kill this sole worker thread — that would wedge every
                    // later dictation with "worker unavailable" until restart.
                    // Mirrors the API worker's catch_unwind.
                    let engine = worker_engine.clone();
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                        move || {
                            let queue_ms = crate::diagnostics::milliseconds(queued_at);
                            let prepare_start = std::time::Instant::now();
                            let (audio, lead_ms) = match audio {
                                InferenceAudio::Prepared(audio) => (audio, 0.0),
                                InferenceAudio::Raw(snapshot) => snapshot.condition_timed(),
                            };
                            let prepare_ms = crate::diagnostics::milliseconds(prepare_start);
                            if audio.is_empty() {
                                return Err(
                                    "No clear speech detected — try speaking closer to the microphone"
                                        .into(),
                                );
                            }
                            // Lazy-load the configured model when the shared slot
                            // is still empty (mirrors the API worker), so dictation
                            // from the mini panel / tray works even if no window
                            // ever loaded a model. Only when empty — a filled slot
                            // is the main window's to manage.
                            {
                                run_transcription(
                                    &engine,
                                    &audio,
                                    &params,
                                    duration_secs,
                                    Some(cancel),
                                )
                            }
                            .map(|mut outcome| {
                                outcome.diagnostics.queue_ms = queue_ms;
                                outcome.diagnostics.prepare_ms = prepare_ms;
                                crate::diagnostics::record(&outcome.diagnostics);
                                // Conditioning a snapshot trims leading silence, so
                                // segment timestamps come back relative to the
                                // trimmed audio. Move them onto the raw snapshot's
                                // timeline: the chunk hold-back rule maps them to raw
                                // sample indices to decide what may be committed.
                                let shift = lead_ms.round() as i64;
                                if shift > 0 {
                                    for seg in &mut outcome.segments {
                                        seg.0 += shift;
                                        seg.1 += shift;
                                    }
                                }
                                outcome
                            })
                        },
                    ))
                    .unwrap_or_else(|_| {
                        tracing::error!("Transcription job panicked; worker recovered");
                        Err("Transcription crashed unexpectedly — please try again.".into())
                    });
                    let _ = reply.send_blocking(result);
                }
            })
            .expect("failed to start transcription worker");

        Rc::new(Self {
            audio: Mutex::new(AudioCapture::new()),
            engine,
            model_catalog: Arc::new(ModelCatalog::new()),
            owner: Cell::new(RecordingOwner::None),
            inference_tx,
        })
    }

    pub fn engine_arc(&self) -> SharedEngine {
        self.engine.clone()
    }

    pub fn model_catalog_arc(&self) -> Arc<ModelCatalog> {
        self.model_catalog.clone()
    }

    // --- recording ownership -------------------------------------------------

    pub fn owner(&self) -> RecordingOwner {
        self.owner.get()
    }

    pub fn is_recording(&self) -> bool {
        self.owner.get() != RecordingOwner::None
    }

    /// Acquire the recording slot for `owner`. Prevents two cpal streams at once
    /// (main window vs. global shortcut). Self-healing: the real source of truth
    /// is whether audio is actually capturing, so if nothing is recording the
    /// slot is free even when a previous owner wasn't released — otherwise a
    /// missed `release()` (or a panic) would wedge recording forever.
    pub fn try_acquire(&self, owner: RecordingOwner) -> bool {
        let busy = self.state() != RecordingState::Idle && self.owner.get() != RecordingOwner::None;
        if busy {
            return self.owner.get() == owner;
        }
        self.owner.set(owner);
        true
    }

    pub fn release(&self) {
        self.owner.set(RecordingOwner::None);
    }

    // --- capture (main thread only: cpal Stream is !Send) --------------------

    /// Start capturing from `device` (or the default), wiring `waveform_tx` for
    /// UI visualization.
    pub fn start(
        &self,
        device: Option<&str>,
        waveform_tx: async_channel::Sender<Vec<f32>>,
    ) -> AppResult<()> {
        let mut cap = self.audio.lock().unwrap_or_else(|e| e.into_inner());
        cap.set_waveform_sender(waveform_tx);
        cap.start_recording(device)
    }

    /// Stop capturing and return the recorded mono 16kHz audio.
    pub fn stop(&self) -> AppResult<Vec<f32>> {
        let mut cap = self.audio.lock().unwrap_or_else(|e| e.into_inner());
        cap.stop_recording()
    }

    /// Stop capture and detach raw audio without conditioning it on the caller's
    /// thread. Use with [`Self::transcribe_snapshot_async`].
    pub fn stop_snapshot(&self) -> AppResult<RawAudioSnapshot> {
        let mut cap = self.audio.lock().unwrap_or_else(|e| e.into_inner());
        cap.stop_recording_snapshot()
    }

    /// Stop capturing and discard the audio. O(1): detaches the raw samples
    /// and drops them — running the full mono+sinc conditioning pipeline (as
    /// `stop_recording` does) for audio nobody wants froze the GTK thread for
    /// seconds when cancelling long recordings.
    pub fn cancel(&self) {
        let mut cap = self.audio.lock().unwrap_or_else(|e| e.into_inner());
        let _ = cap.stop_recording_snapshot();
    }

    pub fn pause(&self) {
        if let Ok(mut cap) = self.audio.lock() {
            cap.pause();
        }
    }

    pub fn resume(&self) {
        if let Ok(mut cap) = self.audio.lock() {
            cap.resume();
        }
    }

    pub fn state(&self) -> RecordingState {
        self.audio
            .lock()
            .map(|c| c.state())
            .unwrap_or(RecordingState::Idle)
    }

    pub fn recording_duration_secs(&self) -> f32 {
        self.audio
            .lock()
            .map(|c| c.recording_duration_secs())
            .unwrap_or(0.0)
    }

    /// Take (and clear) a fatal stream error reported by the audio backend.
    /// UI timers poll this while recording so a dead stream (mic unplugged)
    /// auto-stops instead of showing a live "Recording" state forever.
    pub fn take_stream_error(&self) -> Option<String> {
        self.audio.lock().ok().and_then(|c| c.take_stream_error())
    }

    /// While-recording chunk peek for the global dictation's chunked decode
    /// (see [`crate::audio::buffer::AudioBuffer::peek_stable_chunk`]).
    pub fn peek_stable_chunk(
        &self,
        min_secs: f32,
        max_secs: f32,
    ) -> Option<crate::audio::buffer::ChunkCut> {
        self.audio
            .lock()
            .ok()
            .and_then(|c| c.peek_stable_chunk(min_secs, max_secs))
    }

    /// Commit a successfully decoded chunk's watermark so the next decode (and
    /// the final tail decode) covers only what is not yet committed.
    pub fn commit_chunk(&self, watermark: usize) {
        if let Ok(cap) = self.audio.lock() {
            cap.commit_consumed(watermark);
        }
    }

    /// Non-destructive RAW snapshot of the audio captured so far, for live
    /// transcription while recording continues. Conditioning (mono + sinc
    /// resample) is deliberately left to the inference worker via
    /// [`Self::transcribe_snapshot_async`] — running it on the GTK thread
    /// caused visible jank on every live-preview tick.
    pub fn live_snapshot_raw(&self, max_samples: usize) -> Option<RawAudioSnapshot> {
        self.audio.lock().ok().map(|c| c.snapshot_raw(max_samples))
    }

    /// Spawn transcription on a worker thread; the returned receiver yields the
    /// outcome once, to be awaited via `glib::spawn_future_local`. `cancel`
    /// drops the job (and its sidecar request) when the dictation is cancelled.
    pub fn transcribe_async(
        &self,
        audio: Vec<f32>,
        params: DictationParams,
        duration_secs: f32,
        cancel: crate::cancel::CancelToken,
    ) -> async_channel::Receiver<Result<DictationOutcome, String>> {
        let (sender, receiver) = async_channel::bounded(1);
        self.enqueue_inference(InferenceJob {
            queued_at: std::time::Instant::now(),
            audio: InferenceAudio::Prepared(audio),
            params,
            duration_secs,
            reply: sender,
            cancel,
        });
        receiver
    }

    /// Condition detached raw audio and transcribe it entirely on a worker.
    pub fn transcribe_snapshot_async(
        &self,
        snapshot: RawAudioSnapshot,
        params: DictationParams,
        duration_secs: f32,
        cancel: crate::cancel::CancelToken,
    ) -> async_channel::Receiver<Result<DictationOutcome, String>> {
        let (sender, receiver) = async_channel::bounded(1);
        self.enqueue_inference(InferenceJob {
            queued_at: std::time::Instant::now(),
            audio: InferenceAudio::Raw(snapshot),
            params,
            duration_secs,
            reply: sender,
            cancel,
        });
        receiver
    }

    fn enqueue_inference(&self, job: InferenceJob) {
        if let Err(error) = self.inference_tx.try_send(job) {
            let (job, message) = match error {
                TrySendError::Full(job) => (job, "Transcription queue is full; retry shortly."),
                TrySendError::Disconnected(job) => (job, "Transcription worker is unavailable."),
            };
            let _ = job.reply.try_send(Err(message.into()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_round_trips_through_config_str() {
        for s in ["plain", "message", "email", "note", "code_prompt"] {
            let mode = DictationMode::from_config_str(s);
            assert_eq!(mode.as_config_str(), s);
        }
    }

    #[test]
    fn unknown_mode_falls_back_to_plain() {
        assert_eq!(
            DictationMode::from_config_str("nonsense"),
            DictationMode::Plain
        );
    }

    #[test]
    fn plain_mode_is_identity() {
        let sanitized = postprocess::sanitize_transcript("hello world. this is a test", Some(0.9));
        assert_eq!(apply_mode(&sanitized, DictationMode::Plain), sanitized);
    }

    #[test]
    fn message_mode_adds_terminal_punctuation_and_single_line() {
        assert_eq!(
            apply_mode("Hello world", DictationMode::Message),
            "Hello world."
        );
        assert_eq!(apply_mode("Hi there.", DictationMode::Message), "Hi there.");
        assert_eq!(
            apply_mode("line one\nline two", DictationMode::Message),
            "line one line two."
        );
    }

    #[test]
    fn email_mode_wraps_with_greeting_and_signoff() {
        let out = apply_mode("Please review the attached document.", DictationMode::Email);
        assert!(out.starts_with("Hi,\n\n"));
        assert!(out.contains("Please review the attached document."));
        assert!(out.trim_end().ends_with("Best regards,"));
    }

    #[test]
    fn note_mode_bullets_each_sentence() {
        let out = apply_mode(
            "Buy milk. Call the bank. Finish the report.",
            DictationMode::Note,
        );
        assert_eq!(out, "• Buy milk.\n• Call the bank.\n• Finish the report.");
    }

    #[test]
    fn code_prompt_mode_strips_fillers() {
        let out = apply_mode("Um, add a uh retry loop", DictationMode::CodePrompt);
        assert_eq!(out, "Add a retry loop.");
    }

    #[test]
    fn empty_input_stays_empty_for_all_modes() {
        for m in [
            DictationMode::Plain,
            DictationMode::Message,
            DictationMode::Email,
            DictationMode::Note,
            DictationMode::CodePrompt,
        ] {
            assert_eq!(apply_mode("   ", m), "");
        }
    }

    fn plain_params() -> DictationParams {
        DictationParams {
            model_config: Arc::new(crate::config::AppConfig::default()),
            backend: "whisper".to_string(),
            language_code: None,
            n_threads: 1,
            beam_size: 1,
            temperature: 0.0,
            translate: false,
            initial_prompt: None,
            selected_microphone: None,
            mode: DictationMode::Plain,
            replacements: Vec::new(),
        }
    }

    fn chunk_outcome(segments: Vec<(i64, i64, String)>) -> DictationOutcome {
        let raw_text = segments
            .iter()
            .map(|(_, _, t)| t.trim())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        DictationOutcome {
            incomplete: None,
            id: uuid::Uuid::new_v4().to_string(),
            diagnostics: Default::default(),
            raw_text: raw_text.clone(),
            cleaned_text: raw_text,
            confidence: 0.9,
            confidence_available: true,
            segments,
            detected_language: None,
            duration_secs: 20.0,
        }
    }

    #[test]
    fn split_holds_back_the_cut_tail_and_rebuilds_text() {
        let out = chunk_outcome(vec![
            (0, 4_000, "hello".into()),
            (19_600, 19_950, "wor".into()),
        ]);
        let (committed, watermark) = split_chunk_outcome(out, 20_000.0, true, &plain_params());
        assert_eq!(committed.raw_text, "hello");
        assert_eq!(committed.segments.len(), 1);
        assert_eq!(watermark, 19_600.0);
        assert_eq!(committed.duration_secs, 19.6);
        let tail = DictationOutcome {
            duration_secs: 2.0,
            ..chunk_outcome(vec![(0, 1500, "world".into())])
        };
        let combined = combine_chunked_outcomes(vec![committed], Some(tail), &plain_params(), 21.6);
        assert_eq!(combined.raw_text, "hello world");
        assert_eq!(combined.segments[1].0, 19600);
        assert_eq!(combined.segments[1].1, 21100);
    }

    #[test]
    fn failed_tail_is_partial_but_silent_tail_is_complete() {
        assert!(finish_chunked_take(
            vec![chunk_outcome(vec![(0, 1000, "retained speech".into())])],
            Err(crate::error::AppError::Cancelled.to_string()),
            &plain_params(),
            25.0,
        )
        .is_err());
        for (error, partial) in [
            ("decoder failed", true),
            ("No clear speech detected", false),
        ] {
            let out = finish_chunked_take(
                vec![chunk_outcome(vec![(0, 1000, "retained speech".into())])],
                Err(error.into()),
                &plain_params(),
                25.0,
            )
            .unwrap();
            assert_eq!(out.raw_text, "retained speech");
            assert_eq!(out.incomplete.is_some(), partial);
        }
        assert!(
            finish_chunked_take(vec![], Err("decoder failed".into()), &plain_params(), 1.0)
                .is_err()
        );
    }

    #[test]
    fn split_is_identity_for_a_verified_cut() {
        let out = chunk_outcome(vec![
            (0, 4_000, "hello".into()),
            (19_600, 19_950, "wor".into()),
        ]);
        let (committed, watermark) = split_chunk_outcome(out, 20_000.0, false, &plain_params());
        assert_eq!(committed.raw_text, "hello wor");
        assert_eq!(committed.segments.len(), 2);
        assert_eq!(watermark, 20_000.0);
    }

    #[test]
    fn split_is_identity_for_untimed_segments() {
        // Sidecar backends report no real timings: their chunks commit whole.
        let out = chunk_outcome(vec![(0, 0, "hello".into()), (0, 0, "wor".into())]);
        let (committed, watermark) = split_chunk_outcome(out, 20_000.0, true, &plain_params());
        assert_eq!(committed.raw_text, "hello wor");
        assert_eq!(watermark, 20_000.0);
    }
}
