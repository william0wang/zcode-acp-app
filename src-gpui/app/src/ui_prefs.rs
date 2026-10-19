//! Persisted UI preferences (batch H3) — the desktop counterpart of the TS
//! localStorage keys: the locale picked in the settings panel's two-segment
//! control (`zcode-acp:lang`, overriding the system detection at startup)
//! and the App Update page's release channel (TS `saveUpdateChannel`).
//! One JSON file beside the server book (`protocol::profiles::config_dir`);
//! a missing file or key means "no preference" (system locale / stable).
//!
//! Hand-rolled JSON (the app layer carries serde_json, not serde's derives
//! — its Cargo layering note lists exactly tokio + serde_json beyond
//! kit/protocol); the shape is one flat object of optional string keys,
//! read-modify-written so one preference's save never drops another's.

use std::path::PathBuf;

use serde_json::{Map, Value};

/// The two catalog locales (`src/i18n/`), as the prefs file and the segment
/// control spell them.
pub(crate) const LOCALE_EN: &str = "en";
pub(crate) const LOCALE_ZH: &str = "zh-CN";

/// The two release channels (TS `UpdateChannel`).
pub(crate) const CHANNEL_STABLE: &str = "stable";
pub(crate) const CHANNEL_PREVIEW: &str = "preview";

/// The chat font ladder (TS `FontSize`): three steps over the reading text.
/// Medium is the desktop default — the agent prose inherits the window's
/// 16px default, which is the size the ladder was tuned around.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FontScale {
    Small,
    Medium,
    Large,
}

impl FontScale {
    pub(crate) fn key(self) -> &'static str {
        match self {
            FontScale::Small => "small",
            FontScale::Medium => "medium",
            FontScale::Large => "large",
        }
    }

    /// The (user, agent) chat text sizes in px. Agent prose rides the kit's
    /// TextView, which inherits the wrapper's text_size — the wrapper sets it
    /// so the ladder scales both roles uniformly.
    pub(crate) fn chat_px(self) -> (f32, f32) {
        match self {
            FontScale::Small => (14.0, 15.0),
            FontScale::Medium => (15.0, 16.0),
            FontScale::Large => (16.5, 17.5),
        }
    }
}

/// A stored font size, normalized onto the ladder — anything else reads as
/// "no preference" (medium).
pub(crate) fn stored_font_size(raw: Option<&str>) -> Option<FontScale> {
    match raw {
        Some(size) if size.eq_ignore_ascii_case("small") => Some(FontScale::Small),
        Some(size) if size.eq_ignore_ascii_case("large") => Some(FontScale::Large),
        Some(size) if size.eq_ignore_ascii_case("medium") => Some(FontScale::Medium),
        _ => None,
    }
}

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

/// A stored release channel, normalized onto the two streams — anything
/// else reads as "no preference" (stable).
pub(crate) fn stored_channel(raw: Option<&str>) -> Option<&'static str> {
    match raw {
        Some(channel) if channel.eq_ignore_ascii_case(CHANNEL_PREVIEW) => Some(CHANNEL_PREVIEW),
        Some(channel) if channel.eq_ignore_ascii_case(CHANNEL_STABLE) => Some(CHANNEL_STABLE),
        _ => None,
    }
}

/// The whole prefs object (missing/corrupt file = empty).
fn load_prefs() -> Map<String, Value> {
    std::fs::read_to_string(prefs_path())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| match value {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default()
}

/// Persist one preference, keeping the others. Temp-file + rename: a crash
/// mid-write must not leave a truncated prefs file (a corrupt read silently
/// falls back to the defaults). A failed write never blocks the pick — it
/// still applies to this run, it just does not survive a restart.
fn store_pref(key: &str, value: Value) {
    let path = prefs_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut prefs = load_prefs();
    prefs.insert(key.to_string(), value);
    if let Ok(body) = serde_json::to_string_pretty(&Value::Object(prefs)) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, format!("{body}\n")).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// Read the persisted locale choice, if any (missing/corrupt file = none).
pub(crate) fn load_locale() -> Option<&'static str> {
    stored_locale(load_prefs().get("locale").and_then(Value::as_str))
}

/// Persist the locale choice (the segment control's click; the TS saveLang).
pub(crate) fn store_locale(locale: &str) {
    store_pref("locale", Value::String(locale.to_string()));
}

/// Read the persisted release channel, if any.
pub(crate) fn load_update_channel() -> Option<&'static str> {
    stored_channel(load_prefs().get("updateChannel").and_then(Value::as_str))
}

/// Persist the release channel (the App Update page's segment; the TS
/// `saveUpdateChannel`).
pub(crate) fn store_update_channel(channel: &str) {
    store_pref("updateChannel", Value::String(channel.to_string()));
}

/// Read the persisted chat font size, if any.
pub(crate) fn load_font_size() -> Option<FontScale> {
    stored_font_size(load_prefs().get("fontSize").and_then(Value::as_str))
}

/// Persist the chat font size (the settings segment's click; the TS
/// `saveFontSize`).
pub(crate) fn store_font_size(size: &str) {
    store_pref("fontSize", Value::String(size.to_string()));
}

#[cfg(test)]
mod tests {
    use super::{
        stored_channel, stored_font_size, stored_locale, FontScale, CHANNEL_PREVIEW,
        CHANNEL_STABLE, LOCALE_EN, LOCALE_ZH,
    };

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

    #[test]
    fn stored_channels_normalize_onto_the_two_streams() {
        assert_eq!(stored_channel(Some("preview")), Some(CHANNEL_PREVIEW));
        assert_eq!(stored_channel(Some("Preview")), Some(CHANNEL_PREVIEW));
        assert_eq!(stored_channel(Some("stable")), Some(CHANNEL_STABLE));
        assert_eq!(stored_channel(Some("nightly")), None);
        assert_eq!(stored_channel(None), None);
    }

    #[test]
    fn stored_font_sizes_normalize_onto_the_ladder() {
        assert_eq!(stored_font_size(Some("small")), Some(FontScale::Small));
        assert_eq!(stored_font_size(Some("Large")), Some(FontScale::Large));
        assert_eq!(stored_font_size(Some("medium")), Some(FontScale::Medium));
        assert_eq!(stored_font_size(Some("huge")), None);
        assert_eq!(stored_font_size(None), None);
        // The ladder steps up in both roles.
        let (su, sa) = FontScale::Small.chat_px();
        let (mu, ma) = FontScale::Medium.chat_px();
        let (lu, la) = FontScale::Large.chat_px();
        assert!(su < mu && mu < lu && sa < ma && ma < la);
    }
}
