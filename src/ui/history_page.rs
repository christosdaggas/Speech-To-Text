// Copyright (C) 2026 Christos A. Daggas
// SPDX-License-Identifier: MIT

use crate::i18n::gettext;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk4 as gtk;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// A single history entry.
///
/// Newer optional fields use `#[serde(default)]` so history files written by
/// older builds (which lacked them) still deserialize cleanly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: String,
    pub title: String,
    pub text: String,
    pub language: String,
    pub duration_secs: u64,
    pub timestamp: String,
    pub model: String,
    /// Word count of the raw transcript (for the session-stats display).
    #[serde(default)]
    pub word_count: Option<u32>,
    /// The AI-polished version, when the user produced one ("Improve"/chips).
    #[serde(default)]
    pub polished_text: Option<String>,
    /// LLM summary of long file transcripts, when generated.
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub raw_text: Option<String>,
    #[serde(default)]
    pub incomplete: bool,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct HistoryPage {
        pub list_box: RefCell<Option<gtk::ListBox>>,
        pub search_entry: RefCell<Option<gtk::SearchEntry>>,
        pub empty_status: RefCell<Option<adw::StatusPage>>,
        pub entries: RefCell<Vec<HistoryEntry>>,
        pub rows: RefCell<HashMap<String, adw::ActionRow>>,
        /// Precomputed lowercase "searchable text" per entry id, so the search
        /// box doesn't re-allocate and case-fold the whole transcript corpus
        /// on every query change.
        pub search_cache: RefCell<HashMap<String, String>>,
        /// Bumped when the list is cleared so an in-flight chunked populate
        /// from a stale load stops adding rows.
        pub populate_generation: std::cell::Cell<u64>,
        /// Whether the initial disk load has merged into `entries`. Saves
        /// before that point are deferred (see `save_history`).
        pub loaded: std::cell::Cell<bool>,
        /// A save was requested while the initial load was still in flight.
        pub save_pending: std::cell::Cell<bool>,
        pub loading: std::cell::Cell<bool>,
        pub load_error: RefCell<Option<gtk::Box>>,
        pub page: std::cell::Cell<usize>,
        pub page_label: RefCell<Option<gtk::Label>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for HistoryPage {
        const NAME: &'static str = "SttHistoryPage";
        type Type = super::HistoryPage;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for HistoryPage {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().setup_ui();
        }
    }

    impl WidgetImpl for HistoryPage {}
    impl BoxImpl for HistoryPage {}
}

glib::wrapper! {
    pub struct HistoryPage(ObjectSubclass<imp::HistoryPage>)
        @extends gtk::Widget, gtk::Box;
}

impl HistoryPage {
    pub fn new() -> Self {
        let page: Self = glib::Object::builder()
            .property("orientation", gtk::Orientation::Vertical)
            .property("spacing", 0)
            .build();
        page.load_history_async();
        page
    }

