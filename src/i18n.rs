// Copyright (C) 2026 Christos A. Daggas
// SPDX-License-Identifier: MIT

//! Translation helpers.
//!
//! User-facing strings are wrapped in [`gettext`] so they are extracted by
//! `xgettext` into `po/speech-to-text.pot` and translated via the per-locale
//! `.po`/`.mo` files loaded in `main`.

pub use gettextrs::gettext;

/// English source strings plus every shipped translation in po/LINGUAS.
pub const UI_LANGUAGES: &[(&str, &str)] = &[
    ("en", "English"),
    ("de", "Deutsch"),
    ("el", "Ελληνικά"),
    ("es", "Español"),
    ("fr", "Français"),
    ("it", "Italiano"),
    ("pt", "Português"),
    ("ru", "Русский"),
    ("zh", "中文"),
];

/// libc gettext disables LANGUAGE for C locales, including C.UTF-8 on glibc.
/// Use a real message locale; LANGUAGE still selects the shipped catalog.
pub fn message_locale(language: &str) -> &'static str {
    match language {
        "de" => "de_DE.UTF-8",
        "el" => "el_GR.UTF-8",
        "es" => "es_ES.UTF-8",
        "fr" => "fr_FR.UTF-8",
        "it" => "it_IT.UTF-8",
        "pt" => "pt_PT.UTF-8",
        "ru" => "ru_RU.UTF-8",
        "zh" => "zh_CN.UTF-8",
        _ => "en_US.UTF-8",
    }
}

/// Read just the startup locale, without starting the config writer or running
/// migrations: setlocale and environment changes must precede all threads.
pub fn configured_language() -> Option<String> {
    let bytes = std::fs::read(crate::config::AppConfig::config_file_path()).ok()?;
    language_from_config(&bytes)
}

fn language_from_config(bytes: &[u8]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct LocalePreference {
        #[serde(default)]
        ui_language: Option<String>,
    }
    let preference: LocalePreference = serde_json::from_slice(bytes).ok()?;
    preference
        .ui_language
        .filter(|code| UI_LANGUAGES.iter().any(|(id, _)| code == id))
}

/// Uppercase a label for the small-caps headings the stylesheet renders with
/// `text-transform: uppercase`.
///
/// Unicode's default casing keeps the Greek tonos (`Έτοιμο` → `ΈΤΟΙΜΟ`), but
/// Greek orthography drops it in all-caps — the accented form reads as a
/// spelling mistake. GTK's `text-transform` has no language-aware mode, so the
/// text is uppercased here instead; applying the CSS transform on top is then a
/// no-op and every other language is unaffected.
///
/// The dialytika is not an accent and stays (`ΪΫ`); a tonos combined with one
/// (`ΐ ΰ`) loses only the tonos.
pub fn upper(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.to_uppercase().chars() {
        match c {
            'Ά' => out.push('Α'),
            'Έ' => out.push('Ε'),
            'Ή' => out.push('Η'),
            'Ί' => out.push('Ι'),
            'Ό' => out.push('Ο'),
            'Ύ' => out.push('Υ'),
            'Ώ' => out.push('Ω'),
            // Combining acute, left behind when ΐ/ΰ are uppercased.
            '\u{0301}' => {}
            // Uppercasing ΐ/ΰ also decomposes the dialytika; recompose it so
            // the result is the single character a font expects.
            '\u{0308}' => match out.pop() {
                Some('Ι') => out.push('Ϊ'),
                Some('Υ') => out.push('Ϋ'),
                Some(prev) => {
                    out.push(prev);
                    out.push(c);
                }
                None => out.push(c),
            },
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::upper;

    #[test]
    fn interface_language_is_optional_validated_and_separate_from_speech() {
        assert_eq!(super::language_from_config(br#"{"language":"el"}"#), None);
        assert_eq!(
            super::language_from_config(br#"{"ui_language":"el","language":"en"}"#).as_deref(),
            Some("el")
        );
        assert_eq!(
            super::language_from_config(br#"{"ui_language":null}"#),
            None
        );
        assert_eq!(
            super::language_from_config(br#"{"ui_language":"unknown"}"#),
            None
        );
        assert_eq!(super::language_from_config(b"invalid"), None);
        let config = crate::config::AppConfig {
            ui_language: Some("el".into()),
            ..Default::default()
        };
        let restored: crate::config::AppConfig =
            serde_json::from_slice(&serde_json::to_vec(&config).unwrap()).unwrap();
        assert_eq!(restored.ui_language.as_deref(), Some("el"));
    }

    #[test]
    fn greek_all_caps_drops_the_tonos() {
        assert_eq!(upper("Έτοιμο"), "ΕΤΟΙΜΟ");
        assert_eq!(upper("Τρέχουσα συνεδρία"), "ΤΡΕΧΟΥΣΑ ΣΥΝΕΔΡΙΑ");
        assert_eq!(upper("Εγγραφή"), "ΕΓΓΡΑΦΗ");
    }

    #[test]
    fn greek_dialytika_survives_but_its_tonos_does_not() {
        assert_eq!(upper("προϊόν"), "ΠΡΟΪΟΝ");
        assert_eq!(upper("ενδεικτικό ΐ"), "ΕΝΔΕΙΚΤΙΚΟ Ϊ");
    }

    #[test]
    fn other_languages_are_plain_uppercase() {
        assert_eq!(upper("Current session"), "CURRENT SESSION");
        assert_eq!(upper("Aktuelle Sitzung"), "AKTUELLE SITZUNG");
        assert_eq!(upper("Sesión actual"), "SESIÓN ACTUAL");
        assert_eq!(upper("Sessione corrente"), "SESSIONE CORRENTE");
    }

    /// The stylesheet still applies `text-transform: uppercase`; running it over
    /// an already-uppercased string must not change it again.
    #[test]
    fn applying_it_twice_is_stable() {
        for s in ["Έτοιμο", "Τρέχουσα συνεδρία", "Current session", "προϊόν"]
        {
            assert_eq!(upper(&upper(s)), upper(s));
        }
    }
}
