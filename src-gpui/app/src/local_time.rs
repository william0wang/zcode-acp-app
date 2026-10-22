//! Local-wall-clock stamps for the config surfaces (quota page and card,
//! workflows, system status). The bridge reports epoch ms; every TS client
//! renders those through `new Date` — LOCAL time (ConfigPage `fmtStamp`,
//! QuotaSection `fmtResetTime`), so the desktop must too. Rust std has no
//! local-timezone conversion and the workspace pins no date crate, so the
//! zone comes from the platform: `localtime_r` reads the same
//! `/etc/localtime` (or `TZ`) every native app on the machine uses, DST
//! transitions included. `libc` already sits in the workspace tree via
//! gpui-kit's dependencies — no new external crate (the same pin doctrine
//! as rust-i18n / sys-locale).

/// The `MM-DD HH:MM` stamp every config surface paints (TS ConfigPage
/// `fmtStamp`, QuotaSection `fmtResetTime`), in the machine's local zone.
/// No zone suffix: it is the user's own clock, exactly like the mobile/web
/// pages.
pub(crate) fn fmt_stamp(ms: u64) -> String {
    match local_parts(ms) {
        Some((month, day, hour, minute)) => {
            format!("{month:02}-{day:02} {hour:02}:{minute:02}")
        }
        // No platform zone answered (non-unix today): the same layout on
        // the UTC basis — still aligned, still sortable, never a blank.
        None => fmt_stamp_utc_basis(ms),
    }
}

/// The local (month, day, hour, minute) at `ms`, per the platform zone.
#[cfg(unix)]
fn local_parts(ms: u64) -> Option<(i32, i32, i32, i32)> {
    let secs: libc::time_t = (ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `tm` is a valid out-pointer exclusively owned for the call,
    // and `localtime_r` touches no static state (unlike `localtime`).
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return None;
    }
    // tm_mon is 0-based; the rest read as displayed.
    Some((tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min))
}

#[cfg(not(unix))]
fn local_parts(_ms: u64) -> Option<(i32, i32, i32, i32)> {
    None
}

/// Pure civil-from-days arithmetic (Howard Hinnant) over the UTC basis —
/// the fallback when no platform zone answers.
fn fmt_stamp_utc_basis(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, mi) = (rem / 3600, (rem % 3600) / 60);
    let z = days + 719_468;
    // The era (400-year blocks) only positions the year; month/day read
    // off the day-of-era alone.
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    debug_assert!((1..=12).contains(&m) && (1..=31).contains(&d));
    format!("{m:02}-{d:02} {h:02}:{mi:02}")
}

#[cfg(all(test, unix))]
mod tests {
    use super::fmt_stamp;

    /// TZ is process-global state: the machine's zone is pinned only while
    /// this lock is held, and any other TZ-sensitive test must take it too.
    fn tz_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn set_tz(tz: &str) {
        // macOS localtime_r re-reads `TZ` on every call (probed: a
        // mid-process zone change is honored with no tzset), so the pin
        // takes effect on the next stamp.
        std::env::set_var("TZ", tz);
    }

    #[test]
    fn stamps_follow_the_machine_zone() {
        let _g = tz_lock();
        // 2023-11-14 22:13:20 UTC.
        set_tz("Asia/Shanghai");
        assert_eq!(fmt_stamp(1_700_000_000_000), "11-15 06:13");
        assert_eq!(fmt_stamp(0), "01-01 08:00");
        set_tz("UTC0");
        assert_eq!(fmt_stamp(1_700_000_000_000), "11-14 22:13");
        assert_eq!(fmt_stamp(0), "01-01 00:00");
        // Restore the machine's own zone for whatever runs after.
        std::env::remove_var("TZ");
    }

    #[test]
    fn stamps_follow_dst_transitions() {
        let _g = tz_lock();
        // America/New_York: EST (UTC-5) on 2023-11-14 but EDT (UTC-4) on
        // 2023-07-22 — the zone at the STAMP's instant decides, not the
        // zone the machine sits in now.
        set_tz("America/New_York");
        assert_eq!(fmt_stamp(1_700_000_000_000), "11-14 17:13");
        assert_eq!(fmt_stamp(1_690_000_000_000), "07-22 00:26");
        std::env::remove_var("TZ");
    }
}