    fn setup_ui(&self) {
        let imp = self.imp();
        self.add_css_class("history-page");

        let header_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header_box.set_margin_start(16);
        header_box.set_margin_end(16);
        header_box.set_margin_top(16);
        header_box.set_margin_bottom(8);

        let title = gtk::Label::new(Some(gettext("Transcription History").as_str()));
        title.add_css_class("title-3");
        title.set_hexpand(true);
        title.set_xalign(0.0);
        header_box.append(&title);

        let search_entry = gtk::SearchEntry::new();
        search_entry.set_placeholder_text(Some(gettext("Search transcriptions…").as_str()));
        search_entry.set_hexpand(false);
        search_entry.set_width_chars(25);
        header_box.append(&search_entry);

        let clear_all_btn = gtk::Button::from_icon_name("edit-clear-all-symbolic");
        clear_all_btn.set_tooltip_text(Some(gettext("Clear all history").as_str()));
        clear_all_btn.add_css_class("flat");
        let page_weak = self.downgrade();
        clear_all_btn.connect_clicked(move |_| {
            if let Some(page) = page_weak.upgrade() {
                page.confirm_clear_all();
            }
        });
        header_box.append(&clear_all_btn);
        let export = gtk::Button::with_label(&gettext("Export JSON"));
        let weak = self.downgrade();
        export.connect_clicked(move |_| {
            if let Some(page) = weak.upgrade() {
                page.export_history();
            }
        });
        header_box.append(&export);
        let retention = gtk::Button::with_label(&gettext("Retention"));
        let weak = self.downgrade();
        retention.connect_clicked(move |_| {
            if let Some(page) = weak.upgrade() {
                page.choose_retention();
            }
        });
        header_box.append(&retention);

        self.append(&header_box);

        let error_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let error_label = gtk::Label::new(Some(&gettext(
            "History could not be loaded. Existing entries are protected; retry to save new entries.",
        )));
        error_label.set_wrap(true);
        error_label.set_hexpand(true);
        let retry = gtk::Button::with_label(&gettext("Retry"));
        let weak = self.downgrade();
        retry.connect_clicked(move |_| {
            if let Some(page) = weak.upgrade() {
                page.load_history_async();
            }
        });
        error_box.append(&error_label);
        error_box.append(&retry);
        error_box.set_visible(false);
        self.append(&error_box);
        *imp.load_error.borrow_mut() = Some(error_box);

        let scrolled = gtk::ScrolledWindow::new();
        scrolled.set_vexpand(true);

        let list_box = gtk::ListBox::new();
        list_box.set_selection_mode(gtk::SelectionMode::None);
        list_box.add_css_class("boxed-list");
        list_box.set_margin_start(16);
        list_box.set_margin_end(16);
        list_box.set_margin_bottom(16);

        let placeholder = adw::StatusPage::new();
        placeholder.set_icon_name(Some("document-open-recent-symbolic"));
        placeholder.set_title(gettext("No Transcriptions Yet").as_str());
        placeholder.set_description(Some(
            gettext("Your transcription history will appear here").as_str(),
        ));
        list_box.set_placeholder(Some(placeholder.upcast_ref::<gtk::Widget>()));

        scrolled.set_child(Some(&list_box));
        self.append(&scrolled);
        let paging = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        paging.set_halign(gtk::Align::Center);
        let previous = gtk::Button::with_label(&gettext("Previous"));
        let next = gtk::Button::with_label(&gettext("Next"));
        let label = gtk::Label::new(None);
        paging.append(&previous);
        paging.append(&label);
        paging.append(&next);
        self.append(&paging);
        *imp.page_label.borrow_mut() = Some(label);
        let weak = self.downgrade();
        previous.connect_clicked(move |_| {
            if let Some(page) = weak.upgrade() {
                page.imp().page.set(page.imp().page.get().saturating_sub(1));
                page.refresh_rows();
            }
        });
        let weak = self.downgrade();
        next.connect_clicked(move |_| {
            if let Some(page) = weak.upgrade() {
                page.imp().page.set(page.imp().page.get().saturating_add(1));
                page.refresh_rows();
            }
        });

        // Search filtering over the precomputed lowercase cache — re-building
        // and case-folding the whole transcript corpus per query change made
        // typing in the search box stutter once the history grew.
        let page_weak = self.downgrade();
        let placeholder_ref = placeholder.clone();
        search_entry.connect_search_changed(move |entry| {
            let query = entry.text().to_string().to_lowercase();
            let Some(page) = page_weak.upgrade() else {
                return;
            };
            page.imp().page.set(0);
            page.refresh_rows();
            let placeholder_title = if query.is_empty() {
                gettext("No Transcriptions Yet")
            } else {
                gettext("No matching transcriptions")
            };
            placeholder_ref.set_title(&placeholder_title);
        });

        let page_weak = self.downgrade();
        list_box.connect_row_activated(move |_, activated| {
            let Some(page) = page_weak.upgrade() else {
                return;
            };
            let id = page
                .imp()
                .rows
                .borrow()
                .iter()
                .find_map(|(id, row)| (row == activated).then(|| id.clone()));
            if let Some(id) = id {
                page.show_entry(&id);
            }
        });

        *imp.list_box.borrow_mut() = Some(list_box);
        *imp.search_entry.borrow_mut() = Some(search_entry);
    }

    pub fn add_entry(&self, entry: HistoryEntry) {
        let imp = self.imp();
        imp.search_cache
            .borrow_mut()
            .insert(entry.id.clone(), searchable_blob(&entry));
        upsert_entry(&mut imp.entries.borrow_mut(), entry);
        imp.page.set(0);
        self.refresh_rows();
        self.save_history();
    }

