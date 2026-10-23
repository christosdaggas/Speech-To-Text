// Copyright (C) 2026 Christos A. Daggas
// SPDX-License-Identifier: MIT

//! Retain the portal session/proxy while listening; dropping either unbinds it.
//! GNOME owns the actual binding: preferred_trigger is only a suggestion.

use ashpd::desktop::global_shortcuts::{
    BindShortcutsOptions, ConfigureShortcutsOptions, GlobalShortcuts, NewShortcut,
};
use ashpd::desktop::CreateSessionOptions;
use futures::StreamExt;
use gtk4::gio::prelude::SettingsExt;
use tracing::{info, warn};

const SHORTCUT_ID: &str = "start_dictation";
const TRANSFORM_SHORTCUT_ID: &str = "transform_selection";
const SHORTCUT_DESCRIPTION: &str = "Dictation";
/// Description used by older releases. GNOME persists this beside the binding
/// and does not update it from a later `BindShortcuts` for the same id.
const LEGACY_SHORTCUT_DESCRIPTION: &str = "Start dictation";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutKind {
    Dictation,
    TransformSelection,
}

#[derive(Debug)]
pub enum ShortcutCommand {
    Configure(async_channel::Sender<Result<(), String>>),
}

/// Open the desktop's shortcut configuration dialog.
///
/// `ashpd` gates `ConfigureShortcuts` on interface version 2, but
/// xdg-desktop-portal 1.22 (Fedora 44) ships the method while still
/// advertising version 1. The method exists on the same proxy and connection,
/// so only in that exact mismatch call it directly; a real version-1 portal
/// without the method still fails cleanly. Newer portals take the normal path.
async fn configure_shortcuts_compat(
    proxy: &GlobalShortcuts,
    session: &ashpd::desktop::Session<GlobalShortcuts>,
) -> Result<(), String> {
    match proxy
        .configure_shortcuts(session, None, ConfigureShortcutsOptions::default())
        .await
    {
        Ok(()) => Ok(()),
        Err(ashpd::Error::RequiresVersion(2, 1)) => {
            // Session's Serialize impl emits its D-Bus object path; serde_json
            // gives us the path string without needing ashpd's private getter.
            let session_json = serde_json::to_value(session)
                .map_err(|e| format!("Could not serialize portal session: {e}"))?;
            let session_path = session_json
                .as_str()
                .ok_or_else(|| "Portal session did not serialize to an object path".to_string())?;
            let session_path = ashpd::zvariant::ObjectPath::try_from(session_path)
                .map_err(|e| format!("Invalid portal session path: {e}"))?;
            let options: std::collections::HashMap<String, ashpd::zvariant::Value> =
                std::collections::HashMap::new();
            proxy
                .call_method("ConfigureShortcuts", &(session_path, "", options))
                .await
                .map(|_| ())
                .map_err(|e| format!("ConfigureShortcuts failed: {e}"))
        }
        Err(e) => Err(e.to_string()),
    }
}

/// GNOME persists descriptions beside bindings; BindShortcuts cannot rename
/// them. Migrate only the stock legacy label, preserving user edits.
pub fn migrate_legacy_shortcut_label() {
    if !std::env::var("XDG_CURRENT_DESKTOP")
        .map(|desktops| {
            desktops
                .split(':')
                .any(|desktop| desktop.eq_ignore_ascii_case("GNOME"))
        })
        .unwrap_or(false)
    {
        return;
    }

    let Some(source) = gtk4::gio::SettingsSchemaSource::default() else {
        return;
    };
    let Some(schema) = source.lookup(
        "org.gnome.settings-daemon.global-shortcuts.application",
        true,
    ) else {
        return;
    };

    let path = format!(
        "/org/gnome/settings-daemon/global-shortcuts/{}/",
        crate::APP_ID
    );
    let settings =
        gtk4::gio::Settings::new_full(&schema, None::<&gtk4::gio::SettingsBackend>, Some(&path));
    let stored = settings.value("shortcuts");

    let mut changed = false;
    let mut entries = Vec::new();
    for child in stored.iter() {
        if child.n_children() != 2 {
            entries.push(child);
            continue;
        }

        let Some(id) = child.child_value(0).get::<String>() else {
            entries.push(child);
            continue;
        };
        let mut config = child.child_value(1);

        if id == SHORTCUT_ID {
            let dict = gtk4::glib::VariantDict::new(Some(&config));
            if let Some(description) =
                dict.lookup_value("description", Some(gtk4::glib::VariantTy::STRING))
            {
                if description.get::<String>().as_deref() == Some(LEGACY_SHORTCUT_DESCRIPTION) {
                    dict.insert("description", SHORTCUT_DESCRIPTION);
                    config = dict.end();
                    changed = true;
                }
            }
        }

        entries.push(gtk4::glib::Variant::tuple_from_iter([
            gtk4::glib::Variant::from(id),
            config,
        ]));
    }

    if !changed {
        return;
    }

    let Ok(entry_type) = gtk4::glib::VariantTy::new("(sa{sv})") else {
        return;
    };
    let renamed = gtk4::glib::Variant::array_from_iter_with_type(entry_type, entries);
    if settings.set_value("shortcuts", &renamed).is_err() {
        tracing::warn!("Could not migrate the GNOME shortcut description");
    } else {
        tracing::info!(
            "Renamed the GNOME global shortcut to '{}'",
            SHORTCUT_DESCRIPTION
        );
    }
}

