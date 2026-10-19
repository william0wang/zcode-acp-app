//! The System Status configuration page (requirements §4, M3) — the port of
//! `src/screens/config/SystemStatusPage.tsx` plus the config-page frame
//! semantics it rides (`src/components/config/ConfigPage.tsx`): back header,
//! refresh, the notice banner where Effect Class outcomes surface, the
//! Backend Restart affordance, and the loading/unsupported/error states.
//!
//! Semantics kept from the TS page:
//! - A dashboard, not an editor: every field is read-only, and every null
//!   field is a probe the hub could not run (denied inside a sandbox, absent
//!   hardware) — rendered as "—", never as an error.
//! - Rate fields (CPU %, transfer speeds, hub CPU %) are hub-side deltas:
//!   the first read after a hub start shows "…" until a second read lands.
//! - State precedence (ConfigPageFrame): unsupported (a 404'd route — this
//!   page alone, bridge < 0.62.0) > first-load error > loading > content.
//!   A transient failure over an existing snapshot paints nothing — the
//!   dashboard keeps working.
//! - The restart note is shown ONLY while a needs-restart write is pending;
//!   restarting cancels in-flight turns, so it is never automatic, and with
//!   no session attached there is no bridge to name so the note says so.
//!
//! Strings come from the shared rust-i18n catalogs (`system.*`,
//! `zconfig.*`, `quota.*`, `common.*`, `notice.*`; requirements §2);
//! `…`/`—` are the TS PENDING/PLACEHOLDER glyphs.

use gpui::prelude::FluentBuilder as _;
use gpui::{relative, AnyElement, App, Hsla, div, px, rgb, white};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, Theme};
// The kit glob also brings gpui's self-alias `gpui` into scope — the
// established import shape (workbench.rs, approval_card.rs).
use gpui_kit::*;
use protocol::system_stats::{SystemStatsResponse, SystemStatsPowerSource, SystemStatsVolume};
use rust_i18n::t;

use crate::approval_card::render_toast_strip;
use crate::i18n::fill;
use crate::runtime::{ProtocolRuntime, SystemStatusPageView, UiCommand};

/// Null probe placeholder (TS `PLACEHOLDER`).
const PLACEHOLDER: &str = "—";
/// First-read rate placeholder — the hub has no baseline yet (TS `PENDING`).
const PENDING: &str = "…";

// ---------------------------------------------------------------------------
// Formatters (the TS page's helpers, exact ports)
// ---------------------------------------------------------------------------

/// `87 B`, `1.5 KB`, `234 MB` — one decimal under 100 (bare bytes never
/// get one), integers above (TS `fmtBytes`).
pub(crate) fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0usize;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    let shown = if i == 0 || v >= 100.0 {
        format!("{}", v.round() as u64)
    } else {
        let tenths = (v * 10.0).round() / 10.0;
        // JS `${1.5}` prints "1.5" and `${2}` prints "2" — drop a whole
        // number's trailing ".0".
        if tenths.fract() == 0.0 {
            format!("{}", tenths as u64)
        } else {
            format!("{tenths:.1}")
        }
    };
    format!("{shown} {}", UNITS[i])
}

/// `… KB/s` for a rate without its baseline yet (TS `fmtRate`).
fn fmt_rate(bytes_per_s: Option<u64>) -> String {
    match bytes_per_s {
        None => PENDING.to_string(),
        Some(n) => format!("{}/s", fmt_bytes(n)),
    }
}

/// `45s` / `12m` / `3h 40m` / `2d 5h` (TS `fmtUptime`).
fn fmt_uptime(s: u64) -> String {
    if s < 60 {
        return format!("{s}s");
    }
    let m = s / 60;
    if m < 60 {
        return format!("{m}m");
    }
    let h = m / 60;
    if h < 24 {
        return format!("{h}h {}m", m % 60);
    }
    format!("{}d {}h", h / 24, h % 24)
}