    /// Ask for confirmation before clearing all history (it cannot be undone).
    pub fn confirm_clear_all(&self) {
        let dialog = adw::AlertDialog::new(
            Some(gettext("Clear all history?").as_str()),
            Some(
                gettext(
                    "This permanently deletes every saved transcription. This cannot be undone.",
                )
                .as_str(),
            ),
        );
        dialog.add_response("cancel", gettext("Cancel").as_str());
        dialog.add_response("clear", gettext("Clear All").as_str());
        dialog.set_response_appearance("clear", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        let page = self.clone();
        dialog.choose(self, gtk::gio::Cancellable::NONE, move |resp| {
            if resp.as_str() == "clear" {
                page.clear_all();
            }
        });
    }

    pub fn clear_all(&self) {
        let imp = self.imp();
        if !imp.loaded.get() {
            return; // Never replace unreadable or still-loading history.
        }
        // Stop any in-flight chunked populate from re-adding rows.
        imp.populate_generation
            .set(imp.populate_generation.get().wrapping_add(1));
        if let Some(list_box) = imp.list_box.borrow().as_ref() {
            while let Some(row) = list_box.row_at_index(0) {
                list_box.remove(&row);
            }
        }
        imp.entries.borrow_mut().clear();
        imp.rows.borrow_mut().clear();
        imp.search_cache.borrow_mut().clear();
        self.save_history();
    }

    fn add_entry_row(&self, list_box: &gtk::ListBox, entry: &HistoryEntry) {
        let mut subtitle = format!(
            "{} • {} • {}",
            entry.timestamp,
            entry.language,
            format_duration(entry.duration_secs)
        );
        if let Some(words) = entry.word_count {
            subtitle.push_str(&format!(" • {} {}", words, gettext("words")));
            if let Some(wpm) =
                crate::ui::result_state::wpm(words as usize, entry.duration_secs as f32)
            {
                subtitle.push_str(&format!(" · {} wpm", wpm));
            }
        }
        let row = adw::ActionRow::builder()
            .title(&entry.title)
            .subtitle(&subtitle)
            .activatable(true)
            .build();
        row.set_use_markup(false);
        if entry.incomplete {
            row.set_subtitle(&format!("{} • {}", gettext("Partial transcript"), subtitle));
        }

        let model_badge = gtk::Label::new(Some(&entry.model));
        model_badge.add_css_class("caption");
        model_badge.add_css_class("dim-label");
        row.add_suffix(&model_badge);

        let copy_btn = gtk::Button::from_icon_name("edit-copy-symbolic");
        copy_btn.set_tooltip_text(Some(gettext("Copy to clipboard").as_str()));
        copy_btn.set_valign(gtk::Align::Center);
        copy_btn.add_css_class("flat");
        let text = entry.text.clone();
        copy_btn.connect_clicked(move |btn| {
            if let Some(display) = btn.display().into() {
                let clipboard = gtk::gdk::Display::clipboard(&display);
                clipboard.set_text(&text);
            }
        });
        row.add_suffix(&copy_btn);

        let delete_btn = gtk::Button::from_icon_name("user-trash-symbolic");
        delete_btn.set_tooltip_text(Some(gettext("Delete").as_str()));
        delete_btn.set_valign(gtk::Align::Center);
        delete_btn.add_css_class("flat");

        let list_box_ref = list_box.clone();
        let entry_id = entry.id.clone();
        let page_weak = self.downgrade();
        delete_btn.connect_clicked(move |btn| {
            if let Some(row) = btn.ancestor(gtk::ListBoxRow::static_type()) {
                list_box_ref.remove(&row);
            }
            if let Some(page) = page_weak.upgrade() {
                page.imp().entries.borrow_mut().retain(|e| e.id != entry_id);
                page.imp().rows.borrow_mut().remove(&entry_id);
                page.imp().search_cache.borrow_mut().remove(&entry_id);
                page.save_history();
                page.refresh_rows();
            }
        });

        row.add_suffix(&delete_btn);

        list_box.prepend(&row);
        self.imp().rows.borrow_mut().insert(entry.id.clone(), row);
        self.imp()
            .search_cache
            .borrow_mut()
            .insert(entry.id.clone(), searchable_blob(entry));
    }

    fn show_entry(&self, id: &str) {
        let Some(entry) = self
            .imp()
            .entries
            .borrow()
            .iter()
            .find(|entry| entry.id == id)
            .cloned()
        else {
            return;
        };

        let detail = gtk::Window::builder()
            .title(&entry.title)
            .default_width(640)
            .default_height(480)
            .modal(false)
            .build();
        if let Some(parent) = self.root().and_downcast::<gtk::Window>() {
            detail.set_application(parent.application().as_ref());
        }
        let scrolled = gtk::ScrolledWindow::new();
        let text = gtk::TextView::new();
        text.set_editable(false);
        text.set_cursor_visible(false);
        text.set_wrap_mode(gtk::WrapMode::WordChar);
        text.set_left_margin(12);
        text.set_right_margin(12);
        text.set_top_margin(12);
        text.set_bottom_margin(12);
        text.buffer().set_text(&entry.text);
        scrolled.set_child(Some(&text));
        scrolled.set_vexpand(true);
        let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
        body.set_margin_top(16);
        body.set_margin_bottom(16);
        body.set_margin_start(16);
        body.set_margin_end(16);
        let variants = entry_variants(&entry);
        let labels: Vec<&str> = variants.iter().map(|(label, _)| label.as_str()).collect();
        let selector = gtk::DropDown::from_strings(&labels);
        selector.set_tooltip_text(Some(&gettext("Transcript version")));
        text.buffer().set_text(&variants[0].1);
        let buffer = text.buffer();
        selector.connect_selected_notify(move |selector| {
            if let Some((_, value)) = variants.get(selector.selected() as usize) {
                buffer.set_text(value);
            }
        });
        body.append(&selector);
        body.append(&scrolled);
        detail.set_child(Some(&body));
        detail.present();
    }

    /// Update the title of an existing entry (in memory, on disk, and in the UI
    /// row if visible). Used by the LLM auto-title feature.
    pub fn update_entry_title(&self, id: &str, title: &str) {
        let imp = self.imp();
        let mut changed = false;
        if let Some(e) = imp.entries.borrow_mut().iter_mut().find(|e| e.id == id) {
            e.title = title.to_string();
            changed = true;
            imp.search_cache
                .borrow_mut()
                .insert(id.to_string(), searchable_blob(e));
        }
        if let Some(row) = imp.rows.borrow().get(id) {
            row.set_title(title);
        }
        if changed {
            self.save_history();
        }
    }

    /// Persist history entries to disk as JSON. The in-memory list is
    /// snapshotted on the GTK thread; serialization and the fsync'd atomic
    /// write happen on a worker so a growing history can never stall the UI
    /// right between transcription completion and result delivery.
    fn save_history(&self) {
        let imp = self.imp();
        // Until the initial disk load has merged, the in-memory list holds
        // only post-construction entries — writing it now would TRUNCATE the
        // user's entire prior history on disk. Defer; finish_load releases it.
        if !imp.loaded.get() {
            imp.save_pending.set(true);
            return;
        }
        let days = crate::config::AppConfig::load().history_retention_days;
        let changed = prune_entries(
            &mut imp.entries.borrow_mut(),
            days,
            chrono::Local::now().naive_local(),
        );
        if changed {
            self.rebuild_search_cache();
            self.refresh_rows();
        }
        save_snapshot_async(imp.entries.borrow().clone());
    }

    /// Load history from disk on a worker thread, then populate the list in
    /// small idle-callback chunks — parsing and building thousands of rows
    /// synchronously in `new()` delayed the first window paint by the size of
    /// the user's lifetime history.
    fn load_history_async(&self) {
        if self.imp().loading.replace(true) {
            return;
        }
        let (tx, rx) = async_channel::bounded::<Result<Vec<HistoryEntry>, String>>(1);
        // Capture the generation BEFORE spawning the reader: a "Clear All"
        // confirmed while the file is still being read must cancel the merge,
        // or every "permanently deleted" entry would be resurrected.
        let generation = self.imp().populate_generation.get();
        std::thread::spawn(move || {
            let _ = tx.send_blocking(read_history_from_disk());
        });
        let page_weak = self.downgrade();
        glib::spawn_future_local(async move {
            let entries = rx
                .recv()
                .await
                .unwrap_or_else(|_| Err("History reader unavailable".into()));
            let Some(page) = page_weak.upgrade() else {
                return;
            };
            page.finish_load(entries, generation);
        });
    }

    /// Merge the loaded entries into memory (unless a clear cancelled the
    /// load), start the chunked row populate, and release any save deferred
    /// while the load was in flight.
    fn finish_load(&self, entries: Result<Vec<HistoryEntry>, String>, generation: u64) {
        let imp = self.imp();
        imp.loading.set(false);
        let entries = match entries {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(
                    "History load failed: {}",
                    crate::error::redact_secrets(&error)
                );
                if let Some(banner) = imp.load_error.borrow().as_ref() {
                    banner.set_visible(true);
                }
                return;
            }
        };
        if let Some(banner) = imp.load_error.borrow().as_ref() {
            banner.set_visible(false);
        }
        let cancelled = imp.populate_generation.get() != generation;
        if !cancelled && !entries.is_empty() {
            // Memory first, so search/save/add_entry see the full list at
            // once: loaded history, then anything added since construction.
            {
                let mut current = imp.entries.borrow_mut();
                merge_history(&mut current, entries);
            }
            self.rebuild_search_cache();
            self.refresh_rows();
        }
        imp.loaded.set(true);
        if imp.save_pending.take() || crate::config::AppConfig::load().history_retention_days > 0 {
            self.save_history();
        }
    }