/// Convert a GTK accelerator string (e.g. `"<Ctrl><Alt>space"`) into the XDG
/// "shortcuts" trigger format (e.g. `"CTRL+ALT+space"`). Returns `None` when the
/// input doesn't look like a GTK accelerator, in which case no preferred trigger
/// is suggested and the desktop prompts the user to bind one.
pub(crate) fn gtk_accel_to_xdg_trigger(accel: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut rest = accel.trim();

    while rest.starts_with('<') {
        let end = rest.find('>')?;
        let token = &rest[1..end];
        let modifier = match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "primary" => "CTRL",
            "alt" => "ALT",
            "shift" => "SHIFT",
            "super" | "meta" | "logo" | "mod4" => "LOGO",
            _ => return None,
        };
        parts.push(modifier.to_string());
        rest = rest[end + 1..].trim_start();
    }

    let key = rest.trim();
    if key.is_empty()
        || key.contains(['<', '>', '+'])
        || key.chars().any(char::is_whitespace)
        || gtk4::gdk::Key::from_name(key).is_none()
        || matches!(
            key,
            "Control_L"
                | "Control_R"
                | "Alt_L"
                | "Alt_R"
                | "Shift_L"
                | "Shift_R"
                | "Super_L"
                | "Super_R"
                | "Meta_L"
                | "Meta_R"
                | "ISO_Level3_Shift"
        )
    {
        return None;
    }
    parts.push(key.to_string());
    Some(parts.join("+"))
}

/// Long-lived task: create the session, bind the preferred trigger(s), then
/// forward each activation into `tx`. `transform_trigger` is `Some` only when
/// the transform-selection shortcut is enabled.
///
/// The portal session can die mid-run (xdg-desktop-portal crash or upgrade,
/// session teardown). For an app that lives in the tray for days, a one-shot
/// registration silently killed the primary interaction path until restart —
/// so the session is recreated with backoff for as long as the app runs. (If
/// the portal dies without ending the signal stream, activations stop without
/// a detectable event; that residual case still needs an app restart.)
pub async fn run_global_shortcuts(
    dictation_trigger: String,
    transform_trigger: Option<String>,
    tx: async_channel::Sender<ShortcutKind>,
    commands: async_channel::Receiver<ShortcutCommand>,
) {
    let mut delay = std::time::Duration::from_secs(2);
    loop {
        let started = std::time::Instant::now();
        let result = run_inner(
            &dictation_trigger,
            transform_trigger.clone(),
            &tx,
            &commands,
        )
        .await;
        if tx.is_closed() || commands.is_closed() {
            return; // app shutting down
        }
        let failure = match result {
            Ok(()) => "Global shortcuts session ended".to_string(),
            Err(e) => e.to_string(),
        };
        warn!("Global shortcuts unavailable ({failure}); retrying in {delay:?}");
        // A run that survived a while means the portal was healthy; restart
        // the backoff from the beginning.
        if started.elapsed() >= std::time::Duration::from_secs(60) {
            delay = std::time::Duration::from_secs(2);
        }
        // Answer configuration requests even while registration is unavailable.
        // A fixed deadline preserves retry progress if the user clicks again.
        if !wait_to_retry(&commands, delay, &failure).await {
            return;
        }
        delay = (delay * 2).min(std::time::Duration::from_secs(120));
    }
}

/// Keep the UI responsive during portal recovery; false means app shutdown.
async fn wait_to_retry(
    commands: &async_channel::Receiver<ShortcutCommand>,
    delay: std::time::Duration,
    failure: &str,
) -> bool {
    let retry_at = tokio::time::Instant::now() + delay;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(retry_at) => return true,
            command = commands.recv() => match command {
                Ok(ShortcutCommand::Configure(reply)) => {
                    let _ = reply.try_send(Err(failure.to_string()));
                }
                Err(_) => return false,
            }
        }
    }
}

