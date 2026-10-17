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

/// Wall-clock epoch milliseconds for render-time relative stamps (the TS
/// `fmtRelative` reads `Date.now()` at render the same way).
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The TS `fmtRelative` (dayjs `fromNow`) against the active locale.
pub(crate) fn fmt_relative(ts_ms: u64, now_ms: u64) -> String {
    fmt_relative_in(&rust_i18n::locale().to_string(), ts_ms, now_ms)
}

/// dayjs `relativeTime` with its default thresholds, in the two catalog
/// locales (en / zh-cn wordings verbatim). A zero stamp renders empty like
/// TS; a stamp in the future clamps to "a few seconds ago" (history data
/// is past by construction).
pub(crate) fn fmt_relative_in(locale: &str, ts_ms: u64, now_ms: u64) -> String {
    if ts_ms == 0 {
        return String::new();
    }
    let zh = locale.to_ascii_lowercase().starts_with("zh");
    let secs = now_ms.saturating_sub(ts_ms) as f64 / 1000.0;
    let mins = secs / 60.0;
    let hours = mins / 60.0;
    let days = hours / 24.0;
    let months = days / 30.436875;
    let years = days / 365.2425;
    let r = |v: f64| v.round() as u64;
    // dayjs thresholds: the unit-less rows reuse the previous unit's value.
    if r(secs) <= 44 {
        return if zh { "几秒前".into() } else { "a few seconds ago".into() };
    }
    if r(secs) <= 89 {
        return if zh { "1 分钟前".into() } else { "a minute ago".into() };
    }
    if r(mins) <= 44 {
        let n = r(mins);
        return if zh { format!("{n} 分钟前") } else { format!("{n} minutes ago") };
    }
    if r(mins) <= 89 {
        return if zh { "1 小时前".into() } else { "an hour ago".into() };
    }
    if r(hours) <= 21 {
        let n = r(hours);
        return if zh { format!("{n} 小时前") } else { format!("{n} hours ago") };
    }
    if r(hours) <= 35 {
        return if zh { "1 天前".into() } else { "a day ago".into() };
    }
    if r(days) <= 25 {
        let n = r(days);
        return if zh { format!("{n} 天前") } else { format!("{n} days ago") };
    }
    if r(days) <= 45 {
        return if zh { "1 个月前".into() } else { "a month ago".into() };
    }
    if r(months) <= 10 {
        let n = r(months);
        return if zh { format!("{n} 个月前") } else { format!("{n} months ago") };
    }
    if r(months) <= 17 {
        return if zh { "1 年前".into() } else { "a year ago".into() };
    }
    let n = r(years).max(2);
    if zh { format!("{n} 年前") } else { format!("{n} years ago") }
}

/// Epoch ms from the bridge's ISO-8601 UTC stamps (`2026-10-01T10:00:00Z`,
/// optional fractional seconds); anything else is `None`. The history rows
/// carry ISO where discovery carries epoch ms.
pub(crate) fn parse_iso_ms(s: &str) -> Option<u64> {
    let s = s.trim().strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day): (i64, u32, u32) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut t = hms.split(':');
    let (hh, mm): (u64, u64) = (t.next()?.parse().ok()?, t.next()?.parse().ok()?);
    let ss: u64 = t.next().map(|v| v.parse().ok()).unwrap_or(Some(0))?;
    let millis: u64 = if frac.is_empty() {
        0
    } else {
        let digits: String = frac.chars().take(3).collect();
        let padded = format!("{digits:0<3}");
        padded.parse().ok()?
    };
    // Days from the civil date (Howard Hinnant's algorithm).
    let (y, m) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as i64;
    let doy = (153 * m as i64 + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    if days < 0 {
        return None;
    }
    Some((days as u64) * 86_400_000 + hh * 3_600_000 + mm * 60_000 + ss * 1_000 + millis)
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
    fn relative_stamps_follow_dayjs_thresholds_in_both_locales() {
        use super::{fmt_relative_in, parse_iso_ms};
        let now = 1_800_000_000_000u64;
        let ago = |secs: u64| now - secs * 1000;
        assert_eq!(fmt_relative_in("en", 0, now), "");
        assert_eq!(fmt_relative_in("en", ago(10), now), "a few seconds ago");
        assert_eq!(fmt_relative_in("zh-CN", ago(10), now), "几秒前");
        assert_eq!(fmt_relative_in("en", ago(60), now), "a minute ago");
        assert_eq!(fmt_relative_in("en", ago(5 * 60), now), "5 minutes ago");
        assert_eq!(fmt_relative_in("zh-CN", ago(5 * 60), now), "5 分钟前");
        assert_eq!(fmt_relative_in("en", ago(60 * 60), now), "an hour ago");
        assert_eq!(fmt_relative_in("en", ago(3 * 3600), now), "3 hours ago");
        assert_eq!(fmt_relative_in("en", ago(26 * 3600), now), "a day ago");
        assert_eq!(fmt_relative_in("zh-CN", ago(3 * 86_400), now), "3 天前");
        assert_eq!(fmt_relative_in("en", ago(40 * 86_400), now), "a month ago");
        assert_eq!(fmt_relative_in("en", ago(90 * 86_400), now), "3 months ago");
        assert_eq!(fmt_relative_in("en", ago(400 * 86_400), now), "a year ago");
        assert_eq!(fmt_relative_in("zh-CN", ago(800 * 86_400), now), "2 年前");
        // A future stamp clamps to the floor instead of "in …".
        assert_eq!(fmt_relative_in("en", now + 5_000, now), "a few seconds ago");

        // ISO stamps the history rows carry.
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso_ms("1970-01-02T00:00:01.5Z"), Some(86_401_500));
        assert_eq!(parse_iso_ms("2026-10-01T10:00:00Z"), Some(1_790_848_800_000));
        assert_eq!(parse_iso_ms("not a stamp"), None);
        assert_eq!(parse_iso_ms("2026-10-01"), None);
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