    /// Insert rows for the current entries in batches per idle tick, keeping
    /// the main loop responsive. New dictations arriving mid-populate go
    /// through [`Self::add_entry`] (appended after the snapshot), so the
    /// snapshot walk can't duplicate them; `populate_generation` aborts the
    /// walk when the list is cleared underneath it.
    fn rebuild_search_cache(&self) {
        *self.imp().search_cache.borrow_mut() = self
            .imp()
            .entries
            .borrow()
            .iter()
            .map(|entry| (entry.id.clone(), searchable_blob(entry)))
            .collect();
    }

    fn refresh_rows(&self) {
        const PAGE_SIZE: usize = 100;
        let imp = self.imp();
        let Some(list) = imp.list_box.borrow().clone() else {
            return;
        };
        while let Some(row) = list.row_at_index(0) {
            list.remove(&row);
        }
        imp.rows.borrow_mut().clear();
        let query = imp
            .search_entry
            .borrow()
            .as_ref()
            .map(|entry| entry.text().to_lowercase())
            .unwrap_or_default();
        let entries = imp.entries.borrow();
        let cache = imp.search_cache.borrow();
        let matching: Vec<&HistoryEntry> = entries
            .iter()
            .rev()
            .filter(|entry| {
                query.is_empty()
                    || cache
                        .get(&entry.id)
                        .is_some_and(|blob| blob.contains(&query))
            })
            .collect();
        let pages = matching.len().div_ceil(PAGE_SIZE).max(1);
        let page = imp.page.get().min(pages - 1);
        imp.page.set(page);
        let visible: Vec<HistoryEntry> = matching
            .iter()
            .skip(page * PAGE_SIZE)
            .take(PAGE_SIZE)
            .map(|entry| (*entry).clone())
            .collect();
        drop(cache);
        drop(entries);
        for entry in visible.iter().rev() {
            self.add_entry_row(&list, entry);
        }
        if let Some(label) = imp.page_label.borrow().as_ref() {
            label.set_text(&format!("{} / {}", page + 1, pages));
        }
    }

