// Copyright (C) 2026 Christos A. Daggas
// SPDX-License-Identifier: MIT

//! Cooperative cancellation for transcription work.
//!
//! A dictation's decode jobs and sidecar requests share one [`CancelToken`],
//! created when the dictation starts and aborted when the user cancels it.
//! Cancellation is out-of-band by construction: queued jobs are dropped before
//! they start, an in-flight sidecar HTTP request is dropped mid-flight (its
//! connection closes). Whisper observes the flag through its abort callback;
//! sidecar subprocesses are killed and reaped on cancellation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A shared abort flag. Clones observe (and one owner issues) the abort.
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    aborted: Arc<AtomicBool>,
    parent: Option<Arc<CancelToken>>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Abort every holder of this token. Idempotent.
    pub fn abort(&self) {
        self.aborted.store(true, Ordering::SeqCst);
    }

    pub fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::SeqCst) || self.parent.as_ref().is_some_and(|p| p.is_aborted())
    }

    pub fn child(&self) -> Self {
        Self {
            aborted: Arc::new(AtomicBool::new(false)),
            parent: Some(Arc::new(self.clone())),
        }
    }
}

/// The request future owns this guard; dropping it cancels queued and active work.
pub struct AbortOnDrop(pub CancelToken);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abort_is_visible_to_every_clone() {
        let token = CancelToken::new();
        let observer = token.clone();
        assert!(!observer.is_aborted());
        token.abort();
        assert!(observer.is_aborted());
        assert!(token.is_aborted());
    }

    #[test]
    fn fresh_tokens_are_independent() {
        let a = CancelToken::new();
        a.abort();
        assert!(!CancelToken::new().is_aborted());
    }
    #[test]
    fn request_drop_and_server_shutdown_cancel_active_work() {
        let server = CancelToken::new();
        let first = server.child();
        let second = server.child();
        drop(super::AbortOnDrop(first.clone()));
        assert!(first.is_aborted());
        assert!(!second.is_aborted());
        assert!(!server.is_aborted());
        server.abort();
        assert!(second.is_aborted());
        assert!(server.child().is_aborted());
    }
}
