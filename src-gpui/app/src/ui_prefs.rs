//! Persisted UI preferences (batch H3) — the desktop counterpart of the TS
//! localStorage `zcode-acp:lang` key: the locale picked in the settings
//! panel's two-segment control, overriding the system detection at startup.
//! One JSON file beside the server book (`protocol::profiles::config_dir`);
//! a missing file means "follow the system" (the pre-H3 behavior).
//!
//! Hand-rolled JSON (the app layer carries serde_json, not serde's derives
//! — its Cargo layering note lists exactly tokio + serde_json beyond
//! kit/protocol); the shape is one optional `locale` string.

use std::path::PathBuf;

/// The two catalog locales (`src/i18n/`), as the prefs file and the segment
/// control spell them.
pub(crate) const LOCALE_EN: &str = "en";
pub(crate) const LOCALE_ZH: &str = "zh-CN";

/// The prefs file location: `ui-prefs.json` beside the server book.
pub(crate) fn prefs_path() -> PathBuf {
    protocol::profiles::config_dir().join("ui-prefs.json")
}

/// A stored locale, normalized onto the two catalogs — anything else (an
/// edited file, a retired locale) reads as "no preference" and startup falls
/// back to the system detection.
pub(crate) fn stored_locale(raw: Option<&str>) -> Option<&'static str> {
    match raw {
        Some(locale) if locale.to_ascii_lowercase().starts_with("zh") => Some(LOCALE_ZH),
        Some(locale) if locale.eq_ignore_ascii_case(LOCALE_EN) => Some(LOCALE_EN),
        _ => None,
    }
}

/// Read the persisted locale choice, if any (missing/corrupt file = none).
pub(crate) fn load_locale() -> Option<&'static str> {
    let text = std::fs::read_to_string(prefs_path()).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    stored_locale(value.get("locale").and_then(serde_json::Value::as_str))
}

/// Persist the locale choice (the segment control's click; the TS saveLang).
pub(crate) fn store_locale(locale: &str) {
    let path = prefs_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(body) = serde_json::to_string_pretty(&serde_json::json!({ "locale": locale })) {
        // Temp-file + rename: a crash mid-write must not leave a truncated
        // prefs file (a corrupt read silently falls back to system
        // detection). A failed write never blocks the switch — the pick
        // still applies to this run, it just does not survive a restart.
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, format!("{body}\n")).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{stored_locale, LOCALE_EN, LOCALE_ZH};

    #[test]
    fn stored_locales_normalize_onto_the_catalogs() {
        assert_eq!(stored_locale(Some("en")), Some(LOCALE_EN));
        assert_eq!(stored_locale(Some("EN")), Some(LOCALE_EN));
        assert_eq!(stored_locale(Some("zh-CN")), Some(LOCALE_ZH));
        assert_eq!(stored_locale(Some("zh-Hans-CN")), Some(LOCALE_ZH));
        assert_eq!(stored_locale(Some("zh")), Some(LOCALE_ZH));
        // Unknown or retired spellings, and no preference, follow the system.
        assert_eq!(stored_locale(Some("fr")), None);
        assert_eq!(stored_locale(Some("")), None);
        assert_eq!(stored_locale(None), None);
    }
}