    pub fn update_variants(&self, id: &str, polished: Option<&str>, summary: Option<&str>) {
        {
            let mut entries = self.imp().entries.borrow_mut();
            let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) else {
                return;
            };
            if let Some(text) = polished {
                entry.polished_text = Some(text.into());
            }
            if let Some(text) = summary {
                entry.summary = Some(text.into());
            }
        }
        self.rebuild_search_cache();
        self.save_history();
    }

    fn export_history(&self) {
        let Some(parent) = self.root().and_downcast::<gtk::Window>() else {
            return;
        };
        let entries = self.imp().entries.borrow().clone();
        let dialog = gtk::FileDialog::builder()
            .title(gettext("Export history"))
            .initial_name("transcription-history.json")
            .build();
        dialog.save(Some(&parent.clone()), gtk::gio::Cancellable::NONE, move |result| {
            let Ok(file) = result else { return; };
            let Some(path) = file.path() else { return; };
            let (tx, rx) = async_channel::bounded(1);
            std::thread::spawn(move || {
                let result = serde_json::to_vec_pretty(&entries).map_err(|e| e.to_string())
                    .and_then(|bytes| crate::fsio::write_private(&path, &bytes).map_err(|e| e.to_string()));
                let _ = tx.send_blocking(result);
            });
            glib::spawn_future_local(async move {
                if !matches!(rx.recv().await, Ok(Ok(()))) {
                    let error = adw::AlertDialog::new(Some(&gettext("Export failed")), Some(&gettext("Could not write the selected file. Choose another location and retry.")));
                    error.add_response("close", &gettext("Close"));
                    error.present(Some(&parent));
                }
            });
        });
    }

    fn choose_retention(&self) {
        let dialog = adw::AlertDialog::new(Some(&gettext("History retention")), Some(&gettext("Keep entries for this many days. Zero keeps everything. Applying a limit permanently deletes older entries.")));
        let days = gtk::SpinButton::with_range(0.0, 3650.0, 1.0);
        days.set_value(crate::config::AppConfig::load().history_retention_days as f64);
        dialog.set_extra_child(Some(&days));
        dialog.add_response("cancel", &gettext("Cancel"));
        dialog.add_response("apply", &gettext("Apply retention"));
        dialog.set_response_appearance("apply", adw::ResponseAppearance::Destructive);
        let weak = self.downgrade();
        dialog.choose(self, gtk::gio::Cancellable::NONE, move |response| {
            if response != "apply" {
                return;
            }
            let mut config = crate::config::AppConfig::load();
            config.history_retention_days = days.value_as_int() as u32;
            config.save();
            if let Some(page) = weak.upgrade() {
                page.save_history();
            }
        });
    }
}

fn upsert_entry(entries: &mut Vec<HistoryEntry>, entry: HistoryEntry) {
    if let Some(existing) = entries.iter_mut().find(|existing| existing.id == entry.id) {
        *existing = entry;
    } else {
        entries.push(entry);
    }
}

fn prune_entries(entries: &mut Vec<HistoryEntry>, days: u32, now: chrono::NaiveDateTime) -> bool {
    if days == 0 {
        return false;
    }
    let cutoff = now - chrono::Duration::days(i64::from(days.min(3650)));
    let before = entries.len();
    entries.retain(|entry| {
        chrono::NaiveDateTime::parse_from_str(&entry.timestamp, "%Y-%m-%d %H:%M")
            .map(|time| time >= cutoff)
            .unwrap_or(true)
    });
    entries.len() != before
}

