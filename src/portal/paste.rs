// Copyright (C) 2026 Christos A. Daggas
// SPDX-License-Identifier: MIT

//! One actor owns the RemoteDesktop session and continues serving clipboard
//! transfers after delivery. Open lazily: a live session shows GNOME's sharing
//! indicator. Request consent only from Settings, never over the target app.
//! Clipboard ownership and shortcut dispatch do not prove text insertion.
//! ydotool is a fallback; wtype's virtual-keyboard protocol is absent on Mutter.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, OnceLock,
};
use std::time::{Duration, Instant};

use crate::config::PasteShortcut;

use enumflags2::BitFlags;
use futures::StreamExt;
use tracing::{info, warn};

use ashpd::desktop::clipboard::{Clipboard, RequestClipboardOptions, SetSelectionOptions};
use ashpd::desktop::remote_desktop::{
    DeviceType, KeyState, NotifyKeyboardKeycodeOptions, RemoteDesktop, SelectDevicesOptions,
    StartOptions,
};
use ashpd::desktop::Session;
use ashpd::desktop::{CreateSessionOptions, PersistMode};

// Linux evdev keycodes, also used by ydotool. These describe physical keys.
const KEY_LEFTCTRL: i32 = 29;
const KEY_LEFTSHIFT: i32 = 42;
const KEY_V: i32 = 47;

/// Shared with the UI and carried through the actor queue. Expiry/cancellation
/// is checked again immediately before input, not just when the UI stops waiting.
#[derive(Clone, Debug)]
pub struct DeliveryRequest {
    cancelled: Arc<AtomicBool>,
    started: Arc<AtomicBool>,
    deadline: Instant,
}

impl Default for DeliveryRequest {
    fn default() -> Self {
        Self::new(Duration::from_secs(8))
    }
}

impl DeliveryRequest {
    fn new(lifetime: Duration) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            started: Arc::new(AtomicBool::new(false)),
            deadline: Instant::now() + lifetime,
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_current(&self) -> bool {
        !self.cancelled.load(Ordering::SeqCst) && Instant::now() < self.deadline
    }

    pub fn same_as(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.cancelled, &other.cancelled)
    }

    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    fn begin_injection(&self) -> bool {
        self.is_current()
            && self
                .started
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
    }

    async fn session(&self, live: &mut Option<LiveSession>) -> Result<Option<bool>, ashpd::Error> {
        if !self.is_current() {
            return Ok(None);
        }
        match tokio::time::timeout_at(self.deadline.into(), ensure_live(live, false)).await {
            Ok(result) => result,
            Err(_) => {
                self.cancel();
                Ok(None)
            }
        }
    }
}

/// How long to wait for the first clipboard read after sending the shortcut.
const FIRST_READ_TIMEOUT: Duration = Duration::from_millis(1500);
/// Quiet period after the last served read before a delivery returns.
const QUIET_PERIOD: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteHelper {
    /// XDG RemoteDesktop portal (preferred on GNOME/Wayland).
    RemoteDesktopPortal,
    /// External `ydotool` binary (needs daemon + uinput access).
    Ydotool,
    /// No automated paste available — clipboard only.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// The configured shortcut was sent. A clipboard reader is not proof that
    /// the intended editor inserted text (clipboard managers also read it).
    ShortcutSent,
    /// The selection is available, but input failed or was cancelled.
    ClipboardOnly,
    /// The portal path is unavailable (no grant yet, denied, no Clipboard
    /// portal, or a D-Bus failure). Nothing was injected; the caller must
    /// fall back to the GTK clipboard.
    Unavailable,
    /// Cancelled or expired before delivery. Do not retry in another window.
    Cancelled,
}