/// `—` / `30m` / `2h 5m` for battery time remaining (TS `fmtMinutes`).
fn fmt_minutes(min: Option<u64>) -> String {
    match min {
        None => PLACEHOLDER.to_string(),
        Some(min) if min < 60 => format!("{min}m"),
        Some(min) => format!("{}h {}m", min / 60, min % 60),
    }
}

/// CLI-parity heat color (TS `heatColor`): green → yellow → red,
/// piecewise-linear at 0/50/100%.
fn heat_rgb(pct: f64) -> (u8, u8, u8) {
    let p = pct.clamp(0.0, 100.0);
    let lerp = |a: f64, b: f64, t: f64| (a + (b - a) * t).round() as u8;
    if p < 50.0 {
        let t = p / 50.0;
        (lerp(34.0, 234.0, t), lerp(197.0, 179.0, t), lerp(94.0, 8.0, t))
    } else {
        let t = (p - 50.0) / 50.0;
        (lerp(234.0, 239.0, t), lerp(179.0, 68.0, t), lerp(8.0, 68.0, t))
    }
}

fn heat_color(pct: f64) -> Hsla {
    let (r, g, b) = heat_rgb(pct);
    rgb(((r as u32) << 16) | ((g as u32) << 8) | b as u32).into()
}

/// The collected stamp. TS `fmtStamp` renders LOCAL `MM-DD HH:MM`; Rust std
/// has no local-timezone conversion and the workspace pins no date crate,
/// so this renders UTC with an explicit suffix until the M4 i18n pass.
/// Pure civil-from-days arithmetic (Howard Hinnant), no dependencies.
fn fmt_stamp_utc(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, mi) = (rem / 3600, (rem % 3600) / 60);
    let z = days + 719_468;
    // Only the day-of-era feeds month/day (the year itself is not shown).
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    debug_assert!((1..=12).contains(&m) && (1..=31).contains(&d));
    format!("{m:02}-{d:02} {h:02}:{mi:02} UTC")
}

/// Last non-empty path segment (the TS bridge-row label:
/// `workspace.split("/").filter(Boolean).pop()`), falling back to the full
/// string — and the caller falls back to the bridge id for a null workspace.
fn workspace_leaf(workspace: &str) -> &str {
    workspace
        .split('/')
        .filter(|s| !s.is_empty())
        .next_back()
        .unwrap_or(workspace)
}

/// The `notice.*` i18n keys the store surfaces, resolved through the shared
/// catalog (TS renders keys through `t()`; `t!` falls back to its input, so
/// plain failure text passes through verbatim).
fn notice_text(notice: &str) -> String {
    t!(notice).into_owned()
}

