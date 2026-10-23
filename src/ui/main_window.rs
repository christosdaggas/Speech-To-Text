// Copyright (C) 2026 Christos A. Daggas
// SPDX-License-Identifier: MIT

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{gio, glib};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use crate::application::Application;
use crate::config::{AppConfig, LlmPreset};
use crate::i18n::gettext;
use crate::recording::{DictationMode, DictationParams, RecordingController, RecordingOwner};
use crate::transcription::postprocess;
use crate::transcription::ModelCatalog;
use crate::ui::settings::{
    fill_preferences_width, language_code_to_name, ApiPage, DictationPage, DictionaryPage,
    LanguagePage, LlmPage, MicrophonePage, ModelPage, PerformancePage,
};
use crate::ui::widgets::GpuStatusPanel;
use crate::ui::{
    ControlAction, Controls, HeaderControls, HelpPage, HistoryPage, StatusBar, TranscriptView,
    WelcomeWizard,
};

/// Whisper inference parameters gathered from the settings widgets, shared by
/// the live-recording and file-drop transcription paths.
struct WhisperRunParams {
    n_threads: u32,
    beam_size: u32,
    temperature: f32,
    translate: bool,
    initial_prompt: Option<String>,
}

#[derive(Clone)]
enum RetryAudio {
    Mic(crate::audio::buffer::RawAudioSnapshot),
    File(Arc<crate::audio::file_decoder::DecodedAudio>),
}

#[derive(Clone)]
struct WindowRetry {
    audio: RetryAudio,
    params: DictationParams,
    duration: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavItem {
    Transcription,
    History,
    Recognition,
    DictationAudio,
    Microphone,
    Model,
    Language,
    Performance,
    Dictation,
    Dictionary,
    Llm,
    Api,
    Help,
}

#[cfg(test)]
mod navigation_title_tests {
    use super::NavItem;

    #[test]
    fn only_transcription_omits_the_header_title() {
        assert!(NavItem::Transcription.header_title().is_empty());
        for item in NavItem::all() {
            if !matches!(item, NavItem::Transcription) {
                assert_eq!(item.header_title(), item.title());
            }
        }
    }
}

impl NavItem {
    pub fn icon_name(&self) -> &'static str {
        match self {
            Self::Transcription => "audio-input-microphone-symbolic",
            Self::History => "document-open-recent-symbolic",
            Self::Recognition => "system-software-install-symbolic",
            Self::DictationAudio => "audio-card-symbolic",
            Self::Microphone => "audio-card-symbolic",
            Self::Model => "system-software-install-symbolic",
            Self::Language => "preferences-desktop-locale-symbolic",
            Self::Performance => "preferences-system-symbolic",
            Self::Dictation => "input-keyboard-symbolic",
            Self::Dictionary => "accessories-dictionary-symbolic",
            Self::Llm => "network-transmit-receive-symbolic",
            Self::Api => "network-server-symbolic",
            Self::Help => "help-about-symbolic",
        }
    }

    pub fn title(&self) -> String {
        match self {
            Self::Transcription => gettext("Transcription"),
            Self::History => gettext("History"),
            Self::Recognition => gettext("Recognition"),
            Self::DictationAudio => gettext("Dictation & Audio"),
            Self::Microphone => gettext("Microphone"),
            Self::Model => gettext("Model"),
            Self::Language => gettext("Language"),
            Self::Performance => gettext("Performance"),
            Self::Dictation => gettext("Dictation"),
            Self::Dictionary => gettext("Dictionary"),
            Self::Llm => gettext("LLM"),
            Self::Api => gettext("API"),
            Self::Help => gettext("Help"),
        }
    }

    pub fn is_settings(&self) -> bool {
        matches!(
            self,
            Self::Recognition
                | Self::DictationAudio
                | Self::Microphone
                | Self::Model
                | Self::Language
                | Self::Performance
                | Self::Dictation
                | Self::Dictionary
                | Self::Llm
                | Self::Api
        )
    }

    fn header_title(&self) -> String {
        if matches!(self, Self::Transcription) {
            String::new()
        } else {
            self.title()
        }
    }

    /// Legacy page destinations still select their tab inside the new group.
    fn sidebar_item(self) -> Self {
        match self {
            Self::Model | Self::Language | Self::Performance => Self::Recognition,
            Self::Microphone | Self::Dictation => Self::DictationAudio,
            item => item,
        }
    }

    pub fn all() -> &'static [NavItem] {
        &[
            Self::Transcription,
            Self::History,
            Self::Recognition,
            Self::DictationAudio,
            Self::Dictionary,
            Self::Llm,
            Self::Api,
            Self::Help,
        ]
    }
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct MainWindow {
        pub work_cancel: RefCell<crate::cancel::CancelToken>,
        pub(super) retry_take: RefCell<Option<super::WindowRetry>>,
        pub retry_banner: RefCell<Option<adw::Banner>>,
        pub toast_overlay: RefCell<Option<adw::ToastOverlay>>,
        /// Top-of-content notification bar (themed, in-window) — replaces the
        /// bottom OSD toasts so messages are easy to see and match the theme.
        pub notification_banner: RefCell<Option<adw::Banner>>,
        pub sidebar_box: RefCell<Option<gtk::Box>>,
        pub sidebar_list: RefCell<Option<gtk::ListBox>>,
        pub content_stack: RefCell<Option<gtk::Stack>>,
        pub recognition_stack: RefCell<Option<gtk::Stack>>,
        pub dictation_audio_stack: RefCell<Option<gtk::Stack>>,
        pub header_controls: RefCell<Option<HeaderControls>>,
        pub current_nav: Cell<Option<NavItem>>,
        pub nav_labels: RefCell<Vec<gtk::Label>>,
        pub nav_boxes: RefCell<Vec<gtk::Box>>,
        pub sidebar_collapsed: Cell<bool>,
        pub sidebar_resize_origin: Cell<Option<(i32, i32, i32)>>,
        pub sidebar_toggle_btn: RefCell<Option<gtk::Button>>,
        pub sidebar_title_box: RefCell<Option<gtk::Box>>,
        pub settings_header_label: RefCell<Option<gtk::Label>>,
        pub sidebar_section_rows: RefCell<Vec<gtk::ListBoxRow>>,
        pub info_box: RefCell<Option<gtk::Box>>,
        pub gpu_panel: RefCell<Option<GpuStatusPanel>>,
        pub syncing_backend: Cell<bool>,
        pub syncing_dropdown: Cell<bool>,
        pub syncing_translate: Cell<bool>,
        pub syncing_ai: Cell<bool>,
        /// Guards the live (while-speaking) decode so ticks don't pile up.
        pub live_decoding: Cell<bool>,
        /// True while recording a Voice-edit instruction in the main window.
        pub voice_editing: Cell<bool>,
        /// Set when a live decode was too slow or failed; disables the live loop
        /// for the rest of this recording so it can't flood a slow/VRAM-tight GPU.
        pub live_too_slow: Cell<bool>,
        /// Monotonic ID used to reject callbacks from cancelled or superseded
        /// recording, file-import, and LLM operations.
        pub operation_generation: Cell<u64>,
        /// Only the latest model-load request may publish an engine/UI result.
        pub model_load_generation: Cell<u64>,

        pub transcript_view: RefCell<Option<TranscriptView>>,
        pub controls: RefCell<Option<Controls>>,
        pub status_bar: RefCell<Option<StatusBar>>,
        pub history_page: RefCell<Option<HistoryPage>>,
        pub help_page: RefCell<Option<HelpPage>>,
        pub microphone_page: RefCell<Option<MicrophonePage>>,
        pub model_page: RefCell<Option<ModelPage>>,
        pub language_page: RefCell<Option<LanguagePage>>,
        pub performance_page: RefCell<Option<PerformancePage>>,
        pub dictation_page: RefCell<Option<DictationPage>>,
        pub dictionary_page: RefCell<Option<DictionaryPage>>,
        pub llm_page: RefCell<Option<LlmPage>>,
        pub api_page: RefCell<Option<ApiPage>>,
        /// One result per dictation (raw transcript + AI variants). The transcript
        /// view shows one selectable bubble per entry; `selected_message` is the
        /// one the chips / Improve / Voice edit act on. Append-only + clear-all,
        /// so indices stay in sync with the transcript view's bubbles.
        pub messages: RefCell<Vec<crate::ui::result_state::ResultState>>,
        pub selected_message: Cell<isize>,

        pub config: RefCell<Option<Arc<AppConfig>>>,
        /// Shared recording + transcription controller (owned by the Application).
        pub controller: RefCell<Option<Rc<RecordingController>>>,
        pub engine: RefCell<Option<crate::recording::SharedEngine>>,
        pub model_catalog: RefCell<Option<Arc<ModelCatalog>>>,
        /// Last transcription segments for SRT export: (start_ms, end_ms, text).
        pub last_segments: RefCell<Vec<(i64, i64, String)>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MainWindow {
        const NAME: &'static str = "SpeechToTextMainWindow";
        type Type = super::MainWindow;
        type ParentType = adw::ApplicationWindow;
    }

    impl ObjectImpl for MainWindow {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.setup_ui();
            obj.setup_actions();
        }
    }

    impl WidgetImpl for MainWindow {}
    impl WindowImpl for MainWindow {}
    impl ApplicationWindowImpl for MainWindow {}
    impl AdwApplicationWindowImpl for MainWindow {}
}

glib::wrapper! {
    pub struct MainWindow(ObjectSubclass<imp::MainWindow>)
        @extends gtk::Widget, gtk::Window, gtk::ApplicationWindow, adw::ApplicationWindow,
        @implements gio::ActionGroup, gio::ActionMap;
}

impl MainWindow {
    // Initial window size. The sidebar starts collapsed, so the content area
    // gets WINDOW_WIDTH - SIDEBAR_COLLAPSED_WIDTH (74) at launch. The window
    // stays resizable; this is only the initial size.
    const WINDOW_WIDTH: i32 = 960;
    const WINDOW_HEIGHT: i32 = 680;
    /// Minimum width of the right column (transcript area). The window is now
    /// resizable, so `set_default_size` drives the initial 960px width and this
    /// is only the floor below which the right column won't shrink. Kept modest
    /// so the user can also drag the window smaller; the sidebar stays fixed.
    const CONTENT_MIN_WIDTH: i32 = 560;
    const SIDEBAR_EXPANDED_WIDTH: i32 = 280;
    const SIDEBAR_COLLAPSED_WIDTH: i32 = 74;

    fn begin_operation(&self) -> u64 {
        self.imp().retry_take.borrow_mut().take();
        self.show_retry(false);
        self.imp().work_cancel.borrow().abort();
        *self.imp().work_cancel.borrow_mut() = crate::cancel::CancelToken::new();
        let generation = self.imp().operation_generation.get().wrapping_add(1);
        self.imp().operation_generation.set(generation);
        generation
    }

    fn operation_is_current(&self, generation: u64) -> bool {
        self.imp().operation_generation.get() == generation
    }

    fn show_retry(&self, visible: bool) {
        if let Some(banner) = self.imp().retry_banner.borrow().as_ref() {
            banner.set_revealed(visible);
        }
    }