fn entry_variants(entry: &HistoryEntry) -> Vec<(String, String)> {
    let mut variants = vec![(
        gettext("Raw"),
        entry.raw_text.as_ref().unwrap_or(&entry.text).clone(),
    )];
    if entry.text != variants[0].1 {
        variants.push((gettext("Cleaned"), entry.text.clone()));
    }
    if let Some(text) = &entry.polished_text {
        variants.push((gettext("Polished"), text.clone()));
    }
    if let Some(text) = &entry.summary {
        variants.push((gettext("Summary"), text.clone()));
    }
    variants
}

/// Lowercase haystack used by the search box.
fn searchable_blob(entry: &HistoryEntry) -> String {
    format!(
        "{}\n{}\n{}\n{}\n{}",
        entry.title,
        entry.text,
        entry.polished_text.as_deref().unwrap_or_default(),
        entry.summary.as_deref().unwrap_or_default(),
        entry.raw_text.as_deref().unwrap_or_default(),
    )
    .to_lowercase()
}

/// Read and parse the history file (preserving a corrupt file aside instead of
/// silently losing it). Runs on worker threads.
fn read_history_from_disk() -> Result<Vec<HistoryEntry>, String> {
    // Order the initial read after any in-flight background append (a headless
    // dictation finishing right as the first window opens): reading the file
    // mid read-modify-write would drop the appended entry from memory, and the
    // next page save would then erase it from disk too.
    let _guard = HISTORY_FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = history_file_path();
    load_entries_for_update(&path)
}

fn merge_history(current: &mut Vec<HistoryEntry>, mut loaded: Vec<HistoryEntry>) {
    for entry in std::mem::take(current) {
        if let Some(existing) = loaded.iter_mut().find(|old| old.id == entry.id) {
            *existing = entry;
        } else {
            loaded.push(entry);
        }
    }
    *current = loaded;
}

fn history_file_path() -> std::path::PathBuf {
    crate::config::AppConfig::history_dir().join("history.json")
}

/// Load the on-disk entries ahead of a read-modify-write.
///
/// * Missing file → empty list (first entry ever).
/// * Corrupt JSON → the file is moved aside as `history.json.corrupt-<ts>` and
///   an empty list is returned, so the user's data survives for manual
///   recovery while the app keeps working.
/// * Any other read failure (permissions, I/O error, a directory in the way)
///   → `Err`. Callers must NOT write in that case: the file may be perfectly
///   good, and replacing it with whatever they were about to append would
///   erase the entire history.
fn load_entries_for_update(path: &std::path::Path) -> Result<Vec<HistoryEntry>, String> {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            tracing::warn!("Failed to read history: {}", e);
            return Err(format!("Failed to read history: {e}"));
        }
    };
    match serde_json::from_str(&contents) {
        Ok(e) => Ok(e),
        Err(e) => {
            tracing::warn!("Failed to parse history: {}", e);
            preserve_corrupt_history(path)?;
            Ok(Vec::new())
        }
    }
}

fn preserve_corrupt_history(path: &std::path::Path) -> Result<(), String> {
    let backup = path.with_extension(format!("json.corrupt-{}", uuid::Uuid::new_v4()));
    std::fs::rename(path, backup)
        .map_err(|e| format!("Cannot preserve unreadable history; saving is disabled: {e}"))
}

/// Append `entry` to the history file at `path` (read-modify-write). Does not
/// touch the file when the existing history cannot be read: losing one new
/// entry is recoverable (it is still shown in the panel), losing the whole
/// file is not.
fn append_entry_to_path(path: &std::path::Path, entry: HistoryEntry) {
    let mut entries = match load_entries_for_update(path) {
        Ok(entries) => entries,
        Err(_) => {
            tracing::warn!("Not appending to history: the existing file could not be read");
            return;
        }
    };
    upsert_entry(&mut entries, entry);
    prune_entries(
        &mut entries,
        crate::config::AppConfig::load().history_retention_days,
        chrono::Local::now().naive_local(),
    );
    match serde_json::to_string_pretty(&entries) {
        Ok(json) => {
            if let Err(e) = crate::fsio::write_private(path, json.as_bytes()) {
                tracing::warn!("Failed to write history: {}", e);
            }
        }
        Err(e) => tracing::warn!("Failed to serialize history: {}", e),
    }
}

/// Retitle the entry `id` in the history file at `path`. No-op when the entry
/// is missing or the file cannot be read.
fn update_title_at_path(path: &std::path::Path, id: &str, title: &str) {
    let Ok(mut entries) = load_entries_for_update(path) else {
        return;
    };
    let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) else {
        return;
    };
    entry.title = title.to_string();
    match serde_json::to_string_pretty(&entries) {
        Ok(json) => {
            if let Err(e) = crate::fsio::write_private(path, json.as_bytes()) {
                tracing::warn!("Failed to write history: {}", e);
            }
        }
        Err(e) => tracing::warn!("Failed to serialize history: {}", e),
    }
}

