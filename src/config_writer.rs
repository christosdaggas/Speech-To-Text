//! One coalescing, ordered configuration writer. No disk I/O on GTK callbacks.
use crate::config::AppConfig;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveStatus {
    Pending,
    Persisted,
    Failed(String),
}

#[derive(Default)]
struct State {
    requested: u64,
    completed: u64,
    pending: Option<(u64, AppConfig)>,
    error: Option<String>,
    closed: bool,
}

pub struct ConfigWriter {
    shared: Arc<(Mutex<State>, Condvar)>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl ConfigWriter {
    pub fn new(path: PathBuf) -> Result<Self, String> {
        let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let worker_shared = shared.clone();
        let worker = std::thread::Builder::new()
            .name("config-writer".into())
            .spawn(move || {
                let (lock, wake) = &*worker_shared;
                loop {
                    let (sequence, config) = {
                        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                        while state.pending.is_none() && !state.closed {
                            state = wake.wait(state).unwrap_or_else(|e| e.into_inner());
                        }
                        match state.pending.take() {
                            Some(value) => value,
                            None => break,
                        }
                    };
                    let result = serde_json::to_vec_pretty(&config)
                        .map_err(|e| e.to_string())
                        .and_then(|json| {
                            crate::fsio::write_private(&path, &json).map_err(|e| e.to_string())
                        });
                    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                    state.completed = sequence;
                    state.error = result.err().map(|e| crate::error::redact_secrets(&e));
                    wake.notify_all();
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            shared,
            worker: Some(worker),
        })
    }

    pub fn submit(&self, config: AppConfig) {
        let (lock, wake) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        state.requested += 1;
        state.pending = Some((state.requested, config));
        wake.notify_all();
    }

    pub fn status(&self) -> SaveStatus {
        let state = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        status(&state)
    }

    pub fn flush(&self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let (lock, wake) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        while state.completed < state.requested {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("Configuration save timed out".into());
            }
            state = wake
                .wait_timeout(state, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        match &state.error {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }
}

fn status(state: &State) -> SaveStatus {
    if state.completed < state.requested {
        SaveStatus::Pending
    } else if let Some(error) = &state.error {
        SaveStatus::Failed(error.clone())
    } else {
        SaveStatus::Persisted
    }
}

impl Drop for ConfigWriter {
    fn drop(&mut self) {
        let (lock, wake) = &*self.shared;
        lock.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
        wake.notify_all();
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                tracing::error!("Configuration writer panicked");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn writer_reports_failure_retries_and_flushes_latest_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::create_dir(&path).unwrap();
        let writer = ConfigWriter::new(path.clone()).unwrap();
        writer.submit(AppConfig::default());
        assert!(writer.flush(Duration::from_secs(2)).is_err());
        assert!(matches!(writer.status(), SaveStatus::Failed(_)));
        std::fs::remove_dir(&path).unwrap();
        for count in 1..=20 {
            writer.submit(AppConfig {
                beam_size: count,
                ..AppConfig::default()
            });
        }
        writer.flush(Duration::from_secs(2)).unwrap();
        assert_eq!(writer.status(), SaveStatus::Persisted);
        let config: AppConfig = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(config.beam_size, 20);
    }
}
