//! The shared i18n catalogs (requirements §2): the desktop line localizes
//! through the SAME JSON the mobile/web clients ship — `src/i18n/en.json`
//! and `src/i18n/zh-CN.json`, loaded at compile time by rust-i18n straight
//! from that directory (never forked or copied). The catalogs are i18next
//! resources: nested keys flatten to dotted paths (`zconfig.models`), and
//! placeholders are `{{name}}`. rust-i18n resolves dotted keys natively but
//! only interpolates its own `%{name}` markers, so strings WITH placeholders
//! go through [`fill`] after `t!` instead of `t!`'s own args — the one
//! deliberate seam that keeps the TS files untouched.
//!
//! Locale choice is the system's (`sys-locale`, the same crate cosmic-text
//! already pulls in): the catalogs carry exactly `en` and `zh-CN`, so any
//! Chinese system locale maps onto `zh-CN` and everything else onto `en`
//! (plus rust-i18n's `en` fallback for missed keys).

/// Maps a system locale onto one of the two catalog locales. macOS reports
/// e.g. `zh-Hans-CN`; the catalog's only Chinese is `zh-CN`, so the whole
/// `zh` family collapses onto it rather than relying on rust-i18n's
/// territory fallback (which would step `zh-Hans-CN -> zh-Hans -> zh` and
/// miss the catalog).
pub(crate) fn locale_from_system(raw: Option<&str>) -> &'static str {
    match raw {
        Some(locale) if locale.to_ascii_lowercase().starts_with("zh") => "zh-CN",
        _ => "en",
    }
}

/// The system locale, normalized for rust-i18n. Called once at startup.
pub(crate) fn detect_locale() -> &'static str {
    locale_from_system(sys_locale::get_locale().as_deref())
}

/// Fills an i18next-style `{{name}}` placeholder in a translated string.
/// rust-i18n interpolates only `%{name}`, and rewriting the shared catalogs
/// to that syntax is exactly the fork requirements §2 forbids — so keys with
/// placeholders are fetched whole and filled here.
pub(crate) fn fill(text: &str, args: &[(&str, &str)]) -> String {
    let mut out = text.to_string();
    for (name, value) in args {
        out = out.replace(&format!("{{{{{name}}}}}"), value);
    }
    out
}

#[cfg(test)]
mod tests {
    // Explicit imports, NOT `use super::*` — matches the sibling test
    // modules' shape (the kit glob shadowing note in workbench.rs).
    use super::{detect_locale, fill, locale_from_system};
    use rust_i18n::t;

    #[test]
    fn nested_keys_resolve_in_both_catalogs() {
        assert_eq!(t!("chat.send"), "Send");
        assert_eq!(t!("chat.send", locale = "zh-CN"), "发送");
        assert_eq!(t!("permission.hint"), "The first answer across all clients wins");
        assert_eq!(
            t!("permission.hint", locale = "zh-CN"),
            "所有客户端中最先应答的生效"
        );
        assert_eq!(t!("zconfig.models"), "Models");
        assert_eq!(t!("zconfig.models", locale = "zh-CN"), "模型");
        assert_eq!(t!("system.title"), "System status");
    }

    #[test]
    fn missed_locales_fall_back_to_en() {
        // No zh catalog for zh-TW: the territory fallback walks zh-TW -> zh,
        // finds nothing, and the `en` fallback list answers — never the
        // locale-prefixed key form a raw miss would leak.
        assert_eq!(t!("chat.send", locale = "zh-TW"), "Send");
    }

    #[test]
    fn fill_replaces_i18next_placeholders() {
        assert_eq!(
            fill(
                &t!("zconfig.resetConfirm", locale = "zh-CN"),
                &[("title", "5小时窗口"), ("expires", "11-14 22:13 UTC")]
            ),
            "使用一张 5小时窗口 卡（11-14 22:13 UTC 到期）？此操作不可撤销。"
        );
        // An absent placeholder stays literal; an extra arg is a no-op.
        assert_eq!(fill("Below {{pct}}%", &[("pct", "90")]), "Below 90%");
        assert_eq!(fill("plain text", &[("pct", "90")]), "plain text");
    }

    #[test]
    fn system_locales_collapse_onto_the_catalogs() {
        assert_eq!(locale_from_system(Some("zh-Hans-CN")), "zh-CN");
        assert_eq!(locale_from_system(Some("zh_TW")), "zh-CN");
        assert_eq!(locale_from_system(Some("en_US")), "en");
        assert_eq!(locale_from_system(Some("fr_FR")), "en");
        assert_eq!(locale_from_system(None), "en");
        // The live pick only asserts the shape — the build machine's own
        // locale decides the value.
        assert!(detect_locale() == "en" || detect_locale() == "zh-CN");
    }

    #[test]
    fn runtime_keys_translate_or_fall_back_to_the_input() {
        // The notice banners' shape: a `notice.*` key resolves, plain text
        // passes through verbatim.
        assert_eq!(
            t!("notice.configNeedsRestart"),
            "Saved — restart the backend to apply it"
        );
        assert_eq!(t!("plain failure text"), "plain failure text");
    }
}