async fn run_inner(
    dictation_trigger: &str,
    transform_trigger: Option<String>,
    tx: &async_channel::Sender<ShortcutKind>,
    commands: &async_channel::Receiver<ShortcutCommand>,
) -> Result<(), ashpd::Error> {
    super::ensure_host_app_registered().await?;

    let proxy = GlobalShortcuts::new().await?;
    let session = proxy
        .create_session(CreateSessionOptions::default())
        .await?;

    // Pre-compute the XDG triggers so they outlive the borrowed `NewShortcut`s.
    let dict_trigger = gtk_accel_to_xdg_trigger(dictation_trigger);
    if dict_trigger.is_none() {
        warn!("Could not parse dictation shortcut '{dictation_trigger}'; the desktop will prompt for a binding");
    }
    let xform_trigger = transform_trigger
        .as_deref()
        .map(|t| (t.to_string(), gtk_accel_to_xdg_trigger(t)));

    let mut shortcuts = vec![NewShortcut::new(SHORTCUT_ID, SHORTCUT_DESCRIPTION)
        .preferred_trigger(dict_trigger.as_deref())];
    if let Some((raw, xdg)) = &xform_trigger {
        if xdg.is_none() {
            warn!(
                "Could not parse transform shortcut '{raw}'; the desktop will prompt for a binding"
            );
        }
        shortcuts.push(
            NewShortcut::new(TRANSFORM_SHORTCUT_ID, "Transform selection with AI")
                .preferred_trigger(xdg.as_deref()),
        );
    }

    // Subscribe before binding so an immediate activation isn't missed.
    let mut activated = proxy.receive_activated().await?;

    let request = proxy
        .bind_shortcuts(&session, &shortcuts, None, BindShortcutsOptions::default())
        .await?;
    match request.response() {
        Ok(bound) => info!(
            "Global shortcut(s) bound ({} registered)",
            bound.shortcuts().len()
        ),
        Err(e) => warn!("BindShortcuts returned an error (continuing): {e}"),
    }

    info!("Listening for global shortcuts");

    loop {
        tokio::select! {
            activation = activated.next() => {
                let Some(activation) = activation else { break };
                let id = activation.shortcut_id();
                let kind = if id == SHORTCUT_ID {
                    Some(ShortcutKind::Dictation)
                } else if id == TRANSFORM_SHORTCUT_ID {
                    Some(ShortcutKind::TransformSelection)
                } else {
                    None
                };
                if let Some(kind) = kind {
                    info!("Global shortcut activated: {kind:?}");
                    if tx.send(kind).await.is_err() {
                        break; // receiver dropped → app shutting down
                    }
                }
            }
            command = commands.recv() => {
                match command {
                    Ok(ShortcutCommand::Configure(reply)) => {
                        // Timed-out UI requests must never open a late dialog.
                        if reply.is_closed() { continue; }
                        // GNOME owns the binding; this is the supported way to
                        // open its configuration dialog for the live session.
                        // The call returns once the desktop has been asked to
                        // show the dialog, not when the user closes it.
                        let result = tokio::time::timeout(
                            std::time::Duration::from_secs(8),
                            configure_shortcuts_compat(&proxy, &session),
                        ).await.unwrap_or_else(|_| Err("Shortcut configuration timed out".into()));
                        if let Err(e) = &result {
                            warn!(
                                "Could not open the global shortcut configuration dialog: {e}"
                            );
                        }
                        let _ = reply.try_send(result);
                    }
                    Err(_) => break, // app shutting down
                }
            }
        }
    }

    // Hold the session for the whole loop; dropping it ends the registration.
    drop(session);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_portal_answers_requests_and_shuts_down_without_queueing() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (commands, receiver) = async_channel::bounded(1);
            let worker = tokio::spawn(async move {
                wait_to_retry(
                    &receiver,
                    std::time::Duration::from_secs(120),
                    "No desktop launcher",
                )
                .await
            });
            for _ in 0..8 {
                let (reply, response) = async_channel::bounded(1);
                commands
                    .try_send(ShortcutCommand::Configure(reply))
                    .unwrap();
                let result =
                    tokio::time::timeout(std::time::Duration::from_secs(1), response.recv())
                        .await
                        .unwrap()
                        .unwrap();
                assert_eq!(result, Err("No desktop launcher".into()));
            }
            drop(commands);
            assert!(
                !tokio::time::timeout(std::time::Duration::from_secs(1), worker)
                    .await
                    .unwrap()
                    .unwrap()
            );
        });
    }

    #[test]
    fn converts_gtk_accel_to_xdg_trigger() {
        assert_eq!(
            gtk_accel_to_xdg_trigger("<Ctrl><Alt>space").as_deref(),
            Some("CTRL+ALT+space")
        );
        assert_eq!(
            gtk_accel_to_xdg_trigger("<Primary><Shift>d").as_deref(),
            Some("CTRL+SHIFT+d")
        );
        assert_eq!(
            gtk_accel_to_xdg_trigger("<Super>k").as_deref(),
            Some("LOGO+k")
        );
    }

    #[test]
    fn rejects_non_accelerator() {
        assert_eq!(gtk_accel_to_xdg_trigger(""), None);
        assert_eq!(gtk_accel_to_xdg_trigger("<Ctrl>"), None);
        assert_eq!(gtk_accel_to_xdg_trigger("<Bogus>x"), None);
        for invalid in [
            "<Ctrl><Alt>",
            "<Ctrl>not_a_key",
            "Ctrl+Alt+space",
            "<Ctrl>two words",
            "<Ctrl>Shift_L",
            "<Ctrl>x<Alt>",
        ] {
            assert_eq!(gtk_accel_to_xdg_trigger(invalid), None, "{invalid}");
        }
    }
}