/// Decide which helper to use. Prefers the RemoteDesktop portal (always tried
/// first by [`try_autopaste`]); reports `Ydotool` only when the portal isn't an
/// option and `ydotool` is installed.
pub fn detect_paste_helper() -> PasteHelper {
    // The portal can't be cheaply probed without creating a session, so we
    // optimistically prefer it and degrade gracefully at use time.
    if remote_desktop_portal_likely() {
        PasteHelper::RemoteDesktopPortal
    } else if ydotool_available() {
        PasteHelper::Ydotool
    } else {
        PasteHelper::None
    }
}

/// Heuristic: a portal frontend is reachable (we're in a desktop session bus).
fn remote_desktop_portal_likely() -> bool {
    std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some()
        || std::env::var_os("XDG_RUNTIME_DIR").is_some()
}

pub fn ydotool_available() -> bool {
    binary_on_path("ydotool")
}

fn binary_on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

/// Whether the user has already granted RemoteDesktop access (a persisted
/// restore token exists). Without it, portal paste is skipped silently instead
/// of opening a consent dialog mid-delivery.
pub fn has_portal_grant() -> bool {
    load_restore_token().is_some()
}

enum Cmd {
    /// Own the selection with `text`, send the selected chord, report dispatch.
    Deliver {
        text: String,
        shortcut: PasteShortcut,
        request: DeliveryRequest,
        reply: async_channel::Sender<DeliveryOutcome>,
    },
    /// Send the selected chord (the current clipboard owner serves the text).
    Inject {
        shortcut: PasteShortcut,
        request: DeliveryRequest,
        reply: async_channel::Sender<DeliveryOutcome>,
    },
    /// Own the selection with `text` without injecting a keystroke.
    SetSelection {
        text: String,
        reply: async_channel::Sender<bool>,
    },
    /// Interactively acquire the RemoteDesktop grant (from Settings).
    AcquireGrant { reply: async_channel::Sender<bool> },
    /// Close the live session (after the user revokes the permission).
    CloseSession,
}

/// The portal's SelectionTransfer signal stream: (session, mime type, serial).
type TransferStream =
    std::pin::Pin<Box<dyn futures::Stream<Item = (Session<RemoteDesktop>, String, u32)> + Send>>;

struct LiveSession {
    proxy: &'static RemoteDesktop,
    clipboard: &'static Clipboard,
    session: Session<RemoteDesktop>,
    transfers: TransferStream,
    clipboard_enabled: bool,
}

/// The D-Bus proxies are created once and cached for the process lifetime:
/// the SelectionTransfer signal stream borrows the Clipboard proxy, so both
/// must outlive every session that gets recreated on top of them.
async fn proxies() -> Result<(&'static RemoteDesktop, &'static Clipboard), ashpd::Error> {
    static REMOTE: tokio::sync::OnceCell<RemoteDesktop> = tokio::sync::OnceCell::const_new();
    static CLIPBOARD: tokio::sync::OnceCell<Clipboard> = tokio::sync::OnceCell::const_new();
    let remote = REMOTE.get_or_try_init(RemoteDesktop::new).await?;
    let clipboard = CLIPBOARD.get_or_try_init(Clipboard::new).await?;
    Ok((remote, clipboard))
}

fn sender() -> &'static async_channel::Sender<Cmd> {
    static TX: OnceLock<async_channel::Sender<Cmd>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = async_channel::bounded::<Cmd>(16);
        crate::application::tokio_runtime().spawn(actor(rx));
        tx
    })
}

enum Woken {
    Cmd(Cmd),
    Transfer(Option<(Session<RemoteDesktop>, String, u32)>),
    Closed,
}