/// pmset wording → the catalog's display forms (TS `battStatus` map; an
/// unrecognized status passes through, a null one is nothing at all).
fn battery_status(status: Option<&str>) -> Option<String> {
    status.map(|s| match s {
        "charging" => t!("system.battCharging").into_owned(),
        "discharging" => t!("system.battDischarging").into_owned(),
        "charged" => t!("system.battCharged").into_owned(),
        other => other.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Row primitives (ConfigRow / BarRow / ConfigBlock ports)
// ---------------------------------------------------------------------------

fn faint(theme: &Theme) -> gpui::Hsla {
    // TS text-faint — one step dimmer than text-dim (muted_foreground).
    theme.muted_foreground.opacity(0.75)
}

/// A read-only label/value row, the shape most configuration data takes
/// (TS `ConfigRow`); `hint` is the optional second line under the pair.
fn config_row(theme: &Theme, label: &str, value: String, hint: Option<String>) -> AnyElement {
    let mut row = div().px(px(16.)).py(px(10.)).child(
        div()
            .flex()
            .items_baseline()
            .justify_between()
            .gap(px(12.))
            .child(
                div()
                    .flex_none()
                    .text_size(px(13.))
                    .text_color(theme.muted_foreground)
                    .child(label.to_string()),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(13.))
                    .text_color(theme.foreground)
                    .child(value),
            ),
    );
    if let Some(hint) = hint {
        row = row.child(
            div()
                .pt(px(2.))
                .text_size(px(12.))
                .text_color(faint(theme))
                .child(hint),
        );
    }
    row.into_any_element()
}

/// One usage bar line (TS `BarRow`): mono label + faint value over a 4px
/// heat bar. `low_is_good = false` (the battery) flips the COLOR scale, not
/// the fill — a full bar stays green-side.
fn bar_row(
    theme: &Theme,
    id: &'static str,
    label: String,
    value: String,
    percent: Option<f64>,
    low_is_good: bool,
) -> AnyElement {
    let clamped = percent.map(|p| p.clamp(0.0, 100.0));
    let mut bar = div()
        .mt(px(4.))
        .h(px(4.))
        .overflow_hidden()
        .rounded_full()
        .bg(white().opacity(0.08));
    if let Some(p) = clamped {
        bar = bar.child(
            div()
                .h_full()
                .rounded_full()
                .w(relative(p as f32 / 100.0))
                .bg(heat_color(if low_is_good { p } else { 100.0 - p })),
        );
    }
    div()
        .id(id)
        .px(px(16.))
        .py(px(6.))
        .child(
            div()
                .flex()
                .items_baseline()
                .justify_between()
                .gap(px(12.))
                .child(
                    div()
                        .flex_none()
                        .font_family(theme.mono_font_family.clone())
                        .text_size(px(12.))
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
                .child(
                    div()
                        .flex_none()
                        .truncate()
                        .text_size(px(12.))
                        .text_color(faint(theme))
                        .child(value),
                ),
        )
        .child(bar)
        .into_any_element()
}

/// One grouped block of rows; `divided` draws the top rule between sections
/// (TS `ConfigBlock`).
fn config_block(
    theme: &Theme,
    title: &str,
    divided: bool,
    rows: Vec<AnyElement>,
) -> AnyElement {
    div()
        .when(divided, |this| {
            this.mt(px(8.))
                .border_t_1()
                .border_color(theme.border)
        })
        .child(
            div()
                .px(px(16.))
                .pb(px(4.))
                .pt(px(12.))
                .text_size(px(12.))
                .text_color(faint(theme))
                .child(title.to_uppercase()),
        )
        .children(rows)
        .into_any_element()
}

/// Used-volume percentage — 0 rather than NaN for a zero-total volume
/// (TS `usedPct`).
fn used_pct(v: &SystemStatsVolume) -> f64 {
    if v.total_bytes > 0 {
        v.used_bytes as f64 / v.total_bytes as f64 * 100.0
    } else {
        0.0
    }
}

// ---------------------------------------------------------------------------
// The dashboard body
// ---------------------------------------------------------------------------

fn render_stats(theme: &Theme, stats: &SystemStatsResponse) -> AnyElement {
    let battery = &stats.battery;
    let power = &stats.power;
    let net = &stats.network;
    let m = &stats.memory;

    let batt_status = battery_status(battery.status.as_deref());
    let power_source = match battery.power_source {
        None => PLACEHOLDER.to_string(),
        Some(SystemStatsPowerSource::Ac) => t!("system.powerAc").into_owned(),
        Some(SystemStatsPowerSource::Battery) => t!("system.powerBattery").into_owned(),
    };

    // ---- Host ----
    let host = config_block(
        theme,
        &t!("system.host"),
        false,
        vec![
            config_row(theme, &t!("system.hostname"), stats.host.hostname.clone(), None),
            config_row(
                theme,
                &t!("system.os"),
                stats
                    .host
                    .os_version
                    .clone()
                    .unwrap_or_else(|| PLACEHOLDER.to_string()),
                None,
            ),
            config_row(
                theme,
                &t!("system.model"),
                stats
                    .host
                    .model
                    .clone()
                    .unwrap_or_else(|| PLACEHOLDER.to_string()),
                None,
            ),
            config_row(
                theme,
                &t!("system.chip"),
                stats
                    .host
                    .chip
                    .clone()
                    .unwrap_or_else(|| PLACEHOLDER.to_string()),
                None,
            ),
            config_row(theme, &t!("system.uptime"), fmt_uptime(stats.uptime_s), None),
        ],
    );

    // ---- Battery ----
    let mut battery_rows: Vec<AnyElement> = Vec::new();
    if battery.present {
        // `${percent}% · ${status}`.trim() — the middot drops with no status.
        let value = match (battery.percent, &batt_status) {
            (None, _) => PLACEHOLDER.to_string(),
            (Some(pct), Some(status)) => format!("{pct}% · {status}"),
            (Some(pct), None) => format!("{pct}%"),
        };
        battery_rows.push(bar_row(
            theme,
            "battery-bar",
            t!("system.battery").into_owned(),
            value,
            battery.percent.map(|p| p as f64),
            false,
        ));
        battery_rows.push(config_row(
            theme,
            &t!("system.powerSource"),
            match &batt_status {
                Some(status) => format!("{power_source} · {status}"),
                None => power_source,
            },
            None,
        ));
        battery_rows.push(config_row(
            theme,
            &t!("system.remaining"),
            fmt_minutes(battery.remaining_min),
            None,
        ));
    } else {
        // present: false is a desktop machine, not an error.
        battery_rows.push(config_row(
            theme,
            &t!("system.battery"),
            t!("system.batteryNone").into_owned(),
            None,
        ));
    }
    battery_rows.push(config_row(
        theme,
        &t!("system.sleep"),
        match power.prevent_sleep {
            None => PLACEHOLDER.to_string(),
            Some(true) => t!("system.sleepHeld").into_owned(),
            Some(false) => t!("system.sleepOk").into_owned(),
        },
        // "why won't this machine sleep" — name the holders while the
        // assertion is held.
        (power.prevent_sleep == Some(true) && !power.sleep_holders.is_empty()).then(|| {
            fill(
                &t!("system.sleepHolders"),
                &[("names", &power.sleep_holders.join(", "))],
            )
        }),
    ));
    battery_rows.push(config_row(
        theme,
        &t!("system.thermal"),
        match power.cpu_speed_limit_pct {
            None => PLACEHOLDER.to_string(),
            Some(pct) if pct >= 100 => t!("system.thermalOk").into_owned(),
            Some(pct) => fill(
                &t!("system.thermalLimited"),
                &[("pct", &format!("{pct}"))],
            ),
        },
        None,
    ));
    let battery_block = config_block(theme, &t!("system.battery"), true, battery_rows);

    // ---- Performance ----
    let mut perf_rows: Vec<AnyElement> = vec![
        bar_row(
            theme,
            "cpu-bar",
            format!("CPU · {}C", stats.cpu.cores),
            match stats.cpu.usage_pct {
                None => PENDING.to_string(),
                Some(pct) => format!("{pct}%"),
            },
            stats.cpu.usage_pct,
            true,
        ),
        config_row(
            theme,
            &t!("system.load"),
            stats
                .cpu
                .load_avg
                .iter()
                .map(|v| format!("{v:.1}"))
                .collect::<Vec<_>>()
                .join(" / "),
            None,
        ),
    ];
    // Memory: usedBytes is a probe (vm_stat may be denied) — a null hides
    // the reading, not the machine's other numbers.
    perf_rows.push(bar_row(
        theme,
        "memory-bar",
        t!("system.memory").into_owned(),
        match m.used_bytes {
            None => PLACEHOLDER.to_string(),
            Some(used) => format!("{} / {}", fmt_bytes(used), fmt_bytes(m.total_bytes)),
        },
        m.used_bytes
            .map(|used| used as f64 / m.total_bytes as f64 * 100.0),
        true,
    ));
    if let (Some(swap_total), Some(swap_used)) = (m.swap_total_bytes, m.swap_used_bytes) {
        perf_rows.push(bar_row(
            theme,
            "swap-bar",
            t!("system.swap").into_owned(),
            format!("{} / {}", fmt_bytes(swap_used), fmt_bytes(swap_total)),
            Some(swap_used as f64 / swap_total as f64 * 100.0),
            true,
        ));
    }
    if let Some(root) = &stats.storage.root {
        perf_rows.push(bar_row(
            theme,
            "disk-bar",
            t!("system.storage").into_owned(),
            format!("{} / {}", fmt_bytes(root.used_bytes), fmt_bytes(root.total_bytes)),
            Some(used_pct(root)),
            true,
        ));
    }
    // `home` is null when it IS the root volume (single-boot-volume norm) —
    // one disk row is the whole story then.
    if let Some(home) = &stats.storage.home {
        perf_rows.push(bar_row(
            theme,
            "home-bar",
            t!("system.storageHome").into_owned(),
            format!("{} / {}", fmt_bytes(home.used_bytes), fmt_bytes(home.total_bytes)),
            Some(used_pct(home)),
            true,
        ));
    }
    let perf_block = config_block(theme, &t!("system.performance"), true, perf_rows);

    // ---- Network ----
    let net_block = config_block(
        theme,
        &t!("system.network"),
        true,
        vec![
            config_row(
                theme,
                &t!("system.ssid"),
                net.ssid.clone().unwrap_or_else(|| PLACEHOLDER.to_string()),
                None,
            ),
            config_row(
                theme,
                &t!("system.addresses"),
                if net.addresses.is_empty() {
                    PLACEHOLDER.to_string()
                } else {
                    net.addresses.join(", ")
                },
                None,
            ),
            config_row(
                theme,
                &t!("system.speed"),
                format!(
                    "↓ {} · ↑ {}",
                    fmt_rate(net.rx_bytes_per_s),
                    fmt_rate(net.tx_bytes_per_s)
                ),
                None,
            ),
        ],
    );

    // ---- Processes ----
    let hub = &stats.processes.hub;
    let hub_hint = format!(
        "{}{} · pid {}",
        fmt_bytes(hub.rss_bytes),
        hub.cpu_pct
            .map(|pct| format!(" · CPU {pct}%"))
            .unwrap_or_default(),
        hub.pid
    );
    let mut proc_rows = vec![config_row(
        theme,
        &t!("system.hubProc"),
        format!("v{} · {}", stats.hub.version, fmt_uptime(stats.hub.uptime_s)),
        Some(hub_hint),
    )];
    if stats.processes.bridges.is_empty() {
        proc_rows.push(config_row(
            theme,
            &t!("system.bridges"),
            t!("system.bridgesEmpty").into_owned(),
            None,
        ));
    } else {
        for b in &stats.processes.bridges {
            let label = match b.workspace.as_deref() {
                Some(ws) => workspace_leaf(ws).to_string(),
                None => b.id.clone(),
            };
            proc_rows.push(config_row(
                theme,
                &label,
                match b.rss_bytes {
                    None => PLACEHOLDER.to_string(),
                    Some(rss) => format!(
                        "{}{}",
                        fmt_bytes(rss),
                        b.cpu_pct
                            .map(|pct| format!(" · {pct}%"))
                            .unwrap_or_default()
                    ),
                },
                Some(format!(
                    "bridge · pid {}",
                    b.pid.map(|p| p.to_string()).unwrap_or_else(|| "?".to_string())
                )),
            ));
        }
    }
    let proc_block = config_block(theme, &t!("system.processes"), true, proc_rows);

    div()
        .flex()
        .flex_col()
        .pb(px(8.))
        .child(host)
        .child(battery_block)
        .child(perf_block)
        .child(net_block)
        .child(proc_block)
        .child(
            div()
                .px(px(16.))
                .pb(px(8.))
                .pt(px(12.))
                .text_size(px(12.))
                .text_color(faint(theme))
                .child(fill(
                    &t!("system.collected"),
                    &[("time", &fmt_stamp_utc(stats.collected_at))],
                )),
        )
        .into_any_element()
}

// ---------------------------------------------------------------------------
// The page frame (ConfigPageFrame + RestartNote ports)
// ---------------------------------------------------------------------------

/// The Effect Class outcome surface: every config page renders this frame,
/// so an effect-class or reset-card notice raised inside a section is
/// always on a banner somebody can see (TS ConfigPageFrame's rationale).
fn render_notice_banner(
    theme: &Theme,
    notice: &str,
    runtime: &ProtocolRuntime,
) -> AnyElement {
    let runtime = runtime.clone();
    div()
        .mx(px(16.))
        .mb(px(8.))
        .flex()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .rounded_lg()
        .bg(white().opacity(0.05))
        .px(px(12.))
        .py(px(8.))
        .child(
            div()
                .min_w_0()
                .flex_1()
                .text_size(px(13.))
                .text_color(theme.muted_foreground)
                .child(notice_text(notice)),
        )
        .child(
            Button::new("config-notice-dismiss")
                .ghost()
                .xsmall()
                .icon(IconName::CircleX)
                .tooltip("Dismiss")
                .on_click(move |_, _, _| runtime.send(UiCommand::DismissConfigNotice)),
        )
        .into_any_element()
}

/// The Backend Restart affordance, shown only while a needs-restart write
/// is pending (TS ZCodeConfigScreen `RestartNote`). Restarting cancels any
/// turn still running on that bridge — never automatic, and never offered
/// without a bridge to name (no active session → the hint instead).
fn render_restart_note(theme: &Theme, restart_available: bool, runtime: &ProtocolRuntime) -> AnyElement {
    let mut note = div()
        .mx(px(16.))
        .mt(px(8.))
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.title_bar)
        .px(px(16.))
        .py(px(12.))
        .child(
            div()
                .text_size(px(13.))
                .text_color(theme.muted_foreground)
                .child(t!("zconfig.restartPending").into_owned()),
        )
        .child(
            div()
                .pt(px(4.))
                .text_size(px(12.))
                .text_color(faint(theme))
                .child(t!("zconfig.restartWarning").into_owned()),
        );
    if restart_available {
        let runtime = runtime.clone();
        note = note.child(
            Button::new("restart-backend")
                .secondary()
                .small()
                .w_full()
                .mt(px(8.))
                .icon(IconName::RefreshCw)
                .label(t!("zconfig.restart").into_owned())
                .on_click(move |_, _, _| runtime.send(UiCommand::RestartBackend)),
        );
    } else {
        note = note.child(
            div()
                .pt(px(8.))
                .text_size(px(12.))
                .text_color(theme.warning_foreground)
                .child(t!("zconfig.restartNoInstance").into_owned()),
        );
    }
    note.into_any_element()
}

/// A capability-gap card: the route 404'd on this hub (TS `zconfig.
/// unsupported` texts, kept verbatim — the shared note; for THIS page the
/// gap is bridge < 0.62.0).
fn render_unsupported(theme: &Theme) -> AnyElement {
    div()
        .mx(px(16.))
        .mt(px(8.))
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.title_bar)
        .px(px(16.))
        .py(px(24.))
        .flex()
        .flex_col()
        .items_center()
        .child(
            div()
                .text_size(px(13.))
                .text_color(theme.muted_foreground)
                .child(t!("zconfig.unsupported").into_owned()),
        )
        .child(
            div()
                .pt(px(6.))
                .text_size(px(12.))
                .text_color(faint(theme))
                .child(t!("zconfig.unsupportedHint").into_owned()),
        )
        .into_any_element()
}

/// The first-load failure story (TS `zconfig.loadFailed` + the raw error).
fn render_error(theme: &Theme, error: &str) -> AnyElement {
    div()
        .mx(px(16.))
        .mt(px(8.))
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.title_bar)
        .px(px(16.))
        .py(px(24.))
        .flex()
        .flex_col()
        .items_center()
        .child(
            div()
                .text_size(px(13.))
                .text_color(theme.muted_foreground)
                .child(t!("zconfig.loadFailed").into_owned()),
        )
        .child(
            div()
                .pt(px(6.))
                .text_size(px(12.))
                .text_color(faint(theme))
                .child(error.to_string()),
        )
        .into_any_element()
}

