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

use rust_i18n::t;

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

/// An i18next-style plural lookup (the `count` counterpart of [`fill`]'s
/// seam): the shared catalogs pluralize with `_one`/`_other` suffixes that
/// rust-i18n's exact-key resolution never tries, and locales without plural
/// forms (zh) carry only the bare key. Try the count-appropriate suffix,
/// then the bare spelling, then the `en` fallback — the `{{count}}`
/// placeholder still goes through [`fill`] afterwards.
pub(crate) fn t_count(key: &str, count: u64) -> String {
    t_count_in(&rust_i18n::locale().to_string(), key, count)
}

/// [`t_count`] against an explicit locale (tests; the language switch flips
/// the process-global the wrapper reads).
pub(crate) fn t_count_in(locale: &str, key: &str, count: u64) -> String {
    let suffix = if count == 1 { "_one" } else { "_other" };
    let suffixed = format!("{key}{suffix}");
    // The catalogs' plural convention: `en` carries the `_one`/`_other`
    // variants, zh (no plural forms) carries the bare key. The ACTIVE
    // locale's own spelling must win — i18next resolves [key_suffix, key]
    // per language before crossing to the fallback language, and asking
    // rust-i18n for the suffixed key in zh would answer with the `en`
    // suffixed value straight away.
    if locale.eq_ignore_ascii_case("en") {
        let hit = t!(suffixed.as_str(), locale = "en");
        if hit != suffixed.as_str() {
            return hit.into_owned();
        }
        return t!(key, locale = "en").into_owned();
    }
    let bare = t!(key, locale = locale);
    if bare != key {
        return bare.into_owned();
    }
    // No bare key in the active locale (or its fallbacks): the suffixed
    // spelling, with rust-i18n's own en fallback answering if it can.
    t!(suffixed.as_str(), locale = locale).into_owned()
}

/// Serializes tests that touch the PROCESS-GLOBAL rust-i18n locale against
/// tests that read it. The crate's unit tests share one process (parallel
/// threads); the language-switch UI test flips the global for a moment, and
/// without this lock that window races the `t!("chat.send")` assertions
/// below. Every test that calls bare `t!()` (no explicit `locale =`) takes
/// this lock, as does the switch test itself.
#[cfg(test)]
pub(crate) fn locale_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    // Explicit imports, NOT `use super::*` — matches the sibling test
    // modules' shape (the kit glob shadowing note in workbench.rs).
    use super::{detect_locale, fill, locale_from_system};
    use rust_i18n::t;

    #[test]
    fn nested_keys_resolve_in_both_catalogs() {
        // Bare t!() reads the process-global locale — hold the locale lock
        // against the language-switch UI test's flips (locale_test_lock).
        let _g = super::locale_test_lock();
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
    fn plural_count_keys_resolve_per_locale() {
        use super::{fill, t_count_in};
        // en pluralizes through the suffixed keys (count == 1 → _one).
        assert_eq!(
            t_count_in("en", "zconfig.hooksEntryCount", 1),
            "{{count}} entry"
        );
        assert_eq!(
            t_count_in("en", "zconfig.hooksEntryCount", 2),
            "{{count}} entries"
        );
        // zh has no plural forms — the bare key answers for any count.
        assert_eq!(
            t_count_in("zh-CN", "zconfig.hooksEntryCount", 2),
            "{{count}} 条"
        );
        // The resolved template still interpolates through fill.
        assert_eq!(
            fill(
                &t_count_in("en", "zconfig.hooksEntryCount", 2),
                &[("count", "3")]
            ),
            "3 entries"
        );
        // A key no catalog carries leaks verbatim (the t! miss contract).
        assert_eq!(
            t_count_in("en", "zconfig.noSuchKeyAnywhere", 2),
            "zconfig.noSuchKeyAnywhere"
        );
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
        // Bare t!() reads the process-global locale (see locale_test_lock).
        let _g = super::locale_test_lock();
        // The notice banners' shape: a `notice.*` key resolves, plain text
        // passes through verbatim.
        assert_eq!(
            t!("notice.configNeedsRestart"),
            "Saved — restart the backend to apply it"
        );
        assert_eq!(t!("plain failure text"), "plain failure text");
    }
}