async fn actor(rx: async_channel::Receiver<Cmd>) {
    let mut live: Option<LiveSession> = None;
    // The bytes served for every SelectionTransfer while we own the selection.
    let mut current: Vec<u8> = Vec::new();
    loop {
        let woken = if let Some(l) = live.as_mut() {
            tokio::select! {
                c = rx.recv() => match c {
                    Ok(c) => Woken::Cmd(c),
                    Err(_) => Woken::Closed,
                },
                t = l.transfers.next() => Woken::Transfer(t),
            }
        } else {
            match rx.recv().await {
                Ok(c) => Woken::Cmd(c),
                Err(_) => Woken::Closed,
            }
        };
        match woken {
            Woken::Closed => break,
            Woken::Transfer(None) => {
                // Signal stream ended: the portal went away. Recreate lazily.
                warn!("RemoteDesktop clipboard stream ended; session dropped");
                live = None;
            }
            Woken::Transfer(Some((sess, _mime, serial))) => {
                if let Some(clipboard) = live.as_ref().map(|l| l.clipboard) {
                    serve_transfer(clipboard, &sess, serial, &current).await;
                }
            }
            Woken::Cmd(cmd) => handle_cmd(cmd, &mut live, &mut current).await,
        }
    }
}

async fn handle_cmd(cmd: Cmd, live: &mut Option<LiveSession>, current: &mut Vec<u8>) {
    match cmd {
        Cmd::Deliver {
            text,
            shortcut,
            request,
            reply,
        } => {
            let mut outcome = DeliveryOutcome::Unavailable;
            // Recreate a stale session only before any input has been attempted.
            for _ in 0..2 {
                if !request.is_current() || reply.is_closed() {
                    outcome = DeliveryOutcome::Cancelled;
                    break;
                }
                match request.session(live).await {
                    Ok(Some(_)) if request.is_current() => {
                        let l = live.as_mut().expect("session was ensured");
                        if !l.clipboard_enabled {
                            break;
                        }
                        match tokio::time::timeout_at(
                            request.deadline.into(),
                            set_selection(l.clipboard, &l.session),
                        )
                        .await
                        {
                            Ok(Ok(())) => {
                                *current = text.as_bytes().to_vec();
                                outcome =
                                    match inject_shortcut(l.proxy, &l.session, shortcut, &request)
                                        .await
                                    {
                                        InjectionResult::Sent => DeliveryOutcome::ShortcutSent,
                                        _ => DeliveryOutcome::ClipboardOnly,
                                    };
                                // Serve the immediate read before re-showing the panel.
                                // A read is not evidence of insertion in the target app.
                                if outcome == DeliveryOutcome::ShortcutSent {
                                    serve_until_quiet(l, current, FIRST_READ_TIMEOUT).await;
                                }
                                break;
                            }
                            Ok(Err(e)) => {
                                warn!("Portal selection failed ({e}); recreating session");
                                close_live(live).await;
                            }
                            Err(_) => {
                                request.cancel();
                                outcome = DeliveryOutcome::Cancelled;
                                break;
                            }
                        }
                    }
                    Ok(_) => break,
                    Err(e) => {
                        warn!("RemoteDesktop session error: {e}");
                        close_live(live).await;
                        break;
                    }
                }
            }
            if !request.is_current() && outcome == DeliveryOutcome::Unavailable {
                outcome = DeliveryOutcome::Cancelled;
            }
            info!("Portal delivery outcome: {outcome:?}");
            let _ = reply.send(outcome).await;
        }
        Cmd::Inject {
            shortcut,
            request,
            reply,
        } => {
            let outcome = match request.session(live).await {
                Ok(Some(_)) if request.is_current() => {
                    let l = live.as_ref().expect("session was ensured");
                    match inject_shortcut(l.proxy, &l.session, shortcut, &request).await {
                        InjectionResult::Sent => DeliveryOutcome::ShortcutSent,
                        InjectionResult::Cancelled => DeliveryOutcome::Cancelled,
                        // Never fall back after uncertain input: V may already
                        // have reached the destination, despite a D-Bus error.
                        InjectionResult::Failed => DeliveryOutcome::ClipboardOnly,
                    }
                }
                Err(e) => {
                    warn!("RemoteDesktop session error: {e}");
                    close_live(live).await;
                    DeliveryOutcome::Unavailable
                }
                _ if !request.is_current() => DeliveryOutcome::Cancelled,
                _ => DeliveryOutcome::Unavailable,
            };
            let _ = reply.send(outcome).await;
        }
        Cmd::SetSelection { text, reply } => {
            *current = text.into_bytes();
            let mut ok = false;
            for _ in 0..2 {
                match ensure_live(live, false).await {
                    Ok(Some(_)) => {
                        let (clipboard, session, clipboard_enabled) = {
                            let l = live.as_ref().expect("ensured above");
                            (l.clipboard, &l.session, l.clipboard_enabled)
                        };
                        if !clipboard_enabled {
                            break;
                        }
                        match set_selection(clipboard, session).await {
                            Ok(()) => {
                                ok = true;
                                break;
                            }
                            Err(e) => {
                                warn!("Portal SetSelection failed ({e}); recreating session");
                                close_live(live).await;
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        warn!("RemoteDesktop session error: {e}");
                        close_live(live).await;
                        break;
                    }
                }
            }
            let _ = reply.send(ok).await;
        }
        Cmd::AcquireGrant { reply } => {
            let ok = match ensure_live(live, true).await {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(e) => {
                    warn!("RemoteDesktop grant request failed: {e}");
                    false
                }
            };
            let _ = reply.send(ok).await;
        }
        Cmd::CloseSession => {
            close_live(live).await;
        }
    }
}

/// Make sure a live session exists. Returns `Ok(Some(fresh))` with `fresh`
/// telling whether the session was just created, `Ok(None)` when the portal is
/// not an option (no grant in non-interactive mode, or keyboard denied).
async fn ensure_live(
    live: &mut Option<LiveSession>,
    interactive: bool,
) -> Result<Option<bool>, ashpd::Error> {
    if live.is_some() {
        return Ok(Some(false));
    }
    if !interactive && !has_portal_grant() {
        info!("No RemoteDesktop grant yet — portal paste skipped (grant it from Settings)");
        return Ok(None);
    }
    match open_session().await? {
        Some(l) => {
            *live = Some(l);
            Ok(Some(true))
        }
        None => Ok(None),
    }
}

/// A cancelled session handshake must not leave its consent dialog/session alive.
struct OpeningSession(Option<Session<RemoteDesktop>>);

impl Drop for OpeningSession {
    fn drop(&mut self) {
        if let Some(session) = self.0.take() {
            crate::application::tokio_runtime().spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(1), session.close()).await;
            });
        }
    }
}