/// Monotonic sequencing for page-snapshot saves: two background writes can
/// finish out of order, and an older snapshot must never overwrite a newer one.
static SAVE_SEQ: AtomicU64 = AtomicU64::new(0);
static SAVED_SEQ: AtomicU64 = AtomicU64::new(0);

fn save_snapshot_async(entries: Vec<HistoryEntry>) {
    let seq = SAVE_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    history_write_started();
    std::thread::spawn(move || {
        {
            let _guard = HISTORY_FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            if SAVED_SEQ.load(Ordering::SeqCst) < seq {
                let path = history_file_path();
                match serde_json::to_string_pretty(&entries) {
                    // Transcripts are personal data: write privately (0600 in a
                    // 0700 dir) and atomically so other local users can't read
                    // them and a crash can't corrupt the file.
                    Ok(json) => {
                        if let Err(e) = crate::fsio::write_private(&path, json.as_bytes()) {
                            tracing::warn!("Failed to write history: {}", e);
                        } else {
                            SAVED_SEQ.store(seq, Ordering::SeqCst);
                        }
                    }
                    Err(e) => tracing::warn!("Failed to serialize history: {}", e),
                }
            }
        }
        history_write_finished();
    });
}

/// Count of in-flight background history writes, so shutdown can wait for
/// them: detached threads die with the process, and a dictation saved right
/// before quitting must still reach the disk.
static PENDING_HISTORY_WRITES: AtomicU64 = AtomicU64::new(0);

fn history_write_started() {
    PENDING_HISTORY_WRITES.fetch_add(1, Ordering::SeqCst);
}

fn history_write_finished() {
    PENDING_HISTORY_WRITES.fetch_sub(1, Ordering::SeqCst);
}

