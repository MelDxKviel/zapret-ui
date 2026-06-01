//! Runtime translation catalog.
//!
//! Locale strings live as flat `"key": "value"` JSON maps under `src/locales/`
//! and are embedded into the binary at compile time (single-binary target).
//! [`tr`] is the one lookup used by both sides of the app: the Slint `I18n.t`
//! callback (UI strings) and the backend (the few status messages it builds).

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::config::Language;

const RU_JSON: &str = include_str!("locales/ru.json");
const EN_JSON: &str = include_str!("locales/en.json");

/// Language code Slint and the catalog use. Keep in sync with [`Language`].
pub const RU: &str = "ru";
pub const EN: &str = "en";

struct Catalog {
    ru: HashMap<String, String>,
    en: HashMap<String, String>,
}

fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| Catalog {
        ru: serde_json::from_str(RU_JSON).expect("src/locales/ru.json is valid JSON"),
        en: serde_json::from_str(EN_JSON).expect("src/locales/en.json is valid JSON"),
    })
}

/// The language code for a config [`Language`].
pub fn code(lang: Language) -> &'static str {
    match lang {
        Language::Ru => RU,
        Language::En => EN,
    }
}

/// Look up `key` for `lang`. Falls back to English, then to the key itself, so
/// a missing translation degrades gracefully instead of rendering blank.
pub fn tr(lang: &str, key: &str) -> String {
    let c = catalog();
    let primary = if lang == EN { &c.en } else { &c.ru };
    if let Some(v) = primary.get(key) {
        return v.clone();
    }
    if let Some(v) = c.en.get(key) {
        return v.clone();
    }
    key.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogs_parse_and_share_keys() {
        let c = catalog();
        assert!(!c.ru.is_empty() && !c.en.is_empty());
        // Every English key must have a Russian counterpart (and vice versa),
        // otherwise some screen silently falls back to English.
        let mut missing_ru: Vec<&String> = c.en.keys().filter(|k| !c.ru.contains_key(*k)).collect();
        let mut missing_en: Vec<&String> = c.ru.keys().filter(|k| !c.en.contains_key(*k)).collect();
        missing_ru.sort();
        missing_en.sort();
        assert!(
            missing_ru.is_empty(),
            "keys missing from ru.json: {missing_ru:?}"
        );
        assert!(
            missing_en.is_empty(),
            "keys missing from en.json: {missing_en:?}"
        );
    }

    #[test]
    fn falls_back_to_key_when_unknown() {
        assert_eq!(tr(RU, "no.such.key"), "no.such.key");
    }

    #[test]
    fn returns_russian_by_default() {
        assert_eq!(tr(RU, "common.cancel"), "Отмена");
        assert_eq!(tr(EN, "common.cancel"), "Cancel");
    }

    #[test]
    fn slint_i18n_keys_exist_in_catalogs() {
        let files = [
            include_str!("../ui/main_window.slint"),
            include_str!("../ui/pages/home.slint"),
            include_str!("../ui/pages/strategies.slint"),
            include_str!("../ui/pages/tester.slint"),
            include_str!("../ui/pages/logs.slint"),
            include_str!("../ui/pages/settings.slint"),
            include_str!("../ui/pages/about.slint"),
            include_str!("../ui/components/admin_banner.slint"),
            include_str!("../ui/components/hosts_dialog.slint"),
            include_str!("../ui/components/update_banner.slint"),
        ];
        let c = catalog();
        let mut missing = Vec::new();
        for content in files {
            let mut rest = content;
            while let Some(pos) = rest.find("I18n.t(") {
                rest = &rest[pos + "I18n.t(".len()..];
                let Some(q1) = rest.find('"') else { continue };
                let after_q1 = &rest[q1 + 1..];
                let Some(q2) = after_q1.find('"') else {
                    continue;
                };
                let key = &after_q1[..q2];
                if !c.ru.contains_key(key) || !c.en.contains_key(key) {
                    missing.push(key.to_string());
                }
                rest = &after_q1[q2 + 1..];
            }
        }
        missing.sort();
        missing.dedup();
        assert!(
            missing.is_empty(),
            "Slint keys missing from catalogs: {missing:?}"
        );
    }
}