async fn open_session() -> Result<Option<LiveSession>, ashpd::Error> {
    super::ensure_host_app_registered().await?;

    let (proxy, clipboard) = proxies().await?;
    let session = proxy
        .create_session(CreateSessionOptions::default())
        .await?;

    let mut opening = OpeningSession(Some(session));
    let session = opening.0.as_ref().expect("newly created session");

    let restore_token = load_restore_token();
    let select = SelectDevicesOptions::default()
        .set_devices(BitFlags::from(DeviceType::Keyboard))
        .set_persist_mode(PersistMode::ExplicitlyRevoked)
        .set_restore_token(restore_token.as_deref());
    proxy.select_devices(session, select).await?.response()?;

    // RequestClipboard must be called after SelectDevices and before Start.
    clipboard
        .request(session, RequestClipboardOptions::default())
        .await?;

    let started = match proxy
        .start(session, None, StartOptions::default())
        .await?
        .response()
    {
        Ok(started) => started,
        Err(e) => {
            // The consent dialog was cancelled/denied. Restore tokens are
            // single-use: the token we sent (if any) was consumed by this
            // failed Start, so keeping the file would make has_portal_grant()
            // lie and every later paste/copy/warm-up re-open the dialog in an
            // endless loop. Forget it — the user can re-grant from Settings.
            warn!("RemoteDesktop start denied/cancelled: {e}");
            forget_restore_token();
            let _ = session.close().await;
            return match e {
                ashpd::Error::Response(_) => Ok(None),
                other => Err(other),
            };
        }
    };

    if !started.devices().contains(DeviceType::Keyboard) {
        warn!("RemoteDesktop session did not grant keyboard access");
        // Same reasoning as the denial above: the grant behind the persisted
        // token no longer covers the keyboard, so the token is dead weight.
        forget_restore_token();
        let _ = session.close().await;
        return Ok(None);
    }
    // Persist the (possibly refreshed) restore token so consent is shown once.
    if let Some(token) = started.restore_token() {
        save_restore_token(token);
    }
    let clipboard_enabled = started.is_clipboard_enabled();

    // Subscribe to transfer requests BEFORE ever advertising a selection so we
    // don't miss reads issued in response to the paste shortcut.
    let transfers = clipboard
        .receive_selection_transfer::<RemoteDesktop>()
        .await?;

    info!("RemoteDesktop session established (clipboard={clipboard_enabled})");
    Ok(Some(LiveSession {
        proxy,
        clipboard,
        session: opening.0.take().expect("completed session handshake"),
        transfers: Box::pin(transfers),
        clipboard_enabled,
    }))
}