/// Block (bounded by `timeout`) until all background history writes finished.
/// Called from application shutdown on the main thread — the UI is going away,
/// so a short blocking wait is acceptable.
pub fn wait_for_pending_writes(timeout: std::time::Duration) {
    let deadline = std::time::Instant::now() + timeout;
    while PENDING_HISTORY_WRITES.load(Ordering::SeqCst) > 0 {
        if std::time::Instant::now() >= deadline {
            tracing::warn!("Timed out waiting for background history writes");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Serializes all history-file read-modify-write sequences so a background
/// append (global dictation while the window is closed) and an auto-title update
/// can't clobber each other's changes. Atomic writes prevent *corruption*; this
/// prevents a *lost update*.
static HISTORY_FILE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Append a single entry to the on-disk history file.
///
/// Use this ONLY when no [`HistoryPage`] is loaded in memory (e.g. a global
/// dictation completed while the main window is closed). When the main window
/// is open, route through [`HistoryPage::add_entry`] instead, which keeps the
/// in-memory list and disk in sync — otherwise a later `save_history()` would
/// overwrite a directly-appended entry.
pub fn append_entry_to_disk(entry: &HistoryEntry) {
    let entry = entry.clone();
    history_write_started();
    std::thread::spawn(move || {
        {
            let _guard = HISTORY_FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            append_entry_to_path(&history_file_path(), entry);
        }
        history_write_finished();
    });
}

/// Update the title of an entry directly on disk (used by LLM auto-title when
/// the main window is closed). No-op if the entry id isn't found.
pub fn update_title_on_disk(id: &str, title: &str) {
    let id = id.to_string();
    let title = title.to_string();
    history_write_started();
    std::thread::spawn(move || {
        {
            let _guard = HISTORY_FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            update_title_at_path(&history_file_path(), &id, &title);
        }
        history_write_finished();
    });
}

pub fn update_polished_on_disk(id: &str, text: &str) {
    let id = id.to_string();
    let text = text.to_string();
    history_write_started();
    std::thread::spawn(move || {
        let _guard = HISTORY_FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = history_file_path();
        if let Ok(mut entries) = load_entries_for_update(&path) {
            if let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) {
                entry.polished_text = Some(text);
                match serde_json::to_vec_pretty(&entries) {
                    Ok(bytes) => {
                        if let Err(error) = crate::fsio::write_private(&path, &bytes) {
                            tracing::warn!("History variant save failed: {error}");
                        }
                    }
                    Err(error) => tracing::warn!("History variant serialization failed: {error}"),
                }
            }
        }
        history_write_finished();
    });
}

fn format_duration(secs: u64) -> String {
    let mins = secs / 60;
    let s = secs % 60;
    if mins > 0 {
        format!("{}m {}s", mins, s)
    } else {
        format!("{}s", s)
    }
}

/// The model name recorded with a history entry for `backend`. The sidecar
/// backends have no Whisper model, so labelling them with `selected_model`
/// (as the panel path used to) showed a Whisper model name on Qwen and Cohere
/// dictations.
pub fn history_model_label(backend: &str, selected_model: &str) -> String {
    match backend {
        "cohere" => "cohere-transcribe".to_string(),
        "parakeet" => "parakeet-tdt-0.6b-v3".to_string(),
        "qwen" => "qwen3-asr".to_string(),
        _ => selected_model.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, text: &str) -> HistoryEntry {
        HistoryEntry {
            id: id.to_string(),
            title: text.to_string(),
            text: text.to_string(),
            ..Default::default()
        }
    }

    fn read_entries(path: &std::path::Path) -> Vec<HistoryEntry> {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn append_creates_the_file_and_keeps_existing_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        append_entry_to_path(&path, entry("a", "first"));
        append_entry_to_path(&path, entry("b", "second"));
        let ids: Vec<String> = read_entries(&path).into_iter().map(|e| e.id).collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[test]
    fn retry_merges_pending_entries_without_duplicates_or_loss() {
        let mut pending = vec![entry("b", "newer"), entry("c", "new")];
        merge_history(
            &mut pending,
            vec![entry("a", "old"), entry("b", "old title")],
        );
        assert_eq!(
            pending.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert_eq!(pending[1].text, "newer");
    }

    #[test]
    fn read_and_backup_failures_are_not_empty_successes() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_entries_for_update(dir.path()).is_err());
        assert!(preserve_corrupt_history(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn retry_replaces_partial_and_variants_round_trip() {
        let mut entries = vec![HistoryEntry {
            incomplete: true,
            ..entry("take", "prefix")
        }];
        upsert_entry(
            &mut entries,
            HistoryEntry {
                raw_text: Some("raw".into()),
                polished_text: Some("polished".into()),
                summary: Some("summary".into()),
                ..entry("take", "complete")
            },
        );
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].incomplete);
        let restored: Vec<HistoryEntry> =
            serde_json::from_slice(&serde_json::to_vec(&entries).unwrap()).unwrap();
        assert_eq!(entry_variants(&restored[0]).len(), 4);
        assert!(searchable_blob(&restored[0]).contains("raw"));
    }

    #[test]
    fn retention_is_opt_in_and_preserves_unparseable_dates() {
        let now =
            chrono::NaiveDateTime::parse_from_str("2026-10-23 12:00", "%Y-%m-%d %H:%M").unwrap();
        let mut entries = vec![
            HistoryEntry {
                timestamp: "2020-01-01 12:00".into(),
                ..entry("old", "old")
            },
            HistoryEntry {
                timestamp: "2026-10-22 12:00".into(),
                ..entry("recent", "recent")
            },
            entry("unknown", "legacy"),
        ];
        assert!(!prune_entries(&mut entries, 0, now));
        assert!(prune_entries(&mut entries, 30, now));
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["recent", "unknown"]
        );
    }

    #[test]
    fn append_preserves_a_corrupt_file_instead_of_overwriting_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        std::fs::write(&path, b"{ this is not json").unwrap();

        append_entry_to_path(&path, entry("new", "after corruption"));

        let ids: Vec<String> = read_entries(&path).into_iter().map(|e| e.id).collect();
        assert_eq!(ids, ["new"], "the new entry is written to a fresh file");
        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("history.json.corrupt-")
            })
            .collect();
        assert_eq!(
            backups.len(),
            1,
            "the corrupt original is kept for recovery"
        );
        assert_eq!(
            std::fs::read(backups[0].path()).unwrap(),
            b"{ this is not json"
        );
    }

    #[test]
    fn append_does_not_replace_a_history_it_could_not_read() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the file should be: read fails with something
        // other than NotFound, so the append must leave it alone.
        let path = dir.path().join("history.json");
        std::fs::create_dir(&path).unwrap();

        append_entry_to_path(&path, entry("x", "lost cause"));

        assert!(
            path.is_dir(),
            "nothing was written over the unreadable path"
        );
    }

    #[test]
    fn update_title_rewrites_only_the_matching_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        append_entry_to_path(&path, entry("a", "first"));
        append_entry_to_path(&path, entry("b", "second"));

        update_title_at_path(&path, "b", "Renamed");
        update_title_at_path(&path, "missing", "Ignored");

        let entries = read_entries(&path);
        assert_eq!(entries[0].title, "first");
        assert_eq!(entries[1].title, "Renamed");
    }

    #[test]
    fn model_label_names_sidecar_backends_instead_of_the_whisper_model() {
        assert_eq!(
            history_model_label("whisper", "large-v3-turbo"),
            "large-v3-turbo"
        );
        assert_eq!(history_model_label("qwen", "large-v3-turbo"), "qwen3-asr");
        assert_eq!(
            history_model_label("cohere", "large-v3-turbo"),
            "cohere-transcribe"
        );
    }
}