/// The System Status page. `restart_available` is whether an active tab
/// exists (its bridge is what a restart addresses); the toast strip keeps
/// the workbench's notify-toast contract alive while the page covers the
/// chat surfaces.
pub fn render_system_status_page(
    page: &SystemStatusPageView,
    notice: Option<&str>,
    pending_restart: bool,
    restart_available: bool,
    toast: Option<&str>,
    runtime: &ProtocolRuntime,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let back_runtime = runtime.clone();
    let refresh_runtime = runtime.clone();

    let header = div()
        .flex()
        .items_center()
        .gap(px(4.))
        .px(px(8.))
        .pb(px(8.))
        .child(
            Button::new("system-back")
                .ghost()
                .xsmall()
                .icon(IconName::ArrowLeft)
                .tooltip(t!("common.back").into_owned())
                .on_click(move |_, _, _| back_runtime.send(UiCommand::CloseSystemStatus)),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .text_size(px(14.))
                .text_color(theme.foreground)
                .child(t!("system.title").into_owned()),
        )
        .child(
            Button::new("system-refresh")
                .ghost()
                .xsmall()
                .icon(IconName::RefreshCw)
                .tooltip(t!("quota.refresh").into_owned())
                // The spinner: a read is in flight (TS animate-spin).
                .loading(page.loading)
                .on_click(move |_, _, _| refresh_runtime.send(UiCommand::RefreshSystemStatus)),
        );

    // ConfigPageFrame state precedence: unsupported > error >
    // loading-without-payload > content.
    let body = if !page.supported {
        render_unsupported(&theme)
    } else if let Some(error) = &page.error {
        render_error(&theme, error)
    } else if page.loading && page.stats.is_none() {
        div()
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(theme.muted_foreground)
                    .child(t!("zconfig.loading").into_owned()),
            )
            .into_any_element()
    } else {
        match &page.stats {
            Some(stats) => div()
                .id("system-status-body")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(render_stats(&theme, stats))
                .into_any_element(),
            // A quiet no-data moment (neither loading nor failed) — TS
            // renders an empty body here; keep the page shell.
            None => div().flex_1().min_h_0().into_any_element(),
        }
    };

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .pt(px(8.))
        .child(header)
        .when_some(notice, |this, notice| {
            this.child(render_notice_banner(&theme, notice, runtime))
        })
        .when(pending_restart, |this| {
            this.child(render_restart_note(&theme, restart_available, runtime))
        })
        .when_some(toast, |this, text| {
            this.child(render_toast_strip(runtime, text, &theme))
        })
        .child(body)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    // Explicit imports, NOT `use super::*`: the module-level kit glob
    // re-exports GPUI's `test` macro, which would shadow Rust's built-in
    // #[test] (the kit's own doc note).
    use super::{
        battery_status, fmt_bytes, fmt_minutes, fmt_rate, fmt_stamp_utc, fmt_uptime, heat_rgb,
        notice_text, workspace_leaf,
    };

    #[test]
    fn fmt_bytes_matches_the_ts_shapes() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(87), "87 B");
        assert_eq!(fmt_bytes(1536), "1.5 KB");
        // >= 100 of a unit rounds to an integer.
        assert_eq!(fmt_bytes(234 * 1024 * 1024), "234 MB");
        // One decimal carries below 100, whole numbers drop the ".0".
        assert_eq!(fmt_bytes(64 * 1024_u64.pow(3) + 500 * 1024 * 1024), "64.5 GB");
        assert_eq!(fmt_bytes(2 * 1024), "2 KB");
        // Saturates at TB rather than dividing into PB.
        assert_eq!(fmt_bytes(3 * 1024_u64.pow(4)), "3 TB");
    }

    #[test]
    fn fmt_rate_pending_until_a_baseline_lands() {
        assert_eq!(fmt_rate(None), "…");
        assert_eq!(fmt_rate(Some(2048)), "2 KB/s");
    }

    #[test]
    fn fmt_uptime_buckets_like_the_ts_page() {
        assert_eq!(fmt_uptime(59), "59s");
        assert_eq!(fmt_uptime(60), "1m");
        assert_eq!(fmt_uptime(3599), "59m");
        assert_eq!(fmt_uptime(3600), "1h 0m");
        assert_eq!(fmt_uptime(86399), "23h 59m");
        assert_eq!(fmt_uptime(86400), "1d 0h");
        assert_eq!(fmt_uptime(2 * 86400 + 5 * 3600), "2d 5h");
    }

    #[test]
    fn fmt_minutes_placeholder_and_buckets() {
        assert_eq!(fmt_minutes(None), "—");
        assert_eq!(fmt_minutes(Some(59)), "59m");
        assert_eq!(fmt_minutes(Some(60)), "1h 0m");
        assert_eq!(fmt_minutes(Some(125)), "2h 5m");
    }

    #[test]
    fn heat_scale_anchors_green_yellow_red() {
        assert_eq!(heat_rgb(0.0), (34, 197, 94));
        assert_eq!(heat_rgb(50.0), (234, 179, 8));
        assert_eq!(heat_rgb(100.0), (239, 68, 68));
        // Clamped outside [0, 100].
        assert_eq!(heat_rgb(-10.0), (34, 197, 94));
        assert_eq!(heat_rgb(140.0), (239, 68, 68));
    }

    #[test]
    fn stamp_renders_utc_month_day_hour_minute() {
        assert_eq!(fmt_stamp_utc(0), "01-01 00:00 UTC");
        // 2023-11-14 22:13:20 UTC.
        assert_eq!(fmt_stamp_utc(1_700_000_000_000), "11-14 22:13 UTC");
    }

    #[test]
    fn workspace_leaf_takes_the_last_non_empty_segment() {
        assert_eq!(
            workspace_leaf("/Users/william/Develop/tools/zcode-acp-app"),
            "zcode-acp-app"
        );
        assert_eq!(workspace_leaf("trailing/slash/"), "slash");
        assert_eq!(workspace_leaf("relative"), "relative");
    }

    #[test]
    fn battery_status_maps_pmset_wording() {
        assert_eq!(
            battery_status(Some("charging")).as_deref(),
            Some("charging")
        );
        assert_eq!(
            battery_status(Some("discharging")).as_deref(),
            Some("on battery")
        );
        assert_eq!(battery_status(Some("charged")).as_deref(), Some("charged"));
        // An unrecognized pmset wording passes through verbatim.
        assert_eq!(battery_status(Some("backup")).as_deref(), Some("backup"));
        assert_eq!(battery_status(None), None);
    }

    #[test]
    fn notices_resolve_the_catalog_keys_and_pass_text_through() {
        assert_eq!(
            notice_text("notice.configNeedsRestart"),
            "Saved — restart the backend to apply it"
        );
        assert_eq!(
            notice_text("notice.configRestarted"),
            "Backend restarted"
        );
        assert_eq!(notice_text("plain failure text"), "plain failure text");
    }
}