async fn close_live(live: &mut Option<LiveSession>) {
    if let Some(l) = live.take() {
        let _ = l.session.close().await;
    }
}

async fn set_selection(
    clipboard: &Clipboard,
    session: &Session<RemoteDesktop>,
) -> Result<(), ashpd::Error> {
    clipboard
        .set_selection(
            session,
            SetSelectionOptions::default()
                .set_mime_types(&["text/plain;charset=utf-8", "text/plain"]),
        )
        .await
}

#[derive(Debug, PartialEq, Eq)]
enum InjectionResult {
    Sent,
    Cancelled,
    Failed,
}

fn shortcut_keys(shortcut: PasteShortcut) -> &'static [i32] {
    match shortcut {
        PasteShortcut::CtrlV => &[KEY_LEFTCTRL, KEY_V],
        PasteShortcut::CtrlShiftV => &[KEY_LEFTCTRL, KEY_LEFTSHIFT, KEY_V],
    }
}

/// Always release every attempted key, including a press whose acknowledgement
/// failed. Cancellation stops further presses but must not leave Ctrl held down.
async fn send_chord<F, Fut>(
    shortcut: PasteShortcut,
    request: &DeliveryRequest,
    mut notify: F,
) -> InjectionResult
where
    F: FnMut(i32, bool) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    if !request.begin_injection() {
        return InjectionResult::Cancelled;
    }
    let mut pressed = Vec::with_capacity(3);
    let mut result = InjectionResult::Sent;
    for &key in shortcut_keys(shortcut) {
        if !request.is_current() {
            result = InjectionResult::Cancelled;
            break;
        }
        pressed.push(key);
        if !notify(key, true).await {
            result = InjectionResult::Failed;
            break;
        }
    }
    for key in pressed.into_iter().rev() {
        if !notify(key, false).await {
            result = InjectionResult::Failed;
        }
    }
    result
}

async fn inject_shortcut(
    proxy: &RemoteDesktop,
    session: &Session<RemoteDesktop>,
    shortcut: PasteShortcut,
    request: &DeliveryRequest,
) -> InjectionResult {
    send_chord(shortcut, request, |key, pressed| async move {
        matches!(
            tokio::time::timeout(
                Duration::from_secs(1),
                proxy.notify_keyboard_keycode(
                    session,
                    key,
                    if pressed {
                        KeyState::Pressed
                    } else {
                        KeyState::Released
                    },
                    NotifyKeyboardKeycodeOptions::default(),
                ),
            )
            .await,
            Ok(Ok(()))
        )
    })
    .await
}

