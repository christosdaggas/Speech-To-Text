//! Session diagnostics contain operational metadata only, never speech or paths.
use serde::Serialize;
use std::sync::Mutex;

#[derive(Clone, Debug, Default, Serialize)]
pub struct RunDiagnostics {
    pub backend: String,
    pub model: String,
    pub compute: String,
    pub queue_ms: f64,
    pub prepare_ms: f64,
    pub model_load_ms: f64,
    pub inference_ms: f64,
    pub duration_secs: f32,
}

static LAST: Mutex<Option<RunDiagnostics>> = Mutex::new(None);

pub fn record(run: &RunDiagnostics) {
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = Some(run.clone());
}

pub fn latest() -> Option<RunDiagnostics> {
    LAST.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn milliseconds(started: std::time::Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}
