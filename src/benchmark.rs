//! Explicit, headless, offline benchmark command. Does not write user history.
use crate::recording::{DictationParams, SharedEngine};
use serde::Deserialize;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Deserialize)]
struct Manifest {
    cases: Vec<Case>,
}
#[derive(Deserialize)]
struct Case {
    id: String,
    path: String,
    sha256: String,
    language: String,
    reference: String,
}

pub fn run(args: &[String]) -> Result<(), String> {
    if args.len() != 4 || !matches!(args[3].as_str(), "cpu" | "gpu") {
        return Err("Usage: --benchmark MANIFEST.json whisper|qwen|parakeet MODEL cpu|gpu".into());
    }
    if crate::transcription::capabilities::for_backend(&args[1]).is_none() {
        return Err("Unknown backend".into());
    }
    let manifest_path = Path::new(&args[0]);
    let manifest: Manifest =
        serde_json::from_slice(&std::fs::read(manifest_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    if manifest.cases.is_empty() || manifest.cases.len() > 100 {
        return Err("Expected 1–100 benchmark cases".into());
    }
    let engine: SharedEngine = Arc::new(Mutex::new(None));
    let mut config = crate::config::AppConfig {
        backend: args[1].clone(),
        selected_model: args[2].clone(),
        use_gpu: args[3] == "gpu",
        cpu_fallback: false,
        ..Default::default()
    };
    config.temperature = 0.0;
    if config.backend == "qwen" {
        if !matches!(args[2].as_str(), "0.6B" | "1.7B") {
            return Err("Qwen MODEL must be 0.6B or 1.7B".into());
        }
        config.qwen_model_size = args[2].clone();
    }
    config.beam_size = 2;
    let mut results = Vec::new();
    for case in manifest.cases {
        let path = manifest_path
            .parent()
            .unwrap_or(Path::new("."))
            .join(&case.path);
        crate::transcription::verify::verify_file(&path, &case.sha256)
            .map_err(|e| e.to_string())?;
        config.auto_detect_language = false;
        config.language = Some(case.language.clone());
        let params = DictationParams::from_config(&config);
        for pass in 0..2 {
            let start = std::time::Instant::now();
            let audio =
                crate::audio::file_decoder::decode_audio_file(&path).map_err(|e| e.to_string())?;
            let prepare_ms = crate::diagnostics::milliseconds(start);
            // Benchmark owns this slot; publish locally without changing the
            // interactive application's selected model or configuration.
            let load_start = std::time::Instant::now();
            if config.backend == "whisper" {
                let loaded = crate::recording::ensure_engine_loaded(&engine, &config)?;
                *engine.lock().unwrap_or_else(|e| e.into_inner()) = Some(loaded);
            }
            let load_ms = crate::diagnostics::milliseconds(load_start);
            let mut outcome = crate::recording::run_transcription(
                &engine,
                &audio.samples,
                &params,
                audio.source_duration_secs,
                // Exercise the same native abort callback as GUI/API jobs,
                // even though this command does not request cancellation.
                Some(crate::cancel::CancelToken::new()),
            )?;
            outcome.shift_timestamps(audio.leading_offset_ms);
            outcome.diagnostics.prepare_ms = prepare_ms;
            outcome.diagnostics.model_load_ms += load_ms;
            let total_ms = crate::diagnostics::milliseconds(start);
            results.push(serde_json::json!({ "id": case.id, "pass": pass,
                "language": case.language, "reference": case.reference,
                "raw_text": outcome.raw_text, "text": outcome.cleaned_text,
                "total_ms": total_ms, "rtf": total_ms / 1000.0 / f64::from(audio.source_duration_secs),
                "diagnostics": outcome.diagnostics }));
        }
    }
    let peak_memory = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find(|line| line.starts_with("VmHWM:"))
                .map(str::to_string)
        });
    let result = serde_json::json!({ "version": crate::VERSION, "backend": config.backend,
        "model": config.selected_model, "compute_requested": args[3], "peak_memory": peak_memory,
        "note": "Pass 0 is first-use for each clip, not a fresh process; pass 1 repeats the clip. Synthetic speech is a regression probe, not a general accuracy benchmark.", "results": results });
    println!(
        "{}",
        serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?
    );
    crate::transcription::sidecar_server::shutdown_all();
    Ok(())
}