/// Serve SelectionTransfer requests: wait up to `first` for the first read,
/// then only [`QUIET_PERIOD`] for follow-ups. Returns whether any was served.
/// Later reads (clipboard managers, repeat pastes) are handled by the actor's
/// main loop, so this only needs to confirm the immediate paste.
async fn serve_until_quiet(l: &mut LiveSession, bytes: &[u8], first: Duration) -> bool {
    let mut served_any = false;
    loop {
        let wait = if served_any { QUIET_PERIOD } else { first };
        match tokio::time::timeout(wait, l.transfers.next()).await {
            Ok(Some((sess, _mime, serial))) => {
                if serve_transfer(l.clipboard, &sess, serial, bytes).await {
                    served_any = true;
                }
            }
            Ok(None) => break, // signal stream ended — actor will recreate
            Err(_) => break,   // window elapsed
        }
    }
    served_any
}

/// Answer one SelectionTransfer with `bytes`. Returns whether the write went
/// through.
async fn serve_transfer(
    clipboard: &Clipboard,
    sess: &Session<RemoteDesktop>,
    serial: u32,
    bytes: &[u8],
) -> bool {
    use std::io::Write;
    match clipboard.selection_write(sess, serial).await {
        Ok(zfd) => {
            let std_fd: std::os::fd::OwnedFd = zfd.into();
            let mut f = std::fs::File::from(std_fd);
            let ok = f.write_all(bytes).and_then(|_| f.flush()).is_ok();
            drop(f); // closes the pipe write end
            let _ = clipboard.selection_write_done(sess, serial, ok).await;
            ok
        }
        Err(e) => {
            warn!("selection_write failed: {e}");
            false
        }
    }
}

/// Deliver `text` into the currently-focused app: the portal owns the system
/// selection (focus-independent) and the configured shortcut is sent. The caller must
/// ensure the target app holds keyboard focus (i.e. hide the panel first).
/// See [`DeliveryOutcome`] for the honest result semantics.
#[tracing::instrument(name = "portal.deliver_text", skip(text))]
pub async fn deliver_text_via_portal(
    text: String,
    shortcut: PasteShortcut,
    request: DeliveryRequest,
) -> DeliveryOutcome {
    let (tx, rx) = async_channel::bounded(1);
    if sender()
        .send(Cmd::Deliver {
            text,
            shortcut,
            request,
            reply: tx,
        })
        .await
        .is_err()
    {
        return DeliveryOutcome::Unavailable;
    }
    rx.recv().await.unwrap_or(DeliveryOutcome::Unavailable)
}

/// Own the system selection with `text` WITHOUT injecting a keystroke — a
/// focus-independent "copy to clipboard". Returns whether the portal now owns
/// the selection; on `false` the caller must set the GTK clipboard itself
/// (which requires its surface to be focused on Wayland).
#[tracing::instrument(name = "portal.set_clipboard", skip(text))]
pub async fn set_clipboard_via_portal(text: String) -> bool {
    let (tx, rx) = async_channel::bounded(1);
    if sender()
        .send(Cmd::SetSelection { text, reply: tx })
        .await
        .is_err()
    {
        return false;
    }
    rx.recv().await.unwrap_or(false)
}

/// Attempt to paste the current clipboard contents into the focused app.
/// Reports dispatch without claiming that another app inserted the text.
#[tracing::instrument(name = "portal.autopaste")]
pub async fn try_autopaste(shortcut: PasteShortcut, request: DeliveryRequest) -> DeliveryOutcome {
    let (tx, rx) = async_channel::bounded(1);
    let outcome = if sender()
        .send(Cmd::Inject {
            shortcut,
            request: request.clone(),
            reply: tx,
        })
        .await
        .is_ok()
    {
        rx.recv().await.unwrap_or(DeliveryOutcome::Unavailable)
    } else {
        DeliveryOutcome::Unavailable
    };
    if outcome != DeliveryOutcome::Unavailable || !request.is_current() {
        return outcome;
    }
    if ydotool_available() {
        return tokio::task::spawn_blocking(move || paste_via_ydotool(shortcut, request))
            .await
            .unwrap_or(DeliveryOutcome::ClipboardOnly);
    }
    info!("Auto-paste unavailable");
    DeliveryOutcome::Unavailable
}