    fn retry_last_audio(&self) {
        let Some(controller) = self.controller() else {
            return;
        };
        if controller.is_recording() {
            return;
        }
        let Some(take) = self.imp().retry_take.borrow().clone() else {
            return;
        };
        let generation = self.begin_operation();
        *self.imp().retry_take.borrow_mut() = Some(take.clone());
        if let Some(tv) = self.imp().transcript_view.borrow().as_ref() {
            tv.start_transcribing_anim();
        }
        let cancel = self.imp().work_cancel.borrow().clone();
        let receiver = match take.audio {
            RetryAudio::Mic(audio) => {
                controller.transcribe_snapshot_async(audio, take.params, take.duration, cancel)
            }
            RetryAudio::File(audio) => {
                let (sender, receiver) = async_channel::bounded(1);
                let engine = controller.engine_arc();
                std::thread::spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        crate::recording::run_transcription(
                            &engine,
                            &audio.samples,
                            &take.params,
                            take.duration,
                            Some(cancel),
                        )
                        .map(|mut outcome| {
                            outcome.shift_timestamps(audio.leading_offset_ms);
                            outcome
                        })
                    }))
                    .unwrap_or_else(|_| Err("Transcription worker failed".into()));
                    let _ = sender.send_blocking(result);
                });
                receiver
            }
        };
        let weak = self.downgrade();
        glib::spawn_future_local(async move {
            let result = receiver
                .recv()
                .await
                .unwrap_or_else(|_| Err("Transcription worker unavailable".into()));
            if let Some(window) = weak.upgrade() {
                if window.operation_is_current(generation) {
                    window.apply_transcription_result(result);
                }
            }
        });
    }

    fn sync_theme_class(&self) {
        self.remove_css_class("light-theme");
        self.remove_css_class("dark-theme");
        self.add_css_class(if adw::StyleManager::default().is_dark() {
            "dark-theme"
        } else {
            "light-theme"
        });
    }

    pub fn new(app: &Application, config: Arc<AppConfig>) -> Self {
        let window: Self = glib::Object::builder().property("application", app).build();

        window.set_default_size(Self::WINDOW_WIDTH, Self::WINDOW_HEIGHT);
        // Resizable + maximizable. The sidebar keeps a fixed width (hexpand=false
        // + width_request), and only the content_box has hexpand=true, so all the
        // extra width on resize/maximize goes to the right side.
        window.set_resizable(true);
        window.sync_theme_class();
        let window_weak = window.downgrade();
        adw::StyleManager::default().connect_dark_notify(move |_| {
            if let Some(window) = window_weak.upgrade() {
                window.sync_theme_class();
            }
        });
        window.set_title(Some(crate::APP_NAME));

        // Store config and borrow the shared recording state from the
        // Application's controller so the main window, the mini panel, and the
        // global dictation shortcut all drive the same cpal stream + engine.
        let imp = window.imp();
        *imp.config.borrow_mut() = Some(config.clone());
        let controller = app.controller();
        *imp.engine.borrow_mut() = Some(controller.engine_arc());
        *imp.model_catalog.borrow_mut() = Some(controller.model_catalog_arc());
        *imp.controller.borrow_mut() = Some(controller);

        // One-time migration: move any plaintext HF token from config into the
        // system keyring, then clear it from the (world-readable-ish) config file.
        if let Some(token) = config
            .cohere_hf_token
            .clone()
            .filter(|t| !t.trim().is_empty())
        {
            crate::application::tokio_runtime().spawn(async move {
                if crate::secrets::store_hf_token(&token).await.is_ok() {
                    let mut c = AppConfig::load();
                    c.cohere_hf_token = None;
                    c.save();
                    tracing::info!("Migrated HuggingFace token from config to keyring");
                }
            });
        }

        window.sync_ui_from_config();

        // Closing the window hides it; the app keeps running in the background
        // (tray icon + global shortcut stay active). Quit via Ctrl+Q or the
        // tray "Quit" item. Re-open by launching the app again or via the tray.
        window.connect_close_request(|win| {
            if win
                .controller()
                .map(|controller| {
                    controller.owner() == RecordingOwner::Main
                        && controller.state() != crate::audio::capture::RecordingState::Idle
                })
                .unwrap_or(false)
            {
                win.show_toast(&gettext(
                    "Stop or cancel the active recording before hiding the window.",
                ));
                return glib::Propagation::Stop;
            }
            win.imp().retry_take.borrow_mut().take();
            win.show_retry(false);
            win.set_visible(false);
            glib::Propagation::Stop
        });

        if config.first_run {
            let wizard = WelcomeWizard::new(&window);
            wizard.present();
        } else {
            window.load_selected_model();
        }

        // Diagnostic (inert unless STT_DEBUG_WIDTH is set): log the real ALLOCATED
        // widths after layout settles, so the sidebar width can be verified
        // (width_request is only a minimum — content can force it wider).
        if std::env::var("STT_DEBUG_WIDTH").is_ok() {
            let w = window.downgrade();
            glib::timeout_add_local_once(std::time::Duration::from_millis(2500), move || {
                if let Some(win) = w.upgrade() {
                    let imp = win.imp();
                    let sidebar = imp
                        .sidebar_box
                        .borrow()
                        .as_ref()
                        .map(|b| b.width())
                        .unwrap_or(-1);
                    let collapsed = imp.sidebar_collapsed.get();
                    eprintln!(
                        "STT_SIDEBAR_WIDTH={} STT_SIDEBAR_REQUEST={} STT_WIN_WIDTH={} \
                         STT_WIN_HEIGHT={} collapsed={}",
                        sidebar,
                        if collapsed {
                            Self::SIDEBAR_COLLAPSED_WIDTH
                        } else {
                            Self::SIDEBAR_EXPANDED_WIDTH
                        },
                        win.width(),
                        win.height(),
                        collapsed,
                    );
                }
            });
        }

        window
    }

    fn setup_ui(&self) {
        let imp = self.imp();

        let main_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);

        let sidebar_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        sidebar_box.set_width_request(Self::SIDEBAR_EXPANDED_WIDTH);
        sidebar_box.set_hexpand(false);
        sidebar_box.add_css_class("sidebar-box");

        let sidebar_toggle_btn = gtk::Button::builder()
            .icon_name("sidebar-show-symbolic")
            .tooltip_text("Collapse sidebar")
            .build();
        sidebar_toggle_btn.add_css_class("flat");
        sidebar_toggle_btn.set_action_name(Some("win.toggle-sidebar"));

        let title_label = gtk::Label::new(Some(crate::APP_NAME));
        title_label.add_css_class("title");
        title_label.add_css_class("sidebar-app-title");
        title_label.set_xalign(0.0);
        title_label.set_halign(gtk::Align::Start);

        let title_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        title_box.set_halign(gtk::Align::Fill);
        title_box.append(&title_label);

        let sidebar_header = super::sidebar::header(&title_box, &sidebar_toggle_btn);
        sidebar_box.append(&sidebar_header);

        let sidebar_list = gtk::ListBox::new();
        sidebar_list.set_selection_mode(gtk::SelectionMode::Single);
        sidebar_list.add_css_class("navigation-sidebar");

        let mut nav_labels = Vec::new();
        let mut nav_boxes = Vec::new();

        // Section titles ("WORKSPACE" / "SETTINGS") are intentionally omitted —
        // the navigation reads cleaner as a single uninterrupted list. The empty
        // section-row vec and None header keep the collapse logic below happy.
        let section_rows: Vec<gtk::ListBoxRow> = Vec::new();
        let settings_header_label_opt: Option<gtk::Label> = None;
        for nav_item in NavItem::all() {
            let (row, label, hbox) = self.create_nav_row(*nav_item);
            sidebar_list.append(&row);
            nav_labels.push(label);
            nav_boxes.push(hbox);
        }

        let sidebar_scroll = gtk::ScrolledWindow::new();
        sidebar_scroll.add_css_class("sidebar-scroll");
        sidebar_scroll.set_vexpand(true);
        sidebar_scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
        sidebar_scroll.set_child(Some(&sidebar_list));
        sidebar_box.append(&sidebar_scroll);

        // GPU Status panel at bottom of sidebar
        let gpu_panel = GpuStatusPanel::new();
        sidebar_box.append(&gpu_panel);

        // Bottom-of-sidebar spacer. The update notice lives only in the
        // status bar (bottom right); this box keeps the GPU panel's resting
        // position exactly where it was when it also hosted a banner.
        let info_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
        info_box.set_margin_start(12);
        info_box.set_margin_end(12);
        info_box.set_margin_top(8);
        info_box.set_margin_bottom(8);

        sidebar_box.append(&info_box);

        let content_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        // One shared surface for every page on the right. Individual pages stay
        // transparent so navigating never changes the background colour.
        content_box.add_css_class("content-area");
        // hexpand=true is what makes the right column absorb all extra width when
        // the window is resized/maximized, while the sidebar (hexpand=false) keeps
        // its fixed width. CONTENT_MIN_WIDTH is just the floor for this column.
        content_box.set_hexpand(true);
        content_box.set_width_request(Self::CONTENT_MIN_WIDTH);

        let header_controls = HeaderControls::new();
        content_box.append(&header_controls);

        // In-app notification bar (top of content, themed). Its single action
        // button is used only for the "Undo" after an AI improvement.
        let notification_banner = adw::Banner::new("");
        notification_banner.set_revealed(false);
        content_box.append(&notification_banner);

        let retry_banner = adw::Banner::new(&gettext("Audio retained in memory. Retry without recording again; New, Clear or Close discards it."));
        retry_banner.set_button_label(Some(&gettext("Retry")));
        retry_banner.set_revealed(false);
        let weak = self.downgrade();
        retry_banner.connect_button_clicked(move |_| {
            if let Some(window) = weak.upgrade() {
                window.retry_last_audio();
            }
        });
        content_box.append(&retry_banner);
        *imp.retry_banner.borrow_mut() = Some(retry_banner);

        let persistence_banner = adw::Banner::new("");
        persistence_banner.set_revealed(false);
        persistence_banner.set_button_label(Some(&gettext("Retry")));
        persistence_banner.connect_button_clicked(|_| AppConfig::load().save());
        content_box.append(&persistence_banner);
        let weak_banner = persistence_banner.downgrade();
        glib::timeout_add_local(std::time::Duration::from_millis(500), move || {
            let Some(banner) = weak_banner.upgrade() else {
                return glib::ControlFlow::Break;
            };
            match AppConfig::persistence_status() {
                crate::config_writer::SaveStatus::Pending => {
                    banner.set_title(&gettext("Saving settings…"));
                    banner.set_revealed(true);
                }
                crate::config_writer::SaveStatus::Persisted => banner.set_revealed(false),
                crate::config_writer::SaveStatus::Failed(_) => {
                    banner.set_title(&gettext("Settings could not be saved. Changes currently apply only to this session."));
                    banner.set_revealed(true);
                }
            }
            glib::ControlFlow::Continue
        });

        let content_stack = gtk::Stack::new();
        content_stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        content_stack.set_transition_duration(200);
        content_stack.set_vexpand(true);
        content_stack.set_hexpand(true);

        let transcription_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        transcription_page.set_vexpand(true);
        transcription_page.add_css_class("transcription-page");

        let workspace_surface = gtk::Box::new(gtk::Orientation::Vertical, 0);
        workspace_surface.set_vexpand(true);
        workspace_surface.add_css_class("workspace-surface");

        let controls = Controls::new();
        workspace_surface.append(&controls);

        let transcript_view = TranscriptView::new();
        workspace_surface.append(&transcript_view);

        let transcription_clamp = adw::Clamp::new();
        transcription_clamp.set_maximum_size(940);
        transcription_clamp.set_tightening_threshold(760);
        transcription_clamp.set_vexpand(true);
        transcription_clamp.set_child(Some(&workspace_surface));
        transcription_page.append(&transcription_clamp);

        content_stack.add_named(&transcription_page, Some("transcription"));

        let history_page = HistoryPage::new();
        content_stack.add_named(&history_page, Some("history"));

        let microphone_page = MicrophonePage::new();

        let model_page = ModelPage::new();

        let language_page = LanguagePage::new();

        let performance_page = PerformancePage::new();

        let dictation_page = DictationPage::new();

        let (recognition_page, recognition_stack) = Self::grouped_settings(&[
            ("model", gettext("Model"), model_page.upcast_ref()),
            ("language", gettext("Language"), language_page.upcast_ref()),
            (
                "performance",
                gettext("Performance"),
                performance_page.upcast_ref(),
            ),
        ]);
        content_stack.add_named(&recognition_page, Some("recognition"));

        let (dictation_audio_page, dictation_audio_stack) = Self::grouped_settings(&[
            (
                "dictation",
                gettext("Dictation"),
                dictation_page.upcast_ref(),
            ),
            (
                "microphone",
                gettext("Microphone"),
                microphone_page.upcast_ref(),
            ),
        ]);
        content_stack.add_named(&dictation_audio_page, Some("dictation-audio"));

        let dictionary_page = DictionaryPage::new();
        content_stack.add_named(&dictionary_page, Some("dictionary"));

        let llm_page = LlmPage::new();
        content_stack.add_named(&llm_page, Some("llm"));

        let api_page = ApiPage::new();
        content_stack.add_named(&api_page, Some("api"));

        // Settings pages use AdwPreferencesPage, which centres its content in a
        // 600px clamp by default. Widen every page so the settings fill the full
        // width of the content area instead of a narrow centred column.
        fill_preferences_width(&microphone_page);
        fill_preferences_width(&model_page);
        fill_preferences_width(&language_page);
        fill_preferences_width(&performance_page);
        fill_preferences_width(&dictation_page);
        fill_preferences_width(&dictionary_page);
        fill_preferences_width(&llm_page);
        fill_preferences_width(&api_page);

        let help_page = HelpPage::new();
        content_stack.add_named(&help_page, Some("help"));

        content_box.append(&content_stack);

        let status_bar = StatusBar::new();
        content_box.append(&status_bar);

        main_box.append(&sidebar_box);
        main_box.append(&content_box);

        let toast_overlay = adw::ToastOverlay::new();
        toast_overlay.set_child(Some(&main_box));

        self.set_content(Some(&toast_overlay));

        *imp.toast_overlay.borrow_mut() = Some(toast_overlay);
        *imp.notification_banner.borrow_mut() = Some(notification_banner.clone());

        // The banner's action button is only ever the "Undo" after an AI improve
        // (switch the result back to the raw transcript).
        {
            let window = self.clone();
            notification_banner.connect_button_clicked(move |b| {
                b.set_revealed(false);
                window.select_variant(0);
            });
        }
        *imp.sidebar_box.borrow_mut() = Some(sidebar_box);
        *imp.sidebar_list.borrow_mut() = Some(sidebar_list.clone());
        *imp.content_stack.borrow_mut() = Some(content_stack);
        *imp.recognition_stack.borrow_mut() = Some(recognition_stack);
        *imp.dictation_audio_stack.borrow_mut() = Some(dictation_audio_stack);
        *imp.header_controls.borrow_mut() = Some(header_controls);
        *imp.transcript_view.borrow_mut() = Some(transcript_view);
        *imp.controls.borrow_mut() = Some(controls);
        *imp.status_bar.borrow_mut() = Some(status_bar);
        *imp.history_page.borrow_mut() = Some(history_page);
        *imp.help_page.borrow_mut() = Some(help_page);
        *imp.microphone_page.borrow_mut() = Some(microphone_page);
        *imp.model_page.borrow_mut() = Some(model_page);
        *imp.language_page.borrow_mut() = Some(language_page);
        *imp.performance_page.borrow_mut() = Some(performance_page);
        *imp.dictation_page.borrow_mut() = Some(dictation_page);
        *imp.dictionary_page.borrow_mut() = Some(dictionary_page);
        *imp.llm_page.borrow_mut() = Some(llm_page);
        *imp.api_page.borrow_mut() = Some(api_page);
        *imp.gpu_panel.borrow_mut() = Some(gpu_panel);
        *imp.nav_labels.borrow_mut() = nav_labels;
        *imp.nav_boxes.borrow_mut() = nav_boxes;
        *imp.sidebar_toggle_btn.borrow_mut() = Some(sidebar_toggle_btn);
        *imp.sidebar_title_box.borrow_mut() = Some(title_box);
        *imp.settings_header_label.borrow_mut() = settings_header_label_opt;
        *imp.sidebar_section_rows.borrow_mut() = section_rows;
        *imp.info_box.borrow_mut() = Some(info_box);
        // Start in compact mode: the navigation icons stay visible, but the
        // labels, GPU panel and footer take no width until the user expands it.
        imp.sidebar_collapsed.set(false);
        self.toggle_sidebar();

        let window = self.clone();
        sidebar_list.connect_row_selected(move |_, row| {
            if let Some(row) = row {
                let index = row.index() as usize;
                if let Some(nav_item) = NavItem::all().get(index) {
                    window.navigate_to(*nav_item);
                }
            }
        });

        if let Some(first_row) = sidebar_list.row_at_index(0) {
            sidebar_list.select_row(Some(&first_row));
        }

        self.setup_controls();

        // Wire file drag-and-drop
        self.setup_file_drop();

        self.setup_engine_selector();

        // Wire model dropdown selection change
        self.setup_model_dropdown();

        self.setup_language_display();

        self.setup_ai_toggle();

        // Wire transform chips + raw/polished variant selector
        self.setup_transcript_extras();

        self.check_for_updates();
    }

    /// Wire the "Improve with AI" toggle: arming it turns on auto-improve so the
    /// next transcriptions are improved with the active preset (like Translate).
    /// Kept in sync with the "Auto-improve after dictation" setting via config.
    fn setup_ai_toggle(&self) {
        let Some(controls) = self.imp().controls.borrow().as_ref().cloned() else {
            return;
        };
        let window = self.clone();
        controls.connect_ai_toggled(move |active| {
            if window.imp().syncing_ai.get() {
                return;
            }
            let mut c = AppConfig::load();
            c.llm_auto_apply = active;
            c.save();
        });
        self.refresh_ai_button();
    }

    /// Show/hide the "Improve with AI" toggle and sync its state with the current
    /// `llm_enabled` / `llm_auto_apply` settings.
    fn refresh_ai_button(&self) {
        if let Some(controls) = self.imp().controls.borrow().as_ref() {
            let cfg = AppConfig::load();
            controls.set_ai_visible(cfg.llm_enabled);
            self.imp().syncing_ai.set(true);
            controls.set_ai_active(cfg.llm_auto_apply);
            self.imp().syncing_ai.set(false);
        }
        // Keep the transform chips in sync with the (possibly edited) preset list.
        self.refresh_chip_presets();
    }

    /// Wire chip / raw-polished selector / message-selection callbacks and
    /// populate the chip presets from config.
    fn setup_transcript_extras(&self) {
        let Some(tv) = self.imp().transcript_view.borrow().as_ref().cloned() else {
            return;
        };
        let window = self.clone();
        tv.connect_chip_activated(move |idx| window.on_chip(idx));
        let window = self.clone();
        tv.connect_variant_changed(move |idx| window.select_variant(idx));
        let window = self.clone();
        tv.connect_message_selected(move |idx| window.select_message(idx));
        let window = self.clone();
        tv.connect_voice_edit(move || window.on_voice_edit());
        self.imp().selected_message.set(-1);
        self.refresh_chip_presets();
    }

    fn selected_idx(&self) -> Option<usize> {
        let s = self.imp().selected_message.get();
        if s < 0 {
            None
        } else {
            Some(s as usize)
        }
    }

    /// Refresh the chip preset labels + visibility from the current LLM config.
    fn refresh_chip_presets(&self) {
        let Some(tv) = self.imp().transcript_view.borrow().as_ref().cloned() else {
            return;
        };
        let cfg = AppConfig::load();
        let names: Vec<String> = cfg.llm_presets.iter().map(|p| p.name.clone()).collect();
        tv.set_chip_presets(&names);
        let has_result = !self.imp().messages.borrow().is_empty();
        tv.set_chips_visible(cfg.llm_enabled && has_result);
    }

    /// Show the newest finished transcription as the single current-session
    /// result. Older transcriptions remain in History, not in this workspace.
    fn add_result_message(
        &self,
        state: crate::ui::result_state::ResultState,
        confidence: Option<f64>,
    ) {
        let Some(tv) = self.imp().transcript_view.borrow().as_ref().cloned() else {
            return;
        };
        tv.clear();
        let text = state.active_text().to_string();
        let (words, wpm) = (state.stats.words, state.stats.wpm);
        let mut messages = self.imp().messages.borrow_mut();
        messages.clear();
        messages.push(state);
        drop(messages);
        let idx = tv.add_message(&text);
        self.imp().selected_message.set(idx as isize);
        tv.set_confidence(confidence);
        tv.set_stats(words, wpm);
        // Copy to the clipboard focus-independently when possible: a plain
        // set_text silently no-ops on Wayland if this window lost focus while
        // the (long) transcription ran, leaving stale clipboard content.
        let copy_text = text.clone();
        glib::spawn_future_local(async move {
            let (tx, rx) = async_channel::bounded::<bool>(1);
            let portal_text = copy_text.clone();
            crate::application::tokio_runtime().spawn(async move {
                let ok = crate::portal::paste::set_clipboard_via_portal(portal_text).await;
                let _ = tx.send(ok).await;
            });
            if !rx.recv().await.unwrap_or(false) {
                if let Some(display) = gtk::gdk::Display::default() {
                    display.clipboard().set_text(&copy_text);
                }
            }
        });
        self.refresh_result_controls();
    }

    fn refresh_result_controls(&self) {
        let Some(tv) = self.imp().transcript_view.borrow().as_ref().cloned() else {
            return;
        };
        let cfg = AppConfig::load();
        let Some(sel) = self.selected_idx() else {
            tv.hide_result_controls();
            return;
        };
        let messages = self.imp().messages.borrow();
        if let Some(st) = messages.get(sel) {
            let labels = st.selector_labels(&gettext("Raw"));
            tv.set_variant_selector(&labels, st.active);
            tv.set_stats(st.stats.words, st.stats.wpm);
            tv.set_chips_visible(cfg.llm_enabled);
            tv.set_chips_sensitive(true);
            tv.set_voice_edit_visible(cfg.llm_enabled);
        }
    }

    /// Select a message (clicking its bubble): highlight it and point the
    /// chips / selector / stats at it.
    fn select_message(&self, idx: usize) {
        if idx >= self.imp().messages.borrow().len() {
            return;
        }
        self.imp().selected_message.set(idx as isize);
        if let Some(tv) = self.imp().transcript_view.borrow().as_ref() {
            tv.set_selected_message(idx);
        }
        self.refresh_result_controls();
    }

    /// Switch the active variant of the SELECTED message (from the dropdown).
    fn select_variant(&self, vidx: usize) {
        let Some(sel) = self.selected_idx() else {
            return;
        };
        let active_text = {
            let mut messages = self.imp().messages.borrow_mut();
            let Some(st) = messages.get_mut(sel) else {
                return;
            };
            st.set_active(vidx);
            st.active_text().to_string()
        };
        if let Some(tv) = self.imp().transcript_view.borrow().as_ref() {
            tv.update_message(sel, &active_text);
        }
        if let Some(display) = gtk::gdk::Display::default() {
            display.clipboard().set_text(&active_text);
        }
    }

    /// Handle a transform-chip click: run preset `idx` on the SELECTED message.
    fn on_chip(&self, chip_idx: usize) {
        let Some(sel) = self.selected_idx() else {
            return;
        };
        let cfg = AppConfig::load();
        let Some(preset) = cfg.llm_presets.get(chip_idx).cloned() else {
            return;
        };
        self.run_preset_on_message(sel, preset, false);
    }

    /// Run `preset` on message `msg_idx`'s active text and add the result as a new
    /// (auto-activated) variant on THAT message only — other messages are untouched.
    fn run_preset_on_message(&self, msg_idx: usize, preset: LlmPreset, show_undo: bool) {
        let source = {
            let messages = self.imp().messages.borrow();
            match messages.get(msg_idx) {
                Some(st) => st.active_text().trim().to_string(),
                None => return,
            }
        };
        if source.is_empty() {
            return;
        }
        let llm_cfg = Self::resolve_llm_config_for_preset(&preset);
        if let Some(tv) = self.imp().transcript_view.borrow().as_ref() {
            tv.set_chips_sensitive(false);
        }
        if let Some(sb) = self.imp().status_bar.borrow().as_ref() {
            sb.set_recording_status(&gettext("Improving with AI…"));
        }
        let rx = crate::llm::improve_async(llm_cfg, preset.system_prompt(), source);
        let generation = self.imp().operation_generation.get();
        let window = self.clone();
        let label = preset.name.clone();
        glib::spawn_future_local(async move {
            let res = rx.recv().await;
            if !window.operation_is_current(generation) {
                return;
            }
            if let Some(tv) = window.imp().transcript_view.borrow().as_ref() {
                tv.set_chips_sensitive(true);
            }
            if let Some(sb) = window.imp().status_bar.borrow().as_ref() {
                sb.set_recording_status(&gettext("Ready"));
            }
            match res {
                Ok(Ok(improved)) => {
                    let improved = improved.trim().to_string();
                    if improved.is_empty() {
                        window.show_toast(&gettext("AI returned an empty result"));
                        return;
                    }
                    let active_text = {
                        let mut messages = window.imp().messages.borrow_mut();
                        let Some(st) = messages.get_mut(msg_idx) else {
                            return;
                        };
                        st.push_variant(label, improved);
                        if let (Some(id), Some(history)) =
                            (&st.history_id, window.imp().history_page.borrow().as_ref())
                        {
                            history.update_variants(id, st.polished_text(), None);
                        }
                        st.active_text().to_string()
                    };
                    if let Some(tv) = window.imp().transcript_view.borrow().as_ref() {
                        tv.update_message(msg_idx, &active_text);
                    }
                    // If this is the selected message, refresh its controls.
                    if window.selected_idx() == Some(msg_idx) {
                        window.refresh_result_controls();
                        if let Some(display) = gtk::gdk::Display::default() {
                            display.clipboard().set_text(&active_text);
                        }
                    }
                    if show_undo {
                        window.show_improve_undo_toast();
                    }
                }
                Ok(Err(e)) => window.show_toast(&format!("{}: {}", gettext("AI error"), e)),
                Err(_) => {}
            }
        });
    }

    /// Build the LLM connection config for a preset. Delegates to the canonical
    /// resolver in `application` so the override logic lives in one place.
    fn resolve_llm_config_for_preset(preset: &LlmPreset) -> crate::llm::LlmConfig {
        crate::application::resolve_llm_cfg(&AppConfig::load(), preset)
    }

    /// Auto-improve: run the active preset on the most-recent message.
    fn improve_with_preset(&self, preset_idx: usize, _source_text: String) {
        let cfg = AppConfig::load();
        let Some(preset) = cfg.llm_presets.get(preset_idx).cloned() else {
            return;
        };
        let last = match self.imp().messages.borrow().len() {
            0 => return,
            n => n - 1,
        };
        self.run_preset_on_message(last, preset, true);
    }

    /// Voice edit (main window): toggle recording a spoken instruction for the
    /// selected message.
    fn on_voice_edit(&self) {
        if self.imp().voice_editing.get() {
            self.stop_voice_edit_main();
        } else {
            self.start_voice_edit_main();
        }
    }

    fn start_voice_edit_main(&self) {
        let Some(sel) = self.selected_idx() else {
            self.show_toast(&gettext("Select a message first."));
            return;
        };
        let has_text = self
            .imp()
            .messages
            .borrow()
            .get(sel)
            .map(|s| !s.active_text().trim().is_empty())
            .unwrap_or(false);
        if !has_text {
            self.show_toast(&gettext("No text to edit."));
            return;
        }
        let cfg = AppConfig::load();
        if !cfg.llm_enabled {
            self.show_toast(&gettext(
                "Enable the LLM in Settings → LLM to use Voice edit.",
            ));
            return;
        }
        let Some(controller) = self.controller() else {
            return;
        };
        if !controller.try_acquire(RecordingOwner::VoiceEdit) {
            self.show_toast(&gettext("Busy — finish the current recording first."));
            return;
        }
        if let Some(app) = self.application().and_downcast::<Application>() {
            app.cancel_pending_paste();
        }
        let (waveform_tx, _waveform_rx) = async_channel::bounded::<Vec<f32>>(32);
        match controller.start(cfg.selected_microphone.as_deref(), waveform_tx) {
            Ok(()) => {
                self.imp().voice_editing.set(true);
                if let Some(tv) = self.imp().transcript_view.borrow().as_ref() {
                    tv.set_voice_edit_recording(true);
                }
                if let Some(sb) = self.imp().status_bar.borrow().as_ref() {
                    sb.set_recording_status(&gettext(
                        "Listening for your edit… click \"Stop edit\" when done",
                    ));
                }
                // Poll for a dead stream (mic unplugged) so this edit can't
                // sit on "Listening…" over silence forever — the app-level
                // pollers only cover panel-initiated sessions. voice_editing
                // scopes the poll to THIS window's edit, so it never consumes
                // a stream error belonging to a panel session.
                let win_weak = self.downgrade();
                glib::timeout_add_local(std::time::Duration::from_millis(500), move || {
                    let Some(win) = win_weak.upgrade() else {
                        return glib::ControlFlow::Break;
                    };
                    if !win.imp().voice_editing.get() {
                        return glib::ControlFlow::Break;
                    }
                    let Some(controller) = win.controller() else {
                        return glib::ControlFlow::Break;
                    };
                    if controller.owner() != RecordingOwner::VoiceEdit
                        || controller.state() == crate::audio::capture::RecordingState::Idle
                    {
                        return glib::ControlFlow::Break;
                    }
                    if let Some(err) = controller.take_stream_error() {
                        tracing::warn!("Audio stream died during voice edit: {err}");
                        win.show_toast(&gettext("Microphone stopped — finishing the edit."));
                        win.stop_voice_edit_main();
                        return glib::ControlFlow::Break;
                    }
                    glib::ControlFlow::Continue
                });
            }
            Err(e) => {
                controller.release();
                self.show_toast(&format!("Couldn't start recording: {e}"));
            }
        }
    }

    fn stop_voice_edit_main(&self) {
        self.imp().voice_editing.set(false);
        if let Some(tv) = self.imp().transcript_view.borrow().as_ref() {
            tv.set_voice_edit_recording(false);
        }
        let Some(controller) = self.controller() else {
            return;
        };
        if controller.owner() != RecordingOwner::VoiceEdit {
            return;
        }
        let dur = controller.recording_duration_secs();
        // Detach raw audio only; conditioning runs on the inference worker so
        // stopping a long voice-edit capture can't freeze the GTK thread.
        let audio = match controller.stop_snapshot() {
            Ok(a) => a,
            Err(e) => {
                controller.release();
                self.show_toast(&format!("Error stopping recording: {e}"));
                return;
            }
        };
        controller.release();
        if audio.raw_len() == 0 {
            self.show_toast(&gettext("Didn't catch an instruction."));
            if let Some(sb) = self.imp().status_bar.borrow().as_ref() {
                sb.set_recording_status(&gettext("Ready"));
            }
            return;
        }
        let Some(sel) = self.selected_idx() else {
            return;
        };
        let target = self
            .imp()
            .messages
            .borrow()
            .get(sel)
            .map(|s| s.active_text().to_string())
            .unwrap_or_default();
        if target.trim().is_empty() {
            return;
        }
        if let Some(sb) = self.imp().status_bar.borrow().as_ref() {
            sb.set_recording_status(&gettext("Applying your edit…"));
        }
        let params = self.build_dictation_params(self.active_backend());
        // Not user-cancellable: this is a one-shot voice instruction.
        let receiver = controller.transcribe_snapshot_async(
            audio,
            params,
            dur,
            self.imp().work_cancel.borrow().clone(),
        );
        let window = self.clone();
        glib::spawn_future_local(async move {
            let instruction = match receiver.recv().await {
                Ok(Ok(o)) => o.cleaned_text.trim().to_string(),
                _ => String::new(),
            };
            if instruction.is_empty() {
                if let Some(sb) = window.imp().status_bar.borrow().as_ref() {
                    sb.set_recording_status(&gettext("Ready"));
                }
                window.show_toast(&gettext("Didn't catch an instruction."));
                return;
            }
            let cfg = AppConfig::load();
            let llm_cfg = crate::llm::LlmConfig {
                api_url: cfg.llm_api_url.clone(),
                api_key: None,
                model: cfg.llm_model.clone(),
                temperature: cfg.llm_temperature,
            };
            let system = "You are editing the user's text. Apply the user's spoken instruction to \
                          the TARGET TEXT and reply with ONLY the edited text, preserving the original \
                          language. Do not add explanations or quotes.";
            let user = format!("Apply this instruction: {instruction}\n\nText:\n{target}");
            let rx = crate::llm::improve_async(llm_cfg, system.to_string(), user);
            let res = rx.recv().await;
            if let Some(sb) = window.imp().status_bar.borrow().as_ref() {
                sb.set_recording_status(&gettext("Ready"));
            }
            match res {
                Ok(Ok(edited)) if !edited.trim().is_empty() => {
                    let active_text = {
                        let mut messages = window.imp().messages.borrow_mut();
                        let Some(st) = messages.get_mut(sel) else {
                            return;
                        };
                        st.push_variant(gettext("Voice edit"), edited.trim().to_string());
                        if let (Some(id), Some(history)) =
                            (&st.history_id, window.imp().history_page.borrow().as_ref())
                        {
                            history.update_variants(id, st.polished_text(), None);
                        }
                        st.active_text().to_string()
                    };
                    if let Some(tv) = window.imp().transcript_view.borrow().as_ref() {
                        tv.update_message(sel, &active_text);
                    }
                    if window.selected_idx() == Some(sel) {
                        window.refresh_result_controls();
                        if let Some(display) = gtk::gdk::Display::default() {
                            display.clipboard().set_text(&active_text);
                        }
                    }
                }
                Ok(Ok(_)) => window.show_toast(&gettext("AI returned an empty result")),
                Ok(Err(e)) => window.show_toast(&format!("{}: {}", gettext("AI error"), e)),
                Err(_) => {}
            }
        });
    }

    /// Confirm an AI improvement in the top banner with an Undo button that
    /// switches back to the raw transcript (non-destructive — the AI variant
    /// stays available in the raw/polished selector).
    fn show_improve_undo_toast(&self) {
        self.show_banner(&gettext("Improved with AI"), true);
    }

    /// Show an in-app notification in the top banner (themed, not a bottom OSD
    /// toast). When `with_undo` is set, the banner shows an "Undo" button wired
    /// to revert the last AI improvement. Auto-hides after a few seconds.
    fn show_banner(&self, message: &str, with_undo: bool) {
        let Some(banner) = self.imp().notification_banner.borrow().clone() else {
            return;
        };
        banner.set_title(&crate::error::redact_secrets(message));
        let undo_label = gettext("Undo");
        banner.set_button_label(if with_undo {
            Some(undo_label.as_str())
        } else {
            None
        });
        banner.set_revealed(true);
        // Auto-hide; only hide if the banner still shows THIS message (so a newer
        // notification isn't dismissed early).
        let banner_weak = banner.downgrade();
        let shown = crate::error::redact_secrets(message);
        let secs = if with_undo { 6 } else { 4 };
        glib::timeout_add_seconds_local_once(secs, move || {
            if let Some(b) = banner_weak.upgrade() {
                if b.title().as_str() == shown {
                    b.set_revealed(false);
                }
            }
        });
    }

    /// If auto-improve is enabled, run the active preset over `text`.
    fn maybe_auto_improve(&self, text: &str) {
        let cfg = AppConfig::load();
        if cfg.llm_enabled && cfg.llm_auto_apply && !cfg.llm_presets.is_empty() {
            let idx = cfg.llm_active_preset.min(cfg.llm_presets.len() - 1);
            self.improve_with_preset(idx, text.to_string());
        }
    }

    /// For long transcripts (files / long dictations), generate an LLM summary +
    /// chapters and show them in the transcript view's "Summary & chapters"
    /// section. Self-gates on the LLM being enabled and a length threshold.
    fn maybe_generate_summary(&self, text: &str, duration_secs: f32) {
        let cfg = AppConfig::load();
        if !cfg.llm_enabled || !cfg.llm_auto_summary {
            return;
        }
        let words = crate::ui::result_state::word_count(text);
        if words < crate::limits::SUMMARY_MIN_WORDS
            && duration_secs < crate::limits::SUMMARY_MIN_SECS
        {
            return;
        }
        let Some(tv) = self.imp().transcript_view.borrow().as_ref().cloned() else {
            return;
        };
        let llm_cfg = crate::llm::LlmConfig {
            api_url: cfg.llm_api_url.clone(),
            api_key: None,
            model: cfg.llm_model.clone(),
            temperature: cfg.llm_temperature,
        };
        tv.set_summary_loading(true);
        let history_id = self.selected_idx().and_then(|index| {
            self.imp()
                .messages
                .borrow()
                .get(index)
                .and_then(|state| state.history_id.clone())
        });
        let generation = self.imp().operation_generation.get();

        let rx_sum = crate::llm::improve_async(
            llm_cfg.clone(),
            crate::transcription::summary::SUMMARY_SYSTEM_PROMPT.to_string(),
            text.to_string(),
        );
        let segments = self.imp().last_segments.borrow().clone();
        let chap_input = crate::transcription::summary::build_chaptered_input(&segments);
        let rx_chap = if chap_input.trim().is_empty() {
            None
        } else {
            Some(crate::llm::improve_async(
                llm_cfg,
                crate::transcription::summary::CHAPTERS_SYSTEM_PROMPT.to_string(),
                chap_input,
            ))
        };

        let window = self.clone();
        glib::spawn_future_local(async move {
            let summary_text = match rx_sum.recv().await {
                Ok(Ok(s)) => s.trim().to_string(),
                _ => String::new(),
            };
            let chapters = if let Some(rx) = rx_chap {
                match rx.recv().await {
                    Ok(Ok(r)) => crate::transcription::summary::parse_chapters(&r),
                    _ => Vec::new(),
                }
            } else {
                Vec::new()
            };
            if !window.operation_is_current(generation) {
                return;
            }
            if let (Some(id), Some(history)) =
                (&history_id, window.imp().history_page.borrow().as_ref())
            {
                if !summary_text.is_empty() {
                    history.update_variants(id, None, Some(&summary_text));
                }
            }
            if let Some(tv) = window.imp().transcript_view.borrow().as_ref() {
                if summary_text.is_empty() && chapters.is_empty() {
                    tv.clear_summary();
                } else {
                    tv.set_summary(&summary_text, &chapters);
                }
            }
        });
    }

    fn current_config_snapshot(&self) -> Option<AppConfig> {
        // Read the authoritative process-wide config (kept current by every
        // `save()`), NOT the window's local `imp.config` Arc. The translate and
        // language toggles persist via `AppConfig::load()` + `save()` without
        // updating `imp.config`, so cloning the local Arc here would carry a stale
        // `translate_to_english` (and other fields) and re-stamp it on the next
        // single-field `save()` in the model/engine handlers — silently clobbering
        // the user's choice and making the pop-up translate intermittently.
        // `imp.config` is only ever set to already-saved values, so the cache is
        // always at least as fresh; gate on it only so we return None before the
        // window has been initialized.
        self.imp()
            .config
            .borrow()
            .as_ref()
            .map(|_| AppConfig::load())
    }

    fn controller(&self) -> Option<Rc<RecordingController>> {
        self.imp().controller.borrow().clone()
    }

    /// Build a widget-free parameter snapshot for a dictation run, combining the
    /// Whisper inference settings, the selected language, microphone, and mode.
    fn build_dictation_params(&self, backend: String) -> DictationParams {
        let imp = self.imp();
        let whisper = self.gather_whisper_params();
        let config = self.current_config_snapshot();
        let language_code = imp
            .language_page
            .borrow()
            .as_ref()
            .and_then(|p| p.selected_language_code());
        let selected_microphone = config.as_ref().and_then(|c| c.selected_microphone.clone());
        let mode = config
            .as_ref()
            .map(|c| DictationMode::from_config_str(&c.dictation_mode))
            .unwrap_or_default();
        // Merge the user's initial prompt with the personal-dictionary terms, and
        // collect the replacement rules (both via the shared config helper, so the
        // widget path matches the global-dictation path).
        let initial_prompt = config
            .as_ref()
            .and_then(|c| c.effective_initial_prompt())
            .or(whisper.initial_prompt);
        let replacements = config
            .as_ref()
            .filter(|c| c.dictionary_enabled)
            .map(|c| c.dictionary_replacements.clone())
            .unwrap_or_default();
        DictationParams {
            model_config: Arc::new(config.unwrap_or_default()),
            backend,
            language_code,
            n_threads: whisper.n_threads,
            beam_size: whisper.beam_size,
            temperature: whisper.temperature,
            translate: whisper.translate,
            initial_prompt,
            selected_microphone,
            mode,
            replacements,
        }
    }

    fn active_backend(&self) -> String {
        self.current_config_snapshot()
            .map(|config| config.backend)
            .unwrap_or_else(|| "whisper".to_string())
    }

    fn set_language_display(&self, text: &str) {
        if let Some(sb) = self.imp().status_bar.borrow().as_ref() {
            sb.set_language(text);
        }
    }

    fn ellipsize_chars(text: &str, max_chars: usize) -> String {
        let mut chars = text.chars();
        let truncated: String = chars.by_ref().take(max_chars).collect();
        if chars.next().is_some() {
            format!("{}…", truncated)
        } else {
            truncated
        }
    }

    fn replace_config(&self, config: AppConfig) {
        *self.imp().config.borrow_mut() = Some(Arc::new(config));
    }

    fn sync_ui_from_config(&self) {
        let Some(config) = self.current_config_snapshot() else {
            return;
        };

        let downloaded = Self::downloaded_model_entries();

        // Reflect the engine in the Model tab's "Default Engine" combo (guarded
        // so it doesn't re-fire the change handler).
        if let Some(model_page) = self.imp().model_page.borrow().as_ref() {
            self.imp().syncing_backend.set(true);
            model_page.set_engine(&config.backend);
            self.imp().syncing_backend.set(false);
        }

        if let Some(header) = self.imp().header_controls.borrow().as_ref() {
            // Guard the whole rebuild+restore. `update_models_for_backend` calls
            // `set_selected(0)` internally, which fires the model-changed handler
            // synchronously; without the guard it would clobber
            // `config.selected_model` with the first entry in the list (e.g.
            // "tiny") instead of the model the user actually chose.
            self.imp().syncing_dropdown.set(true);
            header.update_models_for_backend(&config.backend, &downloaded);

            if config.auto_detect_language {
                self.set_language_display("Auto-detect");
            } else if let Some(language) = config.language.as_deref() {
                self.set_language_display(&language_code_to_name(language));
            }

            if config.backend == "parakeet" {
                header.set_selected_model(0);
                header.set_model_status(
                    crate::transcription::parakeet::parakeet_ready(),
                    "parakeet-tdt-0.6b-v3",
                );
            } else if config.backend == "qwen" {
                // If the active size isn't downloaded but the other is, switch the
                // active size to a downloaded one (so the dropdown + status agree).
                let downloaded: Vec<String> = ["0.6B", "1.7B"]
                    .into_iter()
                    .filter(|s| crate::transcription::qwen::is_model_downloaded_size(s))
                    .map(|s| s.to_string())
                    .collect();
                let active = if downloaded.iter().any(|s| s == &config.qwen_model_size) {
                    config.qwen_model_size.clone()
                } else if let Some(first) = downloaded.first() {
                    let mut corrected = config.clone();
                    corrected.qwen_model_size = first.clone();
                    corrected.save();
                    self.replace_config(corrected);
                    first.clone()
                } else {
                    config.qwen_model_size.clone()
                };
                header.set_selected_model_by_id(&active);
                header.set_model_status(crate::transcription::qwen::qwen_ready(), "qwen3-asr");
            } else {
                let index = downloaded
                    .iter()
                    .position(|(id, _)| id == &config.selected_model)
                    .unwrap_or(0) as u32;
                header.set_selected_model(index);
                header.set_model_status(false, "");
            }
            self.imp().syncing_dropdown.set(false);
        }

        self.apply_backend_capabilities(&config.backend);

        // Keep the translate toggles honest on every full re-sync — backend
        // switches force them inactive via apply_backend_capabilities above.
        if config.backend == "whisper" {
            let controls = self.imp().controls.borrow().as_ref().cloned();
            let lang_page = self.imp().language_page.borrow().as_ref().cloned();
            if let (Some(controls), Some(lang_page)) = (controls, lang_page) {
                self.imp().syncing_translate.set(true);
                controls.set_translate_active(config.translate_to_english);
                lang_page.set_translate_enabled(config.translate_to_english);
                self.imp().syncing_translate.set(false);
            }
        }

        if let Some(status_bar) = self.imp().status_bar.borrow().as_ref() {
            if config.backend == "parakeet" {
                status_bar.set_model_name(&gettext("Parakeet v3"));
                status_bar.set_recording_status(
                    if crate::transcription::parakeet::parakeet_ready() {
                        "Ready"
                    } else {
                        "Setup Required"
                    },
                );
            } else if config.backend == "qwen" {
                status_bar.set_model_name(&gettext("Qwen3-ASR"));
                status_bar.set_recording_status(if crate::transcription::qwen::qwen_ready() {
                    "Ready"
                } else {
                    "Setup Required"
                });
            } else if ModelCatalog::is_downloaded(&config.selected_model) {
                status_bar.set_model_name(&config.selected_model);
                status_bar.set_recording_status(&gettext("Loading model…"));
            } else {
                status_bar.set_model_name(&gettext("No model selected"));
                status_bar.set_recording_status(&gettext("No Model"));
            }
        }
    }

    fn toggle_sidebar(&self) {
        let imp = self.imp();

        // GTK grows a toplevel to satisfy a larger minimum but does not shrink
        // it back automatically. Save GTK's default_size before changing the
        // row layout: the window allocation includes decoration geometry.
        let freely_sized = self.is_mapped()
            && !self.is_maximized()
            && !self.is_fullscreen()
            && !self
                .upcast_ref::<gtk::Window>()
                .surface()
                .and_downcast::<gtk::gdk::Toplevel>()
                .is_some_and(|surface| surface.state().contains(gtk::gdk::ToplevelState::TILED));
        if freely_sized && imp.sidebar_resize_origin.get().is_none() {
            if let Some(sidebar) = imp.sidebar_box.borrow().as_ref() {
                imp.sidebar_resize_origin
                    .set(Some(super::sidebar::resize_origin(self, sidebar)));
                let weak = self.downgrade();
                glib::idle_add_local_once(move || {
                    let Some(window) = weak.upgrade() else { return };
                    let Some((width, height, old_sidebar)) =
                        window.imp().sidebar_resize_origin.take()
                    else {
                        return;
                    };
                    if window.is_maximized() || window.is_fullscreen() {
                        return;
                    }
                    let sidebar = window.imp().sidebar_box.borrow().clone();
                    if let Some(sidebar) = sidebar {
                        // Use the sidebar's layout request, not an allocation
                        // from the previous frame.
                        let (_, new_sidebar, _, _) =
                            sidebar.measure(gtk::Orientation::Horizontal, -1);
                        window.set_default_size(
                            super::sidebar::resized_window_width(width, old_sidebar, new_sidebar),
                            height,
                        );
                    }
                });
            }
        }

        let is_collapsed = imp.sidebar_collapsed.get();
        let new_collapsed = !is_collapsed;
        imp.sidebar_collapsed.set(new_collapsed);

        if let Some(sidebar_box) = imp.sidebar_box.borrow().as_ref() {
            if new_collapsed {
                sidebar_box.set_width_request(Self::SIDEBAR_COLLAPSED_WIDTH);
                sidebar_box.add_css_class("sidebar-collapsed");
            } else {
                sidebar_box.set_width_request(Self::SIDEBAR_EXPANDED_WIDTH);
                sidebar_box.remove_css_class("sidebar-collapsed");
            }
        }

        if let Some(title_box) = imp.sidebar_title_box.borrow().as_ref() {
            title_box.set_visible(!new_collapsed);
        }

        // Remove section rows entirely in compact mode so they leave no blank
        // gaps between navigation icons.
        for row in imp.sidebar_section_rows.borrow().iter() {
            row.set_visible(!new_collapsed);
        }

        for label in imp.nav_labels.borrow().iter() {
            label.set_visible(!new_collapsed);
        }

        for hbox in imp.nav_boxes.borrow().iter() {
            if new_collapsed {
                hbox.set_spacing(0);
                hbox.set_halign(gtk::Align::Center);
            } else {
                hbox.set_spacing(12);
                hbox.set_halign(gtk::Align::Fill);
            }
        }

        if let Some(info_box) = imp.info_box.borrow().as_ref() {
            info_box.set_visible(!new_collapsed);
        }

        if let Some(gpu_panel) = imp.gpu_panel.borrow().as_ref() {
            gpu_panel.set_visible(!new_collapsed);
        }

        if let Some(btn) = imp.sidebar_toggle_btn.borrow().as_ref() {
            if new_collapsed {
                btn.set_tooltip_text(Some("Expand sidebar"));
                btn.set_icon_name("sidebar-show-right-symbolic");
            } else {
                btn.set_tooltip_text(Some("Collapse sidebar"));
                btn.set_icon_name("sidebar-show-symbolic");
            }
        }
    }

    fn setup_controls(&self) {
        let imp = self.imp();
        let controls = match imp.controls.borrow().as_ref() {
            Some(c) => c.clone(),
            None => return,
        };
        let window = self.clone();

        controls.connect_action(move |action| match action {
            ControlAction::Record => window.on_record(),
            ControlAction::Pause => window.on_pause(),
            ControlAction::Resume => window.on_resume(),
            ControlAction::Stop => window.on_stop(),
            ControlAction::Cancel => window.on_cancel(),
            ControlAction::OpenFile => window.on_open_file(),
            ControlAction::Copy => window.on_copy(),
            ControlAction::Clear => window.on_clear(),
            ControlAction::Save => window.on_save(),
        });

        if let Some(transcript_view) = imp.transcript_view.borrow().as_ref() {
            let window = self.clone();
            transcript_view.connect_action(move |action| match action {
                ControlAction::Stop => window.on_stop(),
                ControlAction::Copy => window.on_copy(),
                ControlAction::Clear => window.on_clear(),
                ControlAction::Save => window.on_save(),
                ControlAction::OpenFile => window.on_open_file(),
                ControlAction::Record => window.on_record(),
                ControlAction::Pause => window.on_pause(),
                ControlAction::Resume => window.on_resume(),
                ControlAction::Cancel => window.on_cancel(),
            });
        }
    }

    /// Wire drag-and-drop of audio files onto the transcript view.
    fn setup_file_drop(&self) {
        let imp = self.imp();
        let tv = match imp.transcript_view.borrow().as_ref() {
            Some(tv) => tv.clone(),
            None => return,
        };

        let window = self.clone();
        tv.connect_file_dropped(move |path| {
            window.transcribe_file(path);
        });
    }

    /// Transcribe an audio file dropped onto the view.
    /// Gather the current Whisper inference parameters from the settings widgets.
    /// Shared by the live-recording and file-drop transcription paths.
    fn gather_whisper_params(&self) -> WhisperRunParams {
        let imp = self.imp();
        let n_threads = imp
            .performance_page
            .borrow()
            .as_ref()
            .map(|p| p.get_thread_count())
            .unwrap_or(num_cpus::get().min(8) as u32);
        let beam_size = imp
            .performance_page
            .borrow()
            .as_ref()
            .map(|p| p.get_beam_size())
            .unwrap_or(5);
        let temperature = imp
            .performance_page
            .borrow()
            .as_ref()
            .map(|p| p.get_temperature())
            .unwrap_or(0.0);
        let translate = imp
            .controls
            .borrow()
            .as_ref()
            .map(|c| c.is_translate_active())
            .unwrap_or_else(|| {
                imp.language_page
                    .borrow()
                    .as_ref()
                    .map(|p| p.is_translate_enabled())
                    .unwrap_or(false)
            });
        let initial_prompt = imp
            .performance_page
            .borrow()
            .as_ref()
            .and_then(|p| p.get_initial_prompt());
        WhisperRunParams {
            n_threads,
            beam_size,
            temperature,
            translate,
            initial_prompt,
        }
    }

    fn transcribe_file(&self, path: std::path::PathBuf) {
        let imp = self.imp();

        if let Some(sb) = imp.status_bar.borrow().as_ref() {
            sb.set_recording_status(&gettext("Decoding file…"));
        }

        let backend = self.active_backend();

        // Backend readiness up front (matches the live path's UX).
        if backend == "parakeet" && !crate::transcription::parakeet::parakeet_ready() {
            self.show_toast(&gettext(
                "Parakeet is not set up. Go to Settings → Model to download the runtime and model.",
            ));
            return;
        }
        if backend == "qwen" && !crate::transcription::qwen::qwen_ready() {
            self.show_toast(&gettext("Qwen3-ASR is not set up. Go to Settings → Model to download the runtime and model."));
            return;
        }

        let Some(controller) = self.controller() else {
            return;
        };
        if controller.is_recording() {
            self.show_toast(&gettext("Stop the current recording first"));
            return;
        }
        let generation = self.begin_operation();
        let engine = controller.engine_arc();
        let cancel = self.imp().work_cancel.borrow().clone();
        let params = self.build_dictation_params(backend);
        let source_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Audio file")
            .to_string();

        if let Some(tv) = imp.transcript_view.borrow().as_ref() {
            tv.start_transcribing_anim();
        }

        let (sender, receiver) = async_channel::bounded(1);

        // Decode the file and run the shared transcription core on a worker thread.
        // A panic inside the decoder (a malformed file is untrusted input) or
        // the engine must still produce a reply: with the sender dropped on
        // unwind, the window kept its decode animation and "Decoding file…"
        // status forever.
        std::thread::spawn(move || {
            let mut retained = None;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let audio_data = Arc::new(
                    crate::audio::file_decoder::decode_audio_file_cancellable(&path, Some(&cancel))
                        .map_err(|e| format!("Failed to decode file: {}", e))?,
                );
                retained = Some(WindowRetry {
                    audio: RetryAudio::File(audio_data.clone()),
                    params: params.clone(),
                    duration: audio_data.source_duration_secs,
                });
                // No mic time for a file: derive duration from the decoded mono-16k length.
                let duration_secs = audio_data.source_duration_secs;
                crate::recording::run_transcription(
                    &engine,
                    &audio_data.samples,
                    &params,
                    duration_secs,
                    Some(cancel.clone()),
                )
                .map(|mut outcome| {
                    outcome.shift_timestamps(audio_data.leading_offset_ms);
                    outcome
                })
            }))
            .unwrap_or_else(|_| {
                tracing::error!("File transcription panicked; reporting it as an error");
                Err(gettext(
                    "Transcription crashed unexpectedly — please try again.",
                ))
            });
            let _ = sender.send_blocking((result, retained));
        });

        let window = self.clone();
        glib::spawn_future_local(async move {
            // A dropped sender (the worker thread died without replying) is
            // reported like any other failure instead of leaving the UI stuck.
            let (result, retained) = receiver.recv().await.unwrap_or_else(|_| {
                (
                    Err(gettext(
                        "Transcription failed unexpectedly — please try again.",
                    )),
                    None,
                )
            });
            {
                if !window.operation_is_current(generation) {
                    return;
                }
                if let Some(tv) = window.imp().transcript_view.borrow().as_ref() {
                    tv.stop_transcribing_anim();
                }
                match result {
                    Ok(outcome) => {
                        let duration_secs = outcome.duration_secs;
                        let confidence = outcome.confidence;
                        let detected = outcome.detected_language.clone();
                        let segments = outcome.segments;
                        let cleaned = outcome.cleaned_text;
                        if cleaned.is_empty() && outcome.raw_text.trim().is_empty() {
                            *window.imp().last_segments.borrow_mut() = Vec::new();
                            if let Some(sb) = window.imp().status_bar.borrow().as_ref() {
                                sb.set_recording_status(&gettext("Ready"));
                            }
                            window.show_toast(&gettext("No clear speech detected in the audio"));
                            return;
                        }
                        if let Some(sb) = window.imp().status_bar.borrow().as_ref() {
                            sb.set_recording_status(&gettext("Ready"));
                        }
                        *window.imp().last_segments.borrow_mut() = segments.clone();
                        // Surface the auto-detected language (only set when auto-detect was used).
                        if let Some(code) = detected.as_deref() {
                            window.set_language_display(&format!(
                                "Auto-detect ({})",
                                language_code_to_name(code)
                            ));
                        }
                        // Render as the current result (text + confidence + stats +
                        // clipboard + chips/selector).
                        let mut state = crate::ui::result_state::ResultState::from_transcript(
                            outcome.raw_text.clone(),
                            cleaned.clone(),
                            duration_secs,
                            detected.clone(),
                            segments,
                            &gettext("Cleaned"),
                        );
                        state.history_id = Some(outcome.id.clone());
                        window.add_result_message(
                            state,
                            outcome.confidence_available.then_some(confidence as f64),
                        );
                        if let Some(history) = window.imp().history_page.borrow().as_ref() {
                            let config = AppConfig::load();
                            let language = detected
                                .as_deref()
                                .map(|code| {
                                    format!("Auto-detect ({})", language_code_to_name(code))
                                })
                                .or_else(|| config.language.as_deref().map(language_code_to_name))
                                .unwrap_or_else(|| "Auto-detect".to_string());
                            history.add_entry(crate::ui::history_page::HistoryEntry {
                                id: outcome.id.clone(),
                                title: source_name.clone(),
                                text: cleaned.clone(),
                                raw_text: Some(outcome.raw_text),
                                language,
                                duration_secs: duration_secs.round() as u64,
                                timestamp: chrono::Local::now()
                                    .format("%Y-%m-%d %H:%M")
                                    .to_string(),
                                model: outcome.diagnostics.model.clone(),
                                word_count: Some(
                                    crate::ui::result_state::word_count(&cleaned) as u32
                                ),
                                ..Default::default()
                            });
                        }
                        window.show_toast(&gettext("File transcription complete"));
                        window.maybe_generate_summary(&cleaned, duration_secs);
                        window.maybe_auto_improve(&cleaned);
                    }
                    Err(msg) => {
                        *window.imp().retry_take.borrow_mut() =
                            if window.is_visible() { retained } else { None };
                        window.show_retry(window.imp().retry_take.borrow().is_some());
                        window.show_toast(&msg);
                        if let Some(sb) = window.imp().status_bar.borrow().as_ref() {
                            sb.set_recording_status(&gettext("Error"));
                        }
                    }
                }
            }
        });
    }

    /// Wire the "Default Engine" combo in the Model tab: switching it updates the
    /// header model dropdown, status bar, capability gating, and model loading.
    fn setup_engine_selector(&self) {
        let imp = self.imp();

        // Initial sync of the header model list + capabilities from config.
        // Guarded so the internal `set_selected(0)` never persists over the
        // saved model (the model-changed handler may already be connected on
        // later calls / reorderings).
        if let Some(config) = imp.config.borrow().as_ref() {
            let backend = config.backend.clone();
            if let Some(header) = imp.header_controls.borrow().as_ref() {
                let downloaded = Self::downloaded_model_entries();
                imp.syncing_dropdown.set(true);
                header.update_models_for_backend(&backend, &downloaded);
                imp.syncing_dropdown.set(false);
            }
            self.apply_backend_capabilities(&backend);
        }

        let model_page = match imp.model_page.borrow().as_ref() {
            Some(p) => p.clone(),
            None => return,
        };

        let window = self.clone();
        model_page.connect_engine_changed(move |backend| {
            if window.imp().syncing_backend.get() {
                return;
            }

            if let Some(mut new_config) = window.current_config_snapshot() {
                new_config.backend = backend.clone();
                new_config.save();
                window.replace_config(new_config);
            }

            // Any backend change invalidates the warm sidecar server (a new
            // one spawns lazily on the next dictation if needed) — freeing its
            // multi-GB weights immediately, mirroring the engine drop below.
            crate::transcription::sidecar_server::shutdown_all();
            if backend != "whisper" {
                // Non-Whisper backends (Parakeet, Qwen3-ASR) don't use the loaded
                // Whisper engine — drop it to free memory.
                if let Some(engine_arc) = window.imp().engine.borrow().as_ref().cloned() {
                    if let Ok(mut guard) = engine_arc.lock() {
                        *guard = None;
                    }
                }
                tracing::info!("Switched transcription engine to {}", backend);
            } else {
                tracing::info!("Switched transcription engine to Whisper");
            }

            window.sync_ui_from_config();

            if backend == "whisper" {
                if let Some(config) = window.current_config_snapshot() {
                    window.load_model_by_id(&config.selected_model);
                }
            }
        });
    }

    /// Apply UI capability constraints based on the selected backend.
    fn apply_backend_capabilities(&self, backend: &str) {
        let imp = self.imp();
        let is_whisper =
            crate::transcription::capabilities::for_backend(backend).is_some_and(|b| b.translation);
        let supports_auto_detect = matches!(backend, "whisper" | "qwen" | "parakeet");

        // Controls: translate toggle (Whisper-only) — greyed out, not hidden.
        if let Some(controls) = imp.controls.borrow().as_ref() {
            controls.set_translate_enabled(is_whisper);
        }

        if let Some(page) = imp.language_page.borrow().as_ref() {
            page.set_auto_detect_available(supports_auto_detect);
            page.set_automatic_only(backend == "parakeet");
            page.set_translation_available(is_whisper);
        }

        if let Some(page) = imp.performance_page.borrow().as_ref() {
            page.set_whisper_settings_available(is_whisper);
        }
    }

    /// Get downloaded Whisper models as (id, display_name) pairs for the dropdown.
    fn downloaded_model_entries() -> Vec<(String, String)> {
        let catalog = ModelCatalog::new();
        let mut entries = Vec::new();
        for model in catalog.models() {
            if ModelCatalog::is_downloaded(&model.id) {
                entries.push((model.id.clone(), model.display_name.clone()));
            }
        }
        entries
    }

    /// Wire model dropdown changes: when user selects a model, load it.
    fn setup_model_dropdown(&self) {
        let imp = self.imp();
        let header = match imp.header_controls.borrow().as_ref() {
            Some(h) => h.clone(),
            None => return,
        };

        // Sync dropdown with config — find the current model's index in the downloaded list
        if let Some(config) = imp.config.borrow().as_ref() {
            let downloaded = Self::downloaded_model_entries();
            let index = downloaded
                .iter()
                .position(|(id, _)| id == &config.selected_model)
                .unwrap_or(0) as u32;
            imp.syncing_dropdown.set(true);
            header.set_selected_model(index);
            imp.syncing_dropdown.set(false);
        }

        let window = self.clone();
        header.connect_model_changed(move |model_id| {
            // Skip if this change was triggered programmatically
            if window.imp().syncing_dropdown.get() {
                return;
            }

            let backend = window
                .current_config_snapshot()
                .map(|c| c.backend)
                .unwrap_or_else(|| "whisper".to_string());

            // For Qwen the dropdown selects the model SIZE ("0.6B"/"1.7B").
            if backend == "qwen" {
                if let Some(mut cfg) = window.current_config_snapshot() {
                    cfg.qwen_model_size = model_id.clone();
                    cfg.save();
                    window.replace_config(cfg);
                }
                window.sync_ui_from_config();
                return;
            }

            // Parakeet has no selectable model variant.
            if backend != "whisper" {
                return;
            }

            // Whisper: the model_id is the resolved downloaded model ID.
            if let Some(mut new_config) = window.current_config_snapshot() {
                new_config.selected_model = model_id.clone();
                new_config.save();
                window.replace_config(new_config);
            }
            window.load_model_by_id(&model_id);
        });
    }

    fn setup_language_display(&self) {
        let imp = self.imp();

        let language_page = match imp.language_page.borrow().as_ref() {
            Some(p) => p.clone(),
            None => return,
        };

        if let Some(config) = imp.config.borrow().as_ref() {
            if config.auto_detect_language {
                self.set_language_display("Auto-detect");
            } else if let Some(ref lang) = config.language {
                self.set_language_display(&language_code_to_name(lang));
            }
        }

        let window = self.clone();
        language_page.connect_language_changed(move |lang_name| {
            window.set_language_display(&lang_name);
        });

        let controls = match imp.controls.borrow().as_ref() {
            Some(c) => c.clone(),
            None => return,
        };

        // Translate is shared by the main window AND the mini panel / global
        // dictation, so persist it to config and keep both toggles in sync. A
        // guard prevents the two `set_*` calls from ping-ponging.
        let window = self.clone();
        let lang_page_ref = language_page.clone();
        controls.connect_translate_changed(move |active| {
            if window.imp().syncing_translate.get() {
                return;
            }
            window.imp().syncing_translate.set(true);
            lang_page_ref.set_translate_enabled(active);
            window.imp().syncing_translate.set(false);
            let mut c = AppConfig::load();
            c.translate_to_english = active;
            c.save();
        });

        let window = self.clone();
        let controls_ref = controls.clone();
        language_page.connect_translate_changed(move |active| {
            if window.imp().syncing_translate.get() {
                return;
            }
            window.imp().syncing_translate.set(true);
            controls_ref.set_translate_active(active);
            window.imp().syncing_translate.set(false);
            let mut c = AppConfig::load();
            c.translate_to_english = active;
            c.save();
        });

        // Restore the saved translate state into both toggles (guarded so it
        // doesn't re-trigger a save). Read AppConfig::load(), not imp.config:
        // this runs from setup_ui() during GObject construction, before
        // MainWindow::new() stores imp.config, so gating on imp.config
        // silently skipped the restore — the toggles then showed OFF while a
        // persisted `translate_to_english = true` kept translating dictation
        // on the config-driven paths (mini panel / global shortcut).
        let config = AppConfig::load();
        imp.syncing_translate.set(true);
        controls.set_translate_active(config.translate_to_english);
        language_page.set_translate_enabled(config.translate_to_english);
        imp.syncing_translate.set(false);
    }

    fn on_record(&self) {
        let imp = self.imp();
        let Some(controller) = self.controller() else {
            return;
        };

        let backend = self.active_backend();
        let backend_ready = match backend.as_str() {
            "parakeet" => crate::transcription::parakeet::parakeet_ready(),
            "qwen" => crate::transcription::qwen::qwen_ready(),
            _ => controller
                .engine_arc()
                .lock()
                .map(|engine| engine.is_some())
                .unwrap_or(false),
        };
        if !backend_ready {
            self.show_toast(&gettext("The selected transcription model is not ready. Open Model settings and finish setup first."));
            self.navigate_to(NavItem::Model);
            return;
        }

        // Don't start if another owner (e.g. global dictation) is recording.
        if !controller.try_acquire(RecordingOwner::Main) {
            return;
        }
        if let Some(app) = self.application().and_downcast::<Application>() {
            app.cancel_pending_paste();
        }

        let (waveform_tx, waveform_rx) = async_channel::bounded::<Vec<f32>>(32);

        // AudioCapture contains a cpal::Stream which is !Send, but start is fast
        // (sets up the stream), so it runs on the main thread via the controller.
        let selected_device = self
            .current_config_snapshot()
            .and_then(|config| config.selected_microphone);
        let result = controller.start(selected_device.as_deref(), waveform_tx);

        match result {
            Ok(()) => {
                let generation = self.begin_operation();
                if let Some(controls) = imp.controls.borrow().as_ref() {
                    controls.set_recording_state(true);
                }
                if let Some(tv) = imp.transcript_view.borrow().as_ref() {
                    tv.set_recording(true);
                    // Hide the previous result's chips/selector while recording.
                    tv.hide_result_controls();
                }
                if let Some(sb) = imp.status_bar.borrow().as_ref() {
                    sb.set_recording_status(&gettext("Recording…"));
                }
                self.start_recording_timer();

                let window = self.downgrade();
                glib::spawn_future_local(async move {
                    while let Ok(amplitudes) = waveform_rx.recv().await {
                        let Some(win) = window.upgrade() else { break };
                        let tv = win.imp().transcript_view.borrow().clone();
                        if let Some(tv) = tv.as_ref() {
                            tv.update_waveform(amplitudes);
                        }
                    }
                });

                // Live (while-speaking) transcription: periodically re-decode the
                // audio captured so far and show it as tentative text. Whisper
                // only; the authoritative result is produced on Stop.
                if AppConfig::load().live_transcription
                    && crate::transcription::capabilities::for_backend(&self.active_backend())
                        .is_some_and(|b| b.live_preview)
                {
                    self.imp().live_decoding.set(false);
                    self.imp().live_too_slow.set(false);
                    self.start_live_loop(generation);
                }

                tracing::info!("Recording started");
            }
            Err(e) => {
                controller.release();
                self.show_toast(&format!("Failed to start recording: {}", e));
            }
        }
    }

    /// Periodic live decode of the audio captured so far, shown as tentative text
    /// while recording continues. Greedy (beam=1) + plain formatting for low
    /// latency; never overlaps itself; stops when recording ends. The final,
    /// authoritative result is produced by `on_stop`.
    fn start_live_loop(&self, generation: u64) {
        let window = self.downgrade();
        glib::timeout_add_local(std::time::Duration::from_millis(2000), move || {
            let Some(win) = window.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if !win.operation_is_current(generation) {
                return glib::ControlFlow::Break;
            }
            let Some(controller) = win.controller() else {
                return glib::ControlFlow::Break;
            };
            // Stop once recording ends (or another owner took over).
            if controller.owner() != RecordingOwner::Main
                || controller.state() == crate::audio::capture::RecordingState::Idle
            {
                return glib::ControlFlow::Break;
            }
            // Self-protect: if a previous live decode was too slow or failed, stop
            // the loop so we never flood a slow / VRAM-tight GPU (e.g. large-v3).
            // The final on-stop decode still produces the full transcript.
            if win.imp().live_too_slow.get() {
                return glib::ControlFlow::Break;
            }
            // Skip while a previous live decode is still running.
            if win.imp().live_decoding.get() {
                return glib::ControlFlow::Continue;
            }
            const LIVE_WINDOW: usize = 12 * 16_000;
            // Cheap O(n) copy of the raw tail under the lock; conditioning
            // (mono + sinc resample) runs on the inference worker —
            // resampling ~13s of audio on the GTK thread every tick caused
            // visible jank throughout every live-preview recording. The raw
            // tail is already bounded to the ~12s window (+1s context), so
            // decode cost stays capped; the final on-stop pass still decodes
            // the whole take at full quality.
            let Some(snapshot) = controller.live_snapshot_raw(LIVE_WINDOW) else {
                return glib::ControlFlow::Continue;
            };
            // Need at least ~1s of audio to bother.
            if snapshot.duration_secs() < 1.0 {
                return glib::ControlFlow::Continue;
            }
            win.imp().live_decoding.set(true);
            let mut params = DictationParams::from_config(&AppConfig::load());
            params.mode = DictationMode::Plain; // show raw-ish text live
                                                // Greedy, as documented: a beam-5 live decode costs 2-3x more and
                                                // trips the 3.5s self-disable cutoff on hardware where greedy would
                                                // have kept the preview alive. The final on-stop decode still uses
                                                // the configured beam size.
            params.beam_size = 1;
            params.temperature = 0.0;
            let dur = controller.recording_duration_secs();
            let started = std::time::Instant::now();
            let rx = controller.transcribe_snapshot_async(
                snapshot,
                params,
                dur,
                win.imp().work_cancel.borrow().clone(),
            );
            let win2 = win.downgrade();
            glib::spawn_future_local(async move {
                let res = rx.recv().await;
                let Some(win) = win2.upgrade() else { return };
                if !win.operation_is_current(generation) {
                    return;
                }
                win.imp().live_decoding.set(false);
                match res {
                    Ok(Ok(outcome)) => {
                        // If even a 12s greedy decode is slow, this hardware/model
                        // can't keep up live — disable the loop for this recording.
                        if started.elapsed().as_secs_f32() > 3.5 {
                            win.imp().live_too_slow.set(true);
                        }
                        let still_recording = win
                            .controller()
                            .map(|c| c.owner() == RecordingOwner::Main)
                            .unwrap_or(false);
                        if !still_recording {
                            return;
                        }
                        let text = outcome.cleaned_text;
                        if !text.trim().is_empty() {
                            if let Some(tv) = win.imp().transcript_view.borrow().as_ref() {
                                tv.set_live_preview(&text);
                            }
                        }
                    }
                    // A silence-only window conditions down to empty audio and
                    // comes back as "No clear speech detected". That is not a
                    // decode failure — the user just hasn't spoken yet — so
                    // keep the loop alive and try again on the next tick.
                    Ok(Err(msg)) if msg.starts_with("No clear speech") => {}
                    // Any real failure (e.g. "failed to encode" on a VRAM-tight
                    // GPU): stop the live loop silently so it can't repeat.
                    _ => win.imp().live_too_slow.set(true),
                }
            });
            glib::ControlFlow::Continue
        });
    }

    fn on_pause(&self) {
        let imp = self.imp();
        if let Some(controller) = self.controller() {
            controller.pause();
        }
        if let Some(tv) = imp.transcript_view.borrow().as_ref() {
            tv.set_paused_state(true);
        }
        if let Some(sb) = imp.status_bar.borrow().as_ref() {
            sb.set_recording_status(&gettext("Paused"));
        }
    }

    fn on_resume(&self) {
        let imp = self.imp();
        if let Some(controller) = self.controller() {
            controller.resume();
        }
        if let Some(tv) = imp.transcript_view.borrow().as_ref() {
            tv.set_paused_state(false);
        }
        if let Some(sb) = imp.status_bar.borrow().as_ref() {
            sb.set_recording_status(&gettext("Recording…"));
        }
    }

    fn on_stop(&self) {
        let imp = self.imp();
        let Some(controller) = self.controller() else {
            return;
        };

        // Capture the mic duration BEFORE stopping (stop drains the buffer, and
        // silence-trimming makes the conditioned sample count shorter than the
        // real recording time). Needed for words-per-minute stats.
        let duration_secs = controller.recording_duration_secs();

        let audio_snapshot = match controller.stop_snapshot() {
            Ok(snapshot) => snapshot,
            Err(e) => {
                controller.release();
                self.show_toast(&format!("Error stopping recording: {}", e));
                return;
            }
        };
        controller.release();

        if let Some(controls) = imp.controls.borrow().as_ref() {
            controls.set_recording_state(false);
        }
        if let Some(tv) = imp.transcript_view.borrow().as_ref() {
            tv.set_recording(false);
            tv.set_paused_state(false);
        }

        let backend = self.active_backend();

        if let Some(sb) = imp.status_bar.borrow().as_ref() {
            sb.set_recording_status(&match backend.as_str() {
                "parakeet" => gettext("Transcribing with Parakeet…"),
                "qwen" => gettext("Transcribing with Qwen3-ASR…"),
                _ => gettext("Transcribing with Whisper…"),
            });
        }

        // Backend readiness is checked up front so we can show the dedicated
        // "Setup Required" status (the controller also guards internally).
        let params = self.build_dictation_params(backend.clone());
        *imp.retry_take.borrow_mut() = Some(WindowRetry {
            audio: RetryAudio::Mic(audio_snapshot.clone()),
            params: params.clone(),
            duration: duration_secs,
        });
        if backend == "parakeet" && !crate::transcription::parakeet::parakeet_ready() {
            self.show_retry(true);
            self.show_toast(&gettext(
                "Parakeet is not set up. Go to Settings → Model to download the runtime and model.",
            ));
            if let Some(sb) = imp.status_bar.borrow().as_ref() {
                sb.set_recording_status(&gettext("Setup Required"));
            }
            return;
        }
        if backend == "qwen" && !crate::transcription::qwen::qwen_ready() {
            self.show_retry(true);
            self.show_toast(&gettext("Qwen3-ASR is not set up. Go to Settings → Model to download the runtime and model."));
            if let Some(sb) = imp.status_bar.borrow().as_ref() {
                sb.set_recording_status(&gettext("Setup Required"));
            }
            return;
        }

        tracing::info!("Using {} backend for live transcription", backend);
        if let Some(tv) = imp.transcript_view.borrow().as_ref() {
            tv.start_transcribing_anim();
        }
        // Final, authoritative decode. (When live transcription is on, the live
        // preview already ran *during* recording via the live loop; here we always
        // use the clean batch path so the result + chips/toggle/stats render
        // reliably — the previous on-stop streaming path could hide the chips with
        // a late segment update.)
        let generation = self.imp().operation_generation.get();
        let receiver = controller.transcribe_snapshot_async(
            audio_snapshot,
            params,
            duration_secs,
            self.imp().work_cancel.borrow().clone(),
        );
        let window = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(result) = receiver.recv().await {
                if !window.operation_is_current(generation) {
                    return;
                }
                window.apply_transcription_result(result);
            }
        });
    }

    /// Finalize a live transcription result: render it, save to history, and run
    /// the summary / auto-improve passes. Shared by the streaming and batch paths.
    fn apply_transcription_result(
        &self,
        result: Result<crate::recording::DictationOutcome, String>,
    ) {
        if let Some(tv) = self.imp().transcript_view.borrow().as_ref() {
            tv.stop_transcribing_anim();
        }
        match result {
            Ok(outcome) => {
                self.imp().retry_take.borrow_mut().take();
                self.show_retry(false);
                let duration_secs = outcome.duration_secs;
                let confidence = outcome.confidence;
                let detected = outcome.detected_language.clone();
                let segments = outcome.segments;
                let cleaned = outcome.cleaned_text;
                if cleaned.is_empty() && outcome.raw_text.trim().is_empty() {
                    *self.imp().last_segments.borrow_mut() = Vec::new();
                    if let Some(tv) = self.imp().transcript_view.borrow().as_ref() {
                        tv.clear_live_preview();
                    }
                    if let Some(sb) = self.imp().status_bar.borrow().as_ref() {
                        sb.set_recording_status(&gettext("Ready"));
                    }
                    self.show_toast(&gettext(
                        "No clear speech detected — try again in a quieter environment",
                    ));
                    return;
                }
                if let Some(sb) = self.imp().status_bar.borrow().as_ref() {
                    sb.set_recording_status(&gettext("Ready"));
                }
                *self.imp().last_segments.borrow_mut() = segments.clone();
                // Surface the auto-detected language (only set when auto-detect was used).
                if let Some(code) = detected.as_deref() {
                    self.set_language_display(&format!(
                        "Auto-detect ({})",
                        language_code_to_name(code)
                    ));
                }
                // Render as the current result (text + confidence + stats +
                // clipboard + chips/selector).
                let mut state = crate::ui::result_state::ResultState::from_transcript(
                    outcome.raw_text.clone(),
                    cleaned.clone(),
                    duration_secs,
                    detected.clone(),
                    segments,
                    &gettext("Cleaned"),
                );
                state.history_id = Some(outcome.id.clone());
                self.add_result_message(
                    state,
                    outcome.confidence_available.then_some(confidence as f64),
                );
                if let Some(hp) = self.imp().history_page.borrow().as_ref() {
                    // Prefer the language Whisper actually detected (set only
                    // under auto-detect); the file and panel paths record it the
                    // same way, and a bare "Auto-detect" tells the user nothing.
                    let lang_name = detected
                        .as_deref()
                        .map(|code| format!("Auto-detect ({})", language_code_to_name(code)))
                        .or_else(|| {
                            self.imp()
                                .language_page
                                .borrow()
                                .as_ref()
                                .map(|p| p.selected_language_name())
                        })
                        .unwrap_or_else(|| "Auto-detect".to_string());
                    let model_name = outcome.diagnostics.model.clone();
                    let title = Self::ellipsize_chars(
                        if cleaned.is_empty() {
                            &outcome.raw_text
                        } else {
                            &cleaned
                        },
                        60,
                    );
                    let entry_id = outcome.id.clone();
                    let entry = crate::ui::history_page::HistoryEntry {
                        id: entry_id.clone(),
                        title,
                        text: cleaned.clone(),
                        raw_text: Some(outcome.raw_text),
                        language: lang_name,
                        duration_secs: duration_secs.round() as u64,
                        timestamp: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
                        model: model_name,
                        word_count: Some(crate::ui::result_state::word_count(&cleaned) as u32),
                        ..Default::default()
                    };
                    hp.add_entry(entry);
                    if let Some(app) = self
                        .application()
                        .and_downcast::<crate::application::Application>()
                    {
                        app.auto_title(entry_id, cleaned.clone());
                    }
                }
                tracing::info!(
                    "Transcription complete ({:.0}% confidence)",
                    confidence * 100.0
                );
                // A GPU decode may have retried and fallen back on this run.
                self.refresh_compute_mode();
                // Summarize long dictations; auto-improve with the active preset
                // (adds a variant; the raw transcript stays available + in History).
                self.maybe_generate_summary(&cleaned, duration_secs);
                self.maybe_auto_improve(&cleaned);
            }
            Err(msg) => {
                self.show_retry(self.imp().retry_take.borrow().is_some());
                if let Some(tv) = self.imp().transcript_view.borrow().as_ref() {
                    tv.clear_live_preview();
                }
                self.show_toast(&msg);
                if let Some(sb) = self.imp().status_bar.borrow().as_ref() {
                    sb.set_recording_status(&gettext("Error"));
                }
                self.refresh_compute_mode();
            }
        }
    }

    fn on_cancel(&self) {
        let imp = self.imp();
        let Some(controller) = self.controller() else {
            return;
        };
        if !matches!(
            controller.owner(),
            RecordingOwner::Main | RecordingOwner::None
        ) {
            return;
        }

        self.begin_operation();

        controller.cancel();
        controller.release();

        // Reset UI
        if let Some(controls) = imp.controls.borrow().as_ref() {
            controls.set_recording_state(false);
        }
        if let Some(tv) = imp.transcript_view.borrow().as_ref() {
            tv.set_recording(false);
            tv.set_paused_state(false);
            tv.clear_live_preview();
            tv.stop_transcribing_anim();
        }
        if let Some(sb) = imp.status_bar.borrow().as_ref() {
            sb.set_recording_status(&gettext("Ready"));
        }

        self.show_toast(&gettext("Recording cancelled"));
        tracing::info!("Recording cancelled by user");
    }

    fn on_copy(&self) {
        let imp = self.imp();
        if let Some(tv) = imp.transcript_view.borrow().as_ref() {
            let text = tv.get_text();
            if !text.is_empty() {
                if let Some(display) = gtk::gdk::Display::default() {
                    display.clipboard().set_text(&text);
                    self.show_toast(&gettext("Copied to clipboard"));
                }
            }
        }
    }

    fn on_clear(&self) {
        self.begin_operation();
        self.imp().messages.borrow_mut().clear();
        self.imp().selected_message.set(-1);
        if let Some(tv) = self.imp().transcript_view.borrow().as_ref() {
            tv.clear();
        }
    }

    fn on_open_file(&self) {
        if let Some(controller) = self.controller() {
            if controller.is_recording() {
                self.show_toast(&gettext("Stop the current recording first"));
                return;
            }
        }

        let audio_filter = gtk::FileFilter::new();
        audio_filter.set_name(Some(gettext("Audio files").as_str()));
        for pat in [
            "*.wav", "*.mp3", "*.flac", "*.ogg", "*.oga", "*.opus", "*.m4a",
        ] {
            audio_filter.add_pattern(pat);
        }
        let any_filter = gtk::FileFilter::new();
        any_filter.set_name(Some(gettext("All files").as_str()));
        any_filter.add_pattern("*");

        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&audio_filter);
        filters.append(&any_filter);

        let dialog = gtk::FileDialog::builder()
            .title(gettext("Open Audio File").as_str())
            .filters(&filters)
            .default_filter(&audio_filter)
            .modal(true)
            .build();

        let window = self.clone();
        dialog.open(
            Some(&window.clone()),
            gtk::gio::Cancellable::NONE,
            move |result| {
                if let Ok(file) = result {
                    if let Some(path) = file.path() {
                        window.transcribe_file(path);
                    }
                }
            },
        );
    }

    fn on_save(&self) {
        let imp = self.imp();
        let text = match imp.transcript_view.borrow().as_ref() {
            Some(tv) => tv.get_text(),
            None => return,
        };
        if text.is_empty() {
            self.show_toast(&gettext("Nothing to save"));
            return;
        }

        let segments = imp.last_segments.borrow().clone();

        let txt_filter = gtk::FileFilter::new();
        txt_filter.set_name(Some("Text file (.txt)"));
        txt_filter.add_pattern("*.txt");

        let srt_filter = gtk::FileFilter::new();
        srt_filter.set_name(Some("SRT subtitles (.srt)"));
        srt_filter.add_pattern("*.srt");

        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&txt_filter);
        filters.append(&srt_filter);

        let dialog = gtk::FileDialog::builder()
            .title("Save Transcript")
            .initial_name("transcript.txt")
            .filters(&filters)
            .build();

        let window = self.clone();
        dialog.save(
            Some(&window.clone()),
            gtk::gio::Cancellable::NONE,
            move |result| {
                if let Ok(file) = result {
                    if let Some(path) = file.path() {
                        let content = if path.extension().is_some_and(|e| e == "srt")
                            && !segments.is_empty()
                        {
                            let seg_refs: Vec<(i64, i64, &str)> = segments
                                .iter()
                                .map(|(s, e, t)| (*s, *e, t.as_str()))
                                .collect();
                            postprocess::format_as_srt(&seg_refs)
                        } else {
                            text.clone()
                        };
                        match std::fs::write(&path, &content) {
                            Ok(()) => window.show_toast(&gettext("Transcript saved")),
                            Err(e) => window.show_toast(&format!("Failed to save: {}", e)),
                        }
                    }
                }
            },
        );
    }

    fn start_recording_timer(&self) {
        let window = self.downgrade();
        glib::timeout_add_seconds_local(1, move || {
            let Some(window) = window.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let Some(controller) = window.controller() else {
                return glib::ControlFlow::Break;
            };
            // This timer belongs to a Main-owned recording. If ownership moved
            // (the user stopped and immediately started a panel dictation or
            // voice edit within the same tick), stop — and never consume
            // take_stream_error for a session someone else's poller owns.
            if controller.owner() != RecordingOwner::Main {
                return glib::ControlFlow::Break;
            }
            if controller.state() == crate::audio::capture::RecordingState::Idle {
                return glib::ControlFlow::Break;
            }
            // A dead stream (mic unplugged, backend restart) never delivers
            // audio again: auto-stop and transcribe what was captured instead
            // of showing a live "Recording" state forever.
            if let Some(err) = controller.take_stream_error() {
                tracing::warn!("Audio stream died mid-recording: {err}");
                window.show_toast(&gettext("Microphone stopped — finishing the recording."));
                window.on_stop();
                return glib::ControlFlow::Break;
            }
            let secs = controller.recording_duration_secs() as u64;
            if let Some(tv) = window.imp().transcript_view.borrow().as_ref() {
                tv.set_timer(secs);
            }
            glib::ControlFlow::Continue
        });
    }

    fn create_nav_row(&self, nav_item: NavItem) -> (gtk::ListBoxRow, gtk::Label, gtk::Box) {
        super::sidebar::navigation_row(&nav_item.title(), nav_item.icon_name())
    }

    /// Keep each preferences page's scrolling and bindings inside a tabbed group.
    fn grouped_settings(pages: &[(&str, String, &gtk::Widget)]) -> (gtk::Box, gtk::Stack) {
        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        // A persistent, labelled menu keeps every settings section visible.
        container.add_css_class("settings-tabs-container");

        let stack = gtk::Stack::new();
        stack.set_vexpand(true);
        stack.set_hexpand(true);
        stack.set_vhomogeneous(false);
        for (name, title, page) in pages {
            stack.add_titled(*page, Some(name), title);
        }
        let tabs = gtk::StackSwitcher::new();
        tabs.set_stack(Some(&stack));
        tabs.add_css_class("settings-tabs");
        tabs.add_css_class("mode-switcher");
        tabs.set_halign(gtk::Align::Start);
        tabs.set_hexpand(false);
        tabs.set_margin_top(12);
        tabs.set_margin_bottom(12);
        tabs.set_margin_start(12);
        tabs.set_margin_end(12);
        container.append(&tabs);
        container.append(&stack);
        (container, stack)
    }

    /// Navigate to a page, resolving old destinations to their grouped tab.
    pub fn navigate_to(&self, nav_item: NavItem) {
        let imp = self.imp();
        let sidebar_item = nav_item.sidebar_item();

        // Direct actions (such as a missing-model prompt) must also highlight
        // the owning sidebar row. Selecting it can re-enter with the group;
        // resolve the requested tab afterwards so that direct links win.
        if let Some(list) = imp.sidebar_list.borrow().as_ref() {
            if let Some(index) = NavItem::all().iter().position(|item| *item == sidebar_item) {
                if list
                    .selected_row()
                    .is_none_or(|row| row.index() != index as i32)
                {
                    list.select_row(list.row_at_index(index as i32).as_ref());
                }
            }
        }

        let page_name = match nav_item {
            NavItem::Transcription => "transcription",
            NavItem::History => "history",
            NavItem::Recognition | NavItem::Model | NavItem::Language | NavItem::Performance => {
                "recognition"
            }
            NavItem::DictationAudio | NavItem::Microphone | NavItem::Dictation => "dictation-audio",
            NavItem::Dictionary => "dictionary",
            NavItem::Llm => "llm",
            NavItem::Api => "api",
            NavItem::Help => "help",
        };

        if let Some(stack) = imp.content_stack.borrow().as_ref() {
            stack.set_visible_child_name(page_name);
        }

        let tab = match nav_item {
            NavItem::Model => Some((&imp.recognition_stack, "model")),
            NavItem::Language => Some((&imp.recognition_stack, "language")),
            NavItem::Performance => Some((&imp.recognition_stack, "performance")),
            NavItem::Microphone => Some((&imp.dictation_audio_stack, "microphone")),
            NavItem::Dictation => Some((&imp.dictation_audio_stack, "dictation")),
            _ => None,
        };
        if let Some((stack, name)) = tab {
            if let Some(stack) = stack.borrow().as_ref() {
                stack.set_visible_child_name(name);
            }
        }

        imp.current_nav.set(Some(sidebar_item));

        if let Some(header) = imp.header_controls.borrow().as_ref() {
            header.set_page_title(&sidebar_item.header_title());
        }

        // The LLM enable switch may have changed; keep the AI button in sync.
        self.refresh_ai_button();

        // Returning to Transcription with a non-Whisper backend: re-sync so the
        // model dropdown reflects any size just downloaded in Settings (Qwen).
        if matches!(nav_item, NavItem::Transcription)
            && self
                .current_config_snapshot()
                .map(|c| c.backend != "whisper")
                .unwrap_or(false)
        {
            self.sync_ui_from_config();
        }
    }

    fn setup_actions(&self) {
        let diagnostics = gio::ActionEntry::builder("diagnostics")
            .activate(|window: &Self, _, _| window.show_diagnostics())
            .build();
        let action_quit = gio::ActionEntry::builder("quit")
            .activate(|window: &Self, _, _| {
                window.close();
            })
            .build();

        let action_toggle_sidebar = gio::ActionEntry::builder("toggle-sidebar")
            .activate(|window: &Self, _, _| {
                window.toggle_sidebar();
            })
            .build();

        let action_navigate = gio::ActionEntry::builder("navigate")
            .parameter_type(Some(&String::static_variant_type()))
            .activate(|window: &Self, _, parameter| {
                if let Some(name) = parameter.and_then(|p| p.get::<String>()) {
                    let nav_item = match name.as_str() {
                        "transcription" => NavItem::Transcription,
                        "history" => NavItem::History,
                        "recognition" => NavItem::Recognition,
                        "dictation-audio" => NavItem::DictationAudio,
                        "microphone" => NavItem::Microphone,
                        "model" => NavItem::Model,
                        "language" => NavItem::Language,
                        "performance" => NavItem::Performance,
                        "dictation" => NavItem::Dictation,
                        "dictionary" => NavItem::Dictionary,
                        "llm" => NavItem::Llm,
                        "api" => NavItem::Api,
                        "help" => NavItem::Help,
                        _ => return,
                    };
                    window.navigate_to(nav_item);
                }
            })
            .build();

        let action_record_toggle = gio::ActionEntry::builder("record-toggle")
            .activate(|window: &Self, _, _| {
                let is_main_recording = window
                    .controller()
                    .map(|controller| controller.owner() == RecordingOwner::Main)
                    .unwrap_or(false);
                if is_main_recording {
                    window.on_stop();
                } else {
                    window.on_record();
                }
            })
            .build();

        let action_cancel_recording = gio::ActionEntry::builder("cancel-recording")
            .activate(|window: &Self, _, _| {
                window.on_cancel();
            })
            .build();

        self.add_action_entries([
            diagnostics,
            action_quit,
            action_toggle_sidebar,
            action_navigate,
            action_record_toggle,
            action_cancel_recording,
        ]);
    }

    /// Add a history entry through the live HistoryPage (keeps the in-memory
    /// list and disk in sync). Used by the global dictation path when the main
    /// window is open.
    pub fn add_history_entry(&self, entry: crate::ui::history_page::HistoryEntry) {
        if let Some(hp) = self.imp().history_page.borrow().as_ref() {
            hp.add_entry(entry);
        }
    }

    /// Update a history entry's title in place (used by LLM auto-title).
    pub fn history_update_title(&self, id: &str, title: &str) {
        if let Some(hp) = self.imp().history_page.borrow().as_ref() {
            hp.update_entry_title(id, title);
        }
    }

    pub fn history_update_variants(&self, id: &str, polished: &str) {
        if let Some(history) = self.imp().history_page.borrow().as_ref() {
            history.update_variants(id, Some(polished), None);
        }
    }

    fn show_diagnostics(&self) {
        let config = AppConfig::load();
        let loaded = self.controller().and_then(|controller| {
            controller.engine_arc().lock().ok().and_then(|slot| {
                slot.as_ref().map(|engine| {
                    serde_json::json!({
                        "model": engine.model_id(), "gpu_context": engine.uses_gpu()
                    })
                })
            })
        });
        let data = serde_json::json!({ "version": crate::VERSION, "selected_backend": config.backend,
            "selected_model": config.selected_model, "loaded_whisper": loaded,
            "settings_persistence": format!("{:?}", AppConfig::persistence_status()),
            "last_completed_run": crate::diagnostics::latest(),
            "capabilities": crate::transcription::capabilities::BACKENDS });
        let dialog = gtk::Window::builder()
            .title(gettext("Diagnostics"))
            .default_width(640)
            .default_height(480)
            .transient_for(self)
            .modal(true)
            .build();
        let view = gtk::TextView::new();
        view.set_editable(false);
        view.set_monospace(true);
        view.set_wrap_mode(gtk::WrapMode::WordChar);
        view.buffer().set_text(
            &serde_json::to_string_pretty(&data)
                .unwrap_or_else(|_| "Diagnostics unavailable".into()),
        );
        let scrolled = gtk::ScrolledWindow::new();
        scrolled.set_child(Some(&view));
        dialog.set_child(Some(&scrolled));
        dialog.present();
    }

    /// Show a toast notification. Messages are redacted (no secrets/home paths)
    /// since some callers pass through error text.
    pub fn show_toast(&self, message: &str) {
        // Routed through the top in-app banner instead of a bottom OSD toast.
        self.show_banner(message, false);
    }

    /// Try to load the currently selected model in a background thread.
    fn load_selected_model(&self) {
        let config = match self.current_config_snapshot() {
            Some(config) => config,
            None => return,
        };

        if config.backend != "whisper" {
            tracing::info!(
                "{} backend is active; skipping Whisper model preload",
                config.backend
            );
            return;
        }

        // `selected_model` is normally the exact downloaded ID, but older configs
        // (and pre-fix wizard runs) may store a base ID like "tiny" while only the
        // quantized variant "tiny-q5_1" is on disk. Resolve to an actually-present
        // model and persist the correction so the dropdown and engine stay in sync.
        let selected = config.selected_model.clone();
        if ModelCatalog::is_downloaded(&selected) {
            self.load_model_by_id(&selected);
            return;
        }

        let base = ModelCatalog::base_model_id(&selected).to_string();
        let resolved = ModelCatalog::resolve_model(&base, config.use_quantized);
        if ModelCatalog::is_downloaded(&resolved) {
            tracing::info!(
                "Selected model '{}' missing; resolved to downloaded '{}'",
                selected,
                resolved
            );
            let mut corrected = config.clone();
            corrected.selected_model = resolved.clone();
            corrected.save();
            self.replace_config(corrected);
            self.sync_ui_from_config();
            self.load_model_by_id(&resolved);
        } else {
            tracing::warn!(
                "Selected model '{}' not downloaded and no variant available",
                selected
            );
        }
    }

    /// React to a model being deleted from disk: refresh the dropdown and, if the
    /// active model was the one removed, switch to another downloaded model (or
    /// clear the engine if none remain).
    pub fn handle_model_deleted(&self, deleted_id: &str) {
        let was_active = self
            .current_config_snapshot()
            .map(|c| c.selected_model == deleted_id)
            .unwrap_or(false);

        if was_active {
            let downloaded = Self::downloaded_model_entries();
            if let Some((next_id, _)) = downloaded.first().cloned() {
                if let Some(mut config) = self.current_config_snapshot() {
                    config.selected_model = next_id.clone();
                    config.save();
                    self.replace_config(config);
                }
                self.load_model_by_id(&next_id);
            } else {
                // No models left — drop the loaded engine.
                if let Some(engine_arc) = self.imp().engine.borrow().as_ref() {
                    if let Ok(mut guard) = engine_arc.lock() {
                        *guard = None;
                    }
                }
                if let Some(sb) = self.imp().status_bar.borrow().as_ref() {
                    sb.set_model_name(&gettext("No model selected"));
                    sb.set_recording_status(&gettext("No Model"));
                }
            }
        }

        // Always refresh the dropdown to reflect the new downloaded set.
        self.sync_ui_from_config();
        self.show_toast(&format!("Deleted model: {}", deleted_id));
    }

    /// Load a specific model by ID in a background thread.
    /// After loading, updates header and status bar.
    pub fn load_model_by_id(&self, model_id: &str) {
        let imp = self.imp();

        if !ModelCatalog::is_downloaded(model_id) {
            tracing::warn!("Model '{}' not downloaded", model_id);
            return;
        }

        let engine_arc = match imp.engine.borrow().clone() {
            Some(e) => e,
            None => return,
        };

        // The shared slot may already hold this model (startup preload on a
        // hidden launch, or the API worker loaded it first). Reuse it instead
        // of loading a second multi-GB copy; just bring the UI up to date.
        let already_loaded = engine_arc
            .lock()
            .map(|g| {
                g.as_ref()
                    .is_some_and(|e| e.model_id() == model_id && e.uses_gpu() == self.is_gpu_mode())
            })
            .unwrap_or(false);
        if already_loaded {
            self.on_model_loaded(model_id);
            return;
        }

        let generation = imp.model_load_generation.get().wrapping_add(1);
        imp.model_load_generation.set(generation);

        // Check GPU setting from performance page or config
        let use_gpu = imp
            .performance_page
            .borrow()
            .as_ref()
            .map(|p| p.get_gpu_enabled())
            .unwrap_or_else(|| self.is_gpu_mode());

        // Whether to retry on CPU if a GPU load fails.
        let cpu_fallback = imp
            .performance_page
            .borrow()
            .as_ref()
            .map(|p| p.get_cpu_fallback())
            .unwrap_or_else(|| {
                self.current_config_snapshot()
                    .map(|c| c.cpu_fallback)
                    .unwrap_or(true)
            });

        if let Some(sb) = imp.status_bar.borrow().as_ref() {
            // Show the selected model immediately so the status bar matches the
            // header dropdown (not just after a slow/failed load finishes).
            sb.set_model_name(model_id);
            sb.set_recording_status(&gettext("Loading model…"));
        }

        // Result carries the loaded model ID plus whether we fell back to CPU.
        let (sender, receiver) = async_channel::bounded(1);

        let model_id_owned = model_id.to_string();
        let mut load_config = self.current_config_snapshot().unwrap_or_default();
        load_config.selected_model = model_id_owned.clone();
        load_config.use_gpu = use_gpu;
        load_config.cpu_fallback = cpu_fallback;
        let worker_engine = engine_arc.clone();
        std::thread::spawn(move || {
            let result = crate::recording::ensure_engine_loaded(&worker_engine, &load_config)
                .map(|engine| (model_id_owned, use_gpu && !engine.uses_gpu()));
            let _ = sender.send_blocking(result);
        });

        let window = self.clone();
        glib::spawn_future_local(async move {
            while let Ok(result) = receiver.recv().await {
                if window.imp().model_load_generation.get() != generation {
                    return;
                }
                match result {
                    Ok((mid, downgraded)) => {
                        let still_selected = window
                            .current_config_snapshot()
                            .map(|config| {
                                config.backend == "whisper"
                                    && config.selected_model == mid
                                    && config.use_gpu == use_gpu
                            })
                            .unwrap_or(false);
                        if !still_selected {
                            return;
                        }
                        if downgraded {
                            window.show_toast(&gettext("GPU unavailable — loaded model on CPU"));
                        }
                        window.on_model_loaded(&mid);
                    }
                    Err(msg) => window.show_toast(&msg),
                }
            }
        });
    }

    fn on_model_loaded(&self, model_id: &str) {
        let backend = self
            .current_config_snapshot()
            .map(|config| config.backend)
            .unwrap_or_else(|| "whisper".to_string());

        if backend != "whisper" {
            tracing::info!(
                "Whisper model '{}' finished loading, but {} is the active backend; leaving its UI state unchanged",
                model_id, backend
            );
            return;
        }

        tracing::info!("Model '{}' loaded and ready", model_id);

        if let Some(header) = self.imp().header_controls.borrow().as_ref() {
            header.set_model_status(true, model_id);

            // Refresh the dropdown list (a new model may have just been
            // downloaded). Guard the rebuild so `update_models_for_backend`'s
            // internal `set_selected(0)` doesn't fire the change handler and
            // overwrite the freshly-loaded model with the first list entry.
            let downloaded = Self::downloaded_model_entries();
            self.imp().syncing_dropdown.set(true);
            header.update_models_for_backend(&backend, &downloaded);

            let index = downloaded
                .iter()
                .position(|(id, _)| id == model_id)
                .unwrap_or(0) as u32;
            header.set_selected_model(index);
            self.imp().syncing_dropdown.set(false);
        }

        // The status bar must describe the engine that is actually loaded, not
        // the saved preference: a GPU load can fail and silently fall back.
        if let Some(status_bar) = self.imp().status_bar.borrow().as_ref() {
            status_bar.set_model_name(model_id);
            status_bar.set_recording_status(&gettext("Ready"));
        }
        self.refresh_compute_mode();
    }

    /// Reflect the loaded engine's real compute backend in the status bar.
    ///
    /// The config only records the user's *preference*; model loading or a
    /// transcription failure can leave the shared slot on CPU. Reading the
    /// engine keeps the label honest, and the fallback in `recording.rs` is
    /// immediately visible instead of claiming "GPU" while running on CPU.
    pub(crate) fn refresh_compute_mode(&self) {
        let engine = self.imp().engine.borrow().clone();
        let actual = engine.and_then(|shared| {
            shared
                .lock()
                .ok()
                .and_then(|guard| guard.as_ref().map(|engine| engine.uses_gpu()))
        });
        let mode = match actual {
            Some(true) => "GPU",
            Some(false) => "CPU",
            None => {
                if self.is_gpu_mode() {
                    "GPU"
                } else {
                    "CPU"
                }
            }
        };
        if let Some(status_bar) = self.imp().status_bar.borrow().as_ref() {
            status_bar.set_compute_mode(mode);
        }
    }

    fn is_gpu_mode(&self) -> bool {
        self.imp()
            .config
            .borrow()
            .as_ref()
            .map(|c| c.use_gpu)
            .unwrap_or(false)
    }

    /// Check GitHub for a newer release and show the update banner if found.
    fn check_for_updates(&self) {
        // Respect the user's choice: the startup update check is the only
        // unprompted network request the app makes, so it can be disabled.
        if !AppConfig::load().update_check_enabled {
            return;
        }

        let (sender, receiver) =
            async_channel::bounded::<Option<crate::version_check::UpdateInfo>>(1);

        crate::application::tokio_runtime().spawn(async move {
            let result = crate::version_check::check_for_update(crate::VERSION).await;
            let _ = sender.send(result).await;
        });

        let weak_window = self.downgrade();
        glib::spawn_future_local(async move {
            if let Ok(Some(info)) = receiver.recv().await {
                if let Some(window) = weak_window.upgrade() {
                    window.show_update_available(&info);
                }
            }
        });
    }

    /// Surface the new version in the status bar (bottom right, clickable).
    fn show_update_available(&self, info: &crate::version_check::UpdateInfo) {
        if let Some(sb) = self.imp().status_bar.borrow().as_ref() {
            sb.show_update_available(&info.latest_version);
        }
    }
}
