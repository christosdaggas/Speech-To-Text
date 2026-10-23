// Copyright (C) 2026 Christos A. Daggas
// SPDX-License-Identifier: MIT

//! Speech to Text - A GTK4/Libadwaita offline speech-to-text application.
//!
//! Uses Whisper (via whisper.cpp) for local transcription with GPU acceleration support.

use gtk4::glib;
use gtk4::prelude::*;

mod api;
mod application;
mod audio;
mod benchmark;
mod cancel;
mod config;
mod config_writer;
mod diagnostics;
mod error;
mod fsio;
mod i18n;
mod limits;
mod llm;
mod portal;
mod recording;
mod secrets;
mod transcription;
mod tray;
mod ui;
mod version_check;

use application::Application;

/// Application ID for GNOME/Freedesktop
pub const APP_ID: &str = "com.chrisdaggas.speech-to-text";
pub const APP_NAME: &str = "Speech to Text";
/// Shared by the interface, local API and release packaging.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> glib::ExitCode {
    let worker_args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if worker_args
        .first()
        .is_some_and(|arg| arg == "--parakeet-worker")
    {
        return match transcription::parakeet::run_worker(&worker_args[1..]) {
            Ok(()) => glib::ExitCode::SUCCESS,
            Err(_) => glib::ExitCode::FAILURE,
        };
    }
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 2 && matches!(args[1].as_str(), "--version" | "-V") {
        println!("{APP_NAME} {VERSION}");
        return glib::ExitCode::SUCCESS;
    }
    if args.len() == 2 && args[1] == "--engine-version" {
        println!("whisper.cpp {}", whisper_rs::get_whisper_version());
        return glib::ExitCode::SUCCESS;
    }

    // Translations first, while the process is still single-threaded.
    // `setlocale` mutates process-global locale state and the environment with
    // no synchronisation (RUSTSEC-2026-0244), so it has to run before the Tokio
    // runtime below starts worker threads and before GTK is initialised.
    let locale_dir = option_env!("LOCALE_DIR").unwrap_or("/usr/share/locale");
    let ui_language = i18n::configured_language();
    // SAFETY: only a synchronous read has occurred; no other thread exists
    // yet, so nothing can read the locale or the environment concurrently.
    unsafe {
        if let Some(language) = ui_language.as_deref() {
            std::env::set_var("LANGUAGE", language);
        }
        gettextrs::setlocale(gettextrs::LocaleCategory::LcAll, "");
        if let Some(language) = ui_language.as_deref() {
            // glibc gettext also ignores LANGUAGE under C.UTF-8. A real
            // message locale is required; English can host any LANGUAGE.
            if gettextrs::setlocale(
                gettextrs::LocaleCategory::LcMessages,
                i18n::message_locale(language),
            )
            .is_none()
                && gettextrs::setlocale(gettextrs::LocaleCategory::LcMessages, "en_US.UTF-8")
                    .is_none()
            {
                eprintln!(
                    "Interface language requires a non-C message locale (for example en_US.UTF-8)."
                );
            }
        }
    }
    gettextrs::bindtextdomain("speech-to-text", locale_dir).ok();
    gettextrs::bind_textdomain_codeset("speech-to-text", "UTF-8").ok();
    gettextrs::textdomain("speech-to-text").ok();

    let runtime = application::tokio_runtime();
    let _guard = runtime.enter();

    // Set the program name (critical for Wayland window matching)
    glib::set_prgname(Some(APP_ID));
    glib::set_application_name(APP_NAME);

    // Initialize logging. Default: our crate at INFO, dependencies at WARN, so
    // routine logs stay readable and no third-party crate dumps request/response
    // detail. Transcript text and secrets are never logged at any level; set
    // RUST_LOG (e.g. `RUST_LOG=debug`) for verbose diagnostics when troubleshooting.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,speech_to_text=info"));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .init();
    tracing::info!("Starting {} v{}", APP_NAME, VERSION);

    // The autostart-only `--hidden` flag must NOT reach GApplication (FLAGS_NONE
    // would reject the unknown option), so detect it here and hand `run` an argv
    // without it. A manual launch (no flag) always shows the main window.
    if args.get(1).is_some_and(|arg| arg == "--benchmark") {
        return match benchmark::run(&args[2..]) {
            Ok(()) => glib::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{}", crate::error::redact_secrets(&error));
                glib::ExitCode::FAILURE
            }
        };
    }
    let hidden = args.iter().any(|a| a == "--hidden");
    let _ = application::LAUNCH_HIDDEN.set(hidden);
    let argv: Vec<String> = args.into_iter().filter(|a| a != "--hidden").collect();

    let app = Application::new();
    app.run_with_args(&argv)
}