/// Interactively request the RemoteDesktop grant (shows the consent dialog if
/// not yet granted). Called from Settings when the user enables auto-paste so
/// the dialog never interrupts a paste. Returns whether the grant is held.
pub async fn acquire_permission_interactive() -> bool {
    let (tx, rx) = async_channel::bounded(1);
    if sender()
        .send(Cmd::AcquireGrant { reply: tx })
        .await
        .is_err()
    {
        return false;
    }
    rx.recv().await.unwrap_or(false)
}

/// The fallback uses the same chord, cancellation and key-release rules.
fn paste_via_ydotool(shortcut: PasteShortcut, request: DeliveryRequest) -> DeliveryOutcome {
    let result =
        futures::executor::block_on(send_chord(shortcut, &request, |key, pressed| async move {
            let arg = format!("{key}:{}", u8::from(pressed));
            let Ok(mut child) = std::process::Command::new("ydotool")
                .args(["key", &arg])
                .spawn()
            else {
                return false;
            };
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => return status.success(),
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return false;
                    }
                }
            }
        }));
    match result {
        InjectionResult::Sent => DeliveryOutcome::ShortcutSent,
        InjectionResult::Cancelled => DeliveryOutcome::Cancelled,
        InjectionResult::Failed => DeliveryOutcome::ClipboardOnly,
    }
}

fn restore_token_path() -> std::path::PathBuf {
    crate::config::AppConfig::config_dir().join("remote_desktop.token")
}

