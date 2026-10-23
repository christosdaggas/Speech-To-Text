// Copyright (C) 2026 Christos A. Daggas
// SPDX-License-Identifier: MIT

use gtk4 as gtk;
use gtk4::prelude::*;

/// A draggable header without HeaderBar's symmetric title reservation. Its
/// compact minimum and natural widths must agree, otherwise GtkBox changes the
/// sidebar width as the right-hand content competes for space during resizing.
pub fn header(title: &gtk::Box, toggle: &gtk::Button) -> gtk::WindowHandle {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    content.add_css_class("sidebar-header");
    title.set_hexpand(true);
    toggle.add_css_class("sidebar-toggle");
    toggle.set_valign(gtk::Align::Center);
    content.append(title);
    content.append(toggle);
    let handle = gtk::WindowHandle::new();
    handle.set_child(Some(&content));
    handle
}

/// A full-height text cell and a fixed icon cell, with native font metrics.
/// Keep the label's vertical allocation larger than its ink rather than
/// squeezing a fractional-size font into a centered, ellipsized line.
pub fn navigation_row(title: &str, icon_name: &str) -> (gtk::ListBoxRow, gtk::Label, gtk::Box) {
    let row = gtk::ListBoxRow::new();
    row.add_css_class("sidebar-nav-row");
    row.set_selectable(true);
    row.set_tooltip_text(Some(title));

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    content.add_css_class("sidebar-nav-content");
    content.set_valign(gtk::Align::Fill);

    let icon = gtk::Image::from_icon_name(icon_name);
    icon.set_pixel_size(20);
    icon.set_size_request(24, 24);
    icon.set_valign(gtk::Align::Center);
    content.append(&icon);

    let label = gtk::Label::new(Some(title));
    label.add_css_class("sidebar-nav-label");
    label.set_hexpand(true);
    label.set_halign(gtk::Align::Fill);
    label.set_valign(gtk::Align::Fill);
    label.set_xalign(0.0);
    label.set_yalign(0.5);
    label.set_single_line_mode(true);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    content.append(&label);
    row.set_child(Some(&content));
    (row, label, content)
}

/// Keep the allocated content width, including manual window resizes.
pub fn resized_window_width(window_width: i32, old_sidebar: i32, new_sidebar: i32) -> i32 {
    window_width
        .saturating_sub(old_sidebar)
        .saturating_add(new_sidebar)
        .max(1)
}

/// GtkWindow allocations include decoration geometry that set_default_size
/// does not accept. GTK updates default_size on user resizes; use that paired
/// API or each toggle can grow both the content and the window height.
pub fn resize_origin(window: &impl IsA<gtk::Window>, sidebar: &gtk::Box) -> (i32, i32, i32) {
    let (width, height) = window.default_size();
    (width, height, sidebar.width())
}

#[allow(dead_code)]
pub fn create_section_header(title: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(&crate::i18n::upper(title)));
    label.set_halign(gtk::Align::Start);
    label.set_margin_start(12);
    label.set_margin_top(12);
    label.set_margin_bottom(4);
    label.add_css_class("dim-label");
    label.add_css_class("caption");
    label.add_css_class("sidebar-section-header");
    label
}

#[allow(dead_code)]
pub struct Sidebar;

