// Copyright (C) 2026 Christos A. Daggas
// SPDX-License-Identifier: MIT

//! Thin wrappers around the XDG Desktop Portals used by the mini panel:
//! `GlobalShortcuts` (the global dictation hotkey) and `RemoteDesktop`
//! (best-effort auto-paste). All entry points are best-effort: if a portal is
//! unavailable or the user denies it, the app keeps working with in-app
//! recording and clipboard-only paste.

pub mod paste;
pub mod shortcuts;

static REGISTERED: tokio::sync::Mutex<bool> = tokio::sync::Mutex::const_new(false);

/// No dependent portal call may proceed after failure: that would permanently
/// associate ashpd's shared connection with an empty app ID until app restart.
pub async fn ensure_host_app_registered() -> Result<(), ashpd::Error> {
    register_once(&REGISTERED, || async {
        let app_id = ashpd::AppID::try_from(crate::APP_ID)?;
        tokio::time::timeout(
            std::time::Duration::from_secs(8),
            ashpd::register_host_app(app_id),
        )
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Portal registration timed out",
            )
        })??;
        tracing::info!("Registered host app id '{}' with the portal", crate::APP_ID);
        Ok(())
    })
    .await
}

async fn register_once<F, Fut>(
    state: &tokio::sync::Mutex<bool>,
    register: F,
) -> Result<(), ashpd::Error>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), ashpd::Error>>,
{
    let mut registered = state.lock().await;
    if !*registered {
        register().await?;
        *registered = true;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn failed_registration_blocks_portal_use_and_can_be_retried() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let state = tokio::sync::Mutex::new(false);
            let used_portal = AtomicUsize::new(0);
            let result: Result<(), ashpd::Error> = async {
                register_once(&state, || async { Err(ashpd::Error::NoResponse) }).await?;
                used_portal.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            .await;
            assert!(result.is_err());
            assert_eq!(used_portal.load(Ordering::SeqCst), 0);
            assert!(!*state.lock().await);
            register_once(&state, || async { Ok(()) }).await.unwrap();
            register_once(&state, || async { panic!("registration repeated") })
                .await
                .unwrap();
        });
    }

    #[test]
    fn concurrent_portal_users_wait_for_one_successful_registration() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let state = tokio::sync::Mutex::new(false);
            let calls = AtomicUsize::new(0);
            let register = || async {
                calls.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
                Ok(())
            };
            let (a, b) = tokio::join!(
                register_once(&state, register),
                register_once(&state, register)
            );
            a.unwrap();
            b.unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        });
    }
}