fn load_restore_token() -> Option<String> {
    std::fs::read_to_string(restore_token_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn save_restore_token(token: &str) {
    // The restore token lets us re-acquire input-injection permission without a
    // prompt, so it is sensitive: write it privately (0600) and atomically.
    if let Err(e) = crate::fsio::write_private(&restore_token_path(), token.as_bytes()) {
        warn!("Failed to persist RemoteDesktop restore token: {e}");
    }
}

/// Quietly delete a restore token that turned out to be dead (denied/expired
/// grant), so `has_portal_grant()` stops claiming a grant that no longer works.
fn forget_restore_token() {
    match std::fs::remove_file(restore_token_path()) {
        Ok(()) => info!("Forgot stale RemoteDesktop restore token"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!("Failed to remove stale restore token: {e}"),
    }
}

/// Delete the persisted RemoteDesktop restore token and close any live portal
/// session. After this, the next auto-paste falls back to the clipboard until
/// the permission is granted again from Settings. Returns `true` if a token
/// was removed (or none existed); `false` only on an unexpected I/O error.
pub fn revoke_restore_token() -> bool {
    // Best-effort: also tear down the live session so the grant stops working
    // immediately, not just after restart.
    let _ = sender().try_send(Cmd::CloseSession);
    let path = restore_token_path();
    match std::fs::remove_file(&path) {
        Ok(()) => {
            info!("Removed RemoteDesktop restore token");
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            warn!("Failed to remove RemoteDesktop restore token: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::future::ready;

    #[test]
    fn physical_shortcuts_press_and_release_in_order() {
        for (shortcut, expected) in [
            (
                PasteShortcut::CtrlV,
                vec![(29, true), (47, true), (47, false), (29, false)],
            ),
            (
                PasteShortcut::CtrlShiftV,
                vec![
                    (29, true),
                    (42, true),
                    (47, true),
                    (47, false),
                    (42, false),
                    (29, false),
                ],
            ),
        ] {
            let events = RefCell::new(Vec::new());
            let result = futures::executor::block_on(send_chord(
                shortcut,
                &DeliveryRequest::default(),
                |key, pressed| {
                    events.borrow_mut().push((key, pressed));
                    ready(true)
                },
            ));
            assert_eq!(result, InjectionResult::Sent);
            assert_eq!(*events.borrow(), expected);
        }
    }

    #[test]
    fn cancelled_or_expired_jobs_send_no_keys() {
        let cancelled = DeliveryRequest::default();
        cancelled.cancel();
        for request in [cancelled, DeliveryRequest::new(Duration::ZERO)] {
            let result =
                futures::executor::block_on(send_chord(PasteShortcut::CtrlV, &request, |_, _| {
                    panic!("cancelled or expired request must not send keys");
                    #[allow(unreachable_code)]
                    ready(true)
                }));
            assert_eq!(result, InjectionResult::Cancelled);
        }
    }

    #[test]
    fn cancellation_between_modifiers_releases_without_pasting() {
        let request = DeliveryRequest::default();
        let events = RefCell::new(Vec::new());
        let result = futures::executor::block_on(send_chord(
            PasteShortcut::CtrlShiftV,
            &request,
            |key, pressed| {
                events.borrow_mut().push((key, pressed));
                if key == KEY_LEFTSHIFT && pressed {
                    request.cancel();
                }
                ready(true)
            },
        ));
        assert_eq!(result, InjectionResult::Cancelled);
        assert_eq!(
            *events.borrow(),
            vec![(29, true), (42, true), (42, false), (29, false)]
        );
    }

    #[test]
    fn failed_press_releases_all_attempted_keys_and_cannot_retry() {
        let request = DeliveryRequest::default();
        let events = RefCell::new(Vec::new());
        let result = futures::executor::block_on(send_chord(
            PasteShortcut::CtrlShiftV,
            &request,
            |key, pressed| {
                events.borrow_mut().push((key, pressed));
                ready(!(key == KEY_V && pressed))
            },
        ));
        assert_eq!(result, InjectionResult::Failed);
        assert_eq!(
            *events.borrow(),
            vec![
                (29, true),
                (42, true),
                (47, true),
                (47, false),
                (42, false),
                (29, false)
            ]
        );
        let second =
            futures::executor::block_on(send_chord(PasteShortcut::CtrlShiftV, &request, |_, _| {
                panic!("uncertain input must never be retried");
                #[allow(unreachable_code)]
                ready(true)
            }));
        assert_eq!(second, InjectionResult::Cancelled);
    }

    #[test]
    fn failed_release_does_not_skip_remaining_modifier_cleanup() {
        let events = RefCell::new(Vec::new());
        let result = futures::executor::block_on(send_chord(
            PasteShortcut::CtrlV,
            &DeliveryRequest::default(),
            |key, pressed| {
                events.borrow_mut().push((key, pressed));
                ready(key != KEY_V || pressed)
            },
        ));
        assert_eq!(result, InjectionResult::Failed);
        assert_eq!(events.borrow().last(), Some(&(KEY_LEFTCTRL, false)));
    }

    #[test]
    fn actor_discards_expired_delivery_without_changing_clipboard() {
        let (tx, rx) = async_channel::bounded(1);
        let mut current = b"existing clipboard".to_vec();
        let mut live = None;
        futures::executor::block_on(handle_cmd(
            Cmd::Deliver {
                text: "stale transcript".into(),
                shortcut: PasteShortcut::CtrlV,
                request: DeliveryRequest::new(Duration::ZERO),
                reply: tx,
            },
            &mut live,
            &mut current,
        ));
        assert_eq!(rx.try_recv().unwrap(), DeliveryOutcome::Cancelled);
        assert_eq!(current, b"existing clipboard");
        assert!(live.is_none());
    }

    #[test]
    fn successful_delivery_cannot_be_sent_twice() {
        let request = DeliveryRequest::default();
        assert!(request.begin_injection());
        assert!(!request.clone().begin_injection());
        assert!(request.same_as(&request.clone()));
        assert!(!request.same_as(&DeliveryRequest::default()));
    }
}