#[allow(dead_code)]
impl Sidebar {
    pub fn new() -> Self {
        Self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_round_trip_restores_width_and_preserves_manual_resize() {
        let expanded = resized_window_width(960, 74, 280);
        assert_eq!(expanded, 1166);
        assert_eq!(resized_window_width(expanded, 280, 74), 960);
        assert_eq!(resized_window_width(expanded + 120, 280, 74), 1080);
        assert_eq!(resized_window_width(960, 74, 74), 960);
    }

    #[test]
    #[ignore = "requires a graphical GTK session; briefly opens a layout verification window"]
    fn rendered_navigation_keeps_glyph_ink_inside_padded_cells() {
        libadwaita::init().unwrap();
        let display = gtk::gdk::Display::default().unwrap();
        let css = gtk::CssProvider::new();
        css.load_from_string(include_str!("../../data/resources/style.css"));
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        let list = gtk::ListBox::new();
        list.add_css_class("navigation-sidebar");
        let mut labels = Vec::new();
        let mut row_contents = Vec::new();
        for title in ["Recognition", "Dictionary", "API", "Ύψος γραμμάτων"] {
            let (row, label, content) = navigation_row(title, "audio-input-microphone-symbolic");
            list.append(&row);
            labels.push(label);
            row_contents.push(content);
        }
        let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
        sidebar.set_width_request(280);
        let title = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        title.append(&gtk::Label::new(Some("Speech to Text")));
        let toggle = gtk::Button::from_icon_name("sidebar-show-symbolic");
        toggle.add_css_class("flat");
        sidebar.append(&header(&title, &toggle));
        sidebar.append(&list);
        let right = gtk::Box::new(gtk::Orientation::Vertical, 0);
        right.set_width_request(560);
        right.set_hexpand(true);
        let layout = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        layout.append(&sidebar);
        layout.append(&right);
        let window = libadwaita::Window::builder()
            .title("Sidebar layout verification")
            .default_width(900)
            .default_height(400)
            .content(&layout)
            .build();
        window.present();
        let settle = || {
            let context = gtk::glib::MainContext::default();
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
            while std::time::Instant::now() < deadline {
                while context.pending() {
                    context.iteration(false);
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        settle();
        for label in &labels {
            let (ink, _) = label.layout().pixel_extents();
            let (_, y) = label.layout_offsets();
            assert!(
                label.height() >= 32,
                "{}: allocation too short",
                label.text()
            );
            assert!(y + ink.y() >= 4, "{}: ink touches top", label.text());
            assert!(
                y + ink.y() + ink.height() <= label.height() - 4,
                "{}: ink touches bottom",
                label.text()
            );
        }
        let initial_size = window.default_size();
        let content_width = right.width();
        // Wayland configure/allocation completes asynchronously. Wait for the
        // requested geometry rather than assuming a fixed delay is sufficient.
        let wait_for_layout = |width, height, sidebar_width| {
            let context = gtk::glib::MainContext::default();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            loop {
                while context.pending() {
                    context.iteration(false);
                }
                if window.default_size() == (width, height)
                    && sidebar.width() == sidebar_width
                    && right.width() == width - sidebar_width
                {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "layout did not settle: window {:?}, sidebar {}, content {}; expected {width}x{height}, sidebar {sidebar_width}",
                    window.default_size(), sidebar.width(), right.width()
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        for (transition, sidebar_width) in [74, 280, 74, 280, 74, 280].into_iter().enumerate() {
            let (width, height, old_sidebar) = resize_origin(&window, &sidebar);
            title.set_visible(sidebar_width == 280);
            for label in &labels {
                label.set_visible(sidebar_width == 280);
            }
            for content in &row_contents {
                content.set_spacing(if sidebar_width == 280 { 12 } else { 0 });
                content.set_halign(if sidebar_width == 280 {
                    gtk::Align::Fill
                } else {
                    gtk::Align::Center
                });
            }
            if sidebar_width == 74 {
                sidebar.add_css_class("sidebar-collapsed");
            } else {
                sidebar.remove_css_class("sidebar-collapsed");
            }
            sidebar.set_width_request(sidebar_width);
            let (_, new_sidebar, _, _) = sidebar.measure(gtk::Orientation::Horizontal, -1);
            let new_width = resized_window_width(width, old_sidebar, new_sidebar);
            window.set_default_size(new_width, height);
            wait_for_layout(new_width, height, sidebar_width);
            assert_eq!(
                sidebar.width(),
                sidebar_width,
                "sidebar width is not fixed at transition {transition}; window {:?}, request {:?}",
                window.default_size(),
                sidebar.measure(gtk::Orientation::Horizontal, -1)
            );
            if sidebar_width == 74 {
                let restored_size = window.default_size();
                for resized_width in [760, 960, 1200, 840] {
                    window.set_default_size(resized_width, height);
                    wait_for_layout(resized_width, height, sidebar_width);
                    assert_eq!(
                        sidebar.width(),
                        74,
                        "collapsed sidebar changed at window width {resized_width}"
                    );
                }
                window.set_default_size(restored_size.0, restored_size.1);
                wait_for_layout(restored_size.0, restored_size.1, sidebar_width);
            }
            assert_eq!(
                right.width(),
                content_width,
                "content width drifted at sidebar width {sidebar_width}"
            );
            assert_eq!(
                window.default_size().1,
                initial_size.1,
                "window height drifted"
            );
        }
        assert_eq!(
            window.default_size(),
            initial_size,
            "window did not restore its initial size"
        );
        window.destroy();
        gtk::style_context_remove_provider_for_display(&display, &css);
    }
}
