//! The Usage configuration page (UsagePage.tsx port, requirements §4 "M3
//! configuration pages"): two tabs over the Settings API's usage reads —
//! Platform (the account-level monitor data the bridge aggregates from the
//! BigModel/Z.AI monitor API: 365-day activity heatmap, streaks, credits
//! with cache-hit trends, per-model/per-tool usage) and Local (what this
//! machine's agent database recorded, ADR-0027). The two answer different
//! questions, so they stay visually distinct but one entry deep.
//!
//! Frame semantics (`src/components/config/ConfigPage.tsx`): back header, the
//! notice banner every configuration page renders (an Effect Class outcome
//! raised inside a section must stay visible — CONTEXT.md "Effect Class"),
//! the whole-page "hub too old" card, and the first-load spinner that never
//! flashes an empty state over data that has not arrived yet.
//!
//! Backend Restart (CONTEXT.md): the TS app renders the restart affordance
//! on the configuration ENTRY list (ZCodeConfigScreen's `RestartNote`); the
//! workbench has no entry list yet, so this page carries it — same rules:
//! absent unless a needs-restart write is armed, never automatic (it cancels
//! in-flight turns), and it names the active tab's bridge or explains that
//! it cannot.

use gpui::prelude::FluentBuilder as _;
use gpui::{relative, AnyElement, FontWeight, Hsla, SharedString, div, px};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{IconName, Sizable as _, Theme};
use gpui_kit::*;
use rust_i18n::t;

use crate::i18n::fill;
use crate::runtime::{ProtocolRuntime, UiCommand, UsagePageView, UsageTab};

/// A range pill's label (UsagePage.tsx `RANGES` values via the shared
/// catalog — `zconfig.range*`); the keys stay the wire vocabulary.
fn range_label(range: &str) -> std::borrow::Cow<'static, str> {
    match range {
        "today" => t!("zconfig.rangeToday"),
        "7d" => t!("zconfig.range7d"),
        "30d" => t!("zconfig.range30d"),
        "all" => t!("zconfig.rangeAll"),
        other => std::borrow::Cow::Owned(other.to_string()),
    }
}

/// The page's range vocabularies (UsagePage.tsx `RANGES` /
/// `PLATFORM_RANGES`) — the pill keys in render order; labels resolve
/// through the shared catalog (`range_label`). The active one is whichever
/// the store last ASKED for (a response landing out of order is dropped
/// driver-side, so the store record is the pill authority).
const PLATFORM_RANGES: [&str; 3] = ["today", "7d", "30d"];
const LOCAL_RANGES: [&str; 3] = ["7d", "30d", "all"];

/// The heatmap's five square colors (UsagePage.tsx `LEVEL_BG`): level 0 is a
/// recorded zero (earns a square), a missing day keeps the week's shape with
/// no square at all.
fn heatmap_level_color(level: u64) -> Hsla {
    match level.min(4) {
        0 => white_alpha(0.06),
        1 => emerald_500(0.25),
        2 => emerald_500(0.45),
        3 => emerald_500(0.70),
        // The accent stepped down to emerald-600 (batch T), so the hottest
        // square rides 500 at full strength — bright enough to top the
        // ladder without reintroducing the glare.
        _ => emerald_500(0.95),
    }
}

fn white_alpha(a: f32) -> Hsla {
    Hsla {
        h: 0.0,
        s: 0.0,
        l: 1.0,
        a,
    }
}

fn emerald_500(a: f32) -> Hsla {
    Hsla {
        h: 160.0 / 360.0,
        s: 0.84,
        l: 0.40,
        a,
    }
}

fn sky_400(a: f32) -> Hsla {
    Hsla {
        h: 199.0 / 360.0,
        s: 0.93,
        l: 0.60,
        a,
    }
}

fn amber_300() -> Hsla {
    Hsla {
        h: 46.0 / 360.0,
        s: 0.96,
        l: 0.64,
        a: 1.0,
    }
}

/// `notice.*` i18n keys resolved through the shared catalog (`t!` falls
/// back to its input — plain-text notify messages pass through, the TS
/// `startsWith("notice.")` check generalized).
fn notice_text(notice: &str) -> String {
    t!(notice).into_owned()
}

/// Compact token count (`fmtTokens`): `1.2M`, `34k`, `870`.
fn fmt_tokens(n: f64) -> String {
    if n >= 1_000_000.0 {
        format!("{:.1}M", n / 1_000_000.0)
    } else if n >= 1_000.0 {
        if n >= 10_000.0 {
            format!("{:.0}k", n / 1_000.0)
        } else {
            format!("{:.1}k", n / 1_000.0)
        }
    } else if n.fract() == 0.0 {
        // JS String(number) prints integral values bare.
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// `1h 23m` / `23m` — compact on purpose; the row is dense already.
fn fmt_duration(ms: u64) -> String {
    let hours = ms / 3_600_000;
    let minutes = ((ms % 3_600_000) as f64 / 60_000.0).round() as u64;
    if hours == 0 {
        format!("{minutes}m")
    } else {
        format!("{hours}h {minutes}m")
    }
}

/// TS `slice(5)` on the peak-day date: `2026-10-15` -> `10-15`.
fn date_tail(date: &str) -> String {
    date.chars().skip(5).collect()
}

// ---------------------------------------------------------------------------
// Frame + page
// ---------------------------------------------------------------------------

/// The whole page: frame (header, notice banner, restart note, unsupported
/// card, first-load spinner) around the two tabs' content.
pub fn render_usage_page(
    view: &UsagePageView,
    notice: Option<&str>,
    pending_restart: bool,
    restart_available: bool,
    runtime: &ProtocolRuntime,
    theme: &Theme,
) -> AnyElement {
    let mut page = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .bg(theme.background);

    // Header: back arrow + title (ConfigPageFrame's row).
    let rt = runtime.clone();
    page = page.child(
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_1()
            .h_10()
            .px_2()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.title_bar)
            .child(
                Button::new("usage-back")
                    .ghost()
                    .small()
                    .icon(IconName::ArrowLeft)
                    .tooltip("Back to the workbench")
                    .on_click(move |_, _, _| rt.send(UiCommand::CloseUsage)),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.foreground)
                    .child(t!("zconfig.usage").into_owned()),
            ),
    );

    // The notice banner every config page renders (an Effect Class outcome
    // must be visible wherever the user lands, ConfigPage.tsx:52).
    if let Some(text) = notice {
        let rt = runtime.clone();
        page = page.child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .justify_between()
                .gap_2()
                .mx_3()
                .mt_2()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(white_alpha(0.05))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(12.))
                        .text_color(theme.muted_foreground)
                        .child(notice_text(text)),
                )
                .child(
                    div()
                        .id("usage-notice-dismiss")
                        .flex_none()
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(theme.muted_foreground.opacity(0.7))
                        .child("✕")
                        .on_click(move |_, _, _| rt.send(UiCommand::DismissConfigNotice)),
                ),
        );
    }

    // The Backend Restart affordance (RestartNote): absent unless armed.
    if pending_restart {
        let rt = runtime.clone();
        let action: AnyElement = if restart_available {
            Button::new("usage-restart")
                .secondary()
                .small()
                .w_full()
                .mt_2()
                .icon(IconName::RefreshCw)
                .label(t!("zconfig.restart").into_owned())
                .on_click(move |_, _, _| rt.send(UiCommand::RestartBackend))
                .into_any_element()
        } else {
            // No tab open: no bridge to name — say so instead of failing on
            // tap (RestartNote's amber line).
            div()
                .mt_2()
                .text_size(px(10.))
                .text_color(amber_300())
                .child(t!("zconfig.restartNoInstance").into_owned())
                .into_any_element()
        };
        page = page.child(
            div()
                .flex()
                .flex_none()
                .flex_col()
                .mx_3()
                .mt_2()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(theme.title_bar)
                .border_1()
                .border_color(theme.border)
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.muted_foreground)
                        .child(t!("zconfig.restartPending").into_owned()),
                )
                .child(
                    div()
                        .mt_1()
                        .text_size(px(10.))
                        .text_color(theme.muted_foreground.opacity(0.7))
                        .child(t!("zconfig.restartWarning").into_owned()),
                )
                .child(action),
        );
    }

    // The frame's three states (ConfigPageFrame): unsupported (an older hub),
    // first-load spinner, or the page content.
    let body: AnyElement = if view.supported == Some(false) {
        // A capability gap, not a failure: the hub predates the settings API.
        div()
            .flex()
            .flex_col()
            .flex_none()
            .items_center()
            .mx_3()
            .mt_2()
            .px_4()
            .py_6()
            .rounded_lg()
            .bg(theme.title_bar)
            .border_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(theme.muted_foreground)
                    .child(t!("zconfig.unsupported").into_owned()),
            )
            .child(
                div()
                    .mt_1()
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground.opacity(0.7))
                    .child(t!("zconfig.unsupportedHint").into_owned()),
            )
            .into_any_element()
    } else {
        // `hasLoaded` (UsagePage.tsx): the platform tab is loaded once its
        // payload landed OR the route retired; the local tab once its payload
        // landed (the /settings/all snapshot seeds it, so this is usually
        // immediate). The spinner shows only while a load is in flight AND
        // nothing has been shown yet — a refresh keeps the content on screen.
        let has_loaded = match view.tab {
            UsageTab::Platform => view.platform.is_some() || !view.platform_supported,
            UsageTab::Local => view.local.is_some(),
        };
        if view.loading && !has_loaded {
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme.muted_foreground.opacity(0.7))
                        .child(t!("zconfig.loading").into_owned()),
                )
                .into_any_element()
        } else {
            // Tab pills (page-level) + the active tab's content, in the
            // scroll area.
            let mut content = div().flex().flex_col();
            content = content.child(render_tab_pills(view, runtime, theme));
            content = match view.tab {
                UsageTab::Platform => content.child(render_platform_tab(view, runtime, theme)),
                UsageTab::Local => content.child(render_local_tab(view, runtime, theme)),
            };
            div()
                .id("usage-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(content.pb_4())
                .into_any_element()
        }
    };
    page.child(body).into_any_element()
}

/// The Platform / This device tab pills (UsagePage.tsx:47).
fn render_tab_pills(view: &UsagePageView, runtime: &ProtocolRuntime, theme: &Theme) -> gpui::Div {
    let platform = view.tab == UsageTab::Platform;
    let rt = runtime.clone();
    let pill_platform = pill(
        SharedString::from("usage-tab-platform"),
        &t!("zconfig.usageTabPlatform"),
        platform,
        theme,
        move || rt.send(UiCommand::UsageTab { platform: true }),
    );
    let rt = runtime.clone();
    let pill_local = pill(
        SharedString::from("usage-tab-local"),
        &t!("zconfig.usageTabLocal"),
        !platform,
        theme,
        move || rt.send(UiCommand::UsageTab { platform: false }),
    );
    div()
        .flex()
        .gap_2()
        .px_4()
        .py_3()
        .flex_none()
        .child(pill_platform)
        .child(pill_local)
}

/// One segmented-control pill (active: bg + medium + ink; inactive: faint,
/// hover hint). `fire` sends the pill's command.
fn pill<F>(id: SharedString, label: &str, active: bool, theme: &Theme, fire: F) -> impl IntoElement
where
    F: Fn() + 'static,
{
    div()
        .id(id)
        .test_support()
        .flex()
        .flex_1()
        .items_center()
        .justify_center()
        .py(px(6.))
        .rounded_md()
        .text_size(px(13.))
        .cursor_pointer()
        .when(active, |this| {
            this.bg(white_alpha(0.10))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.foreground)
        })
        .when(!active, |this| {
            this.text_color(theme.muted_foreground.opacity(0.8))
                .hover(|h| h.bg(white_alpha(0.05)))
        })
        .child(label.to_string())
        .on_click(move |_, _, _| fire())
}

// ---------------------------------------------------------------------------
// Platform tab
// ---------------------------------------------------------------------------

fn render_platform_tab(
    view: &UsagePageView,
    runtime: &ProtocolRuntime,
    theme: &Theme,
) -> AnyElement {
    let mut col = div().flex().flex_col();

    // The range pills: a control over the upstream query (the 365-day
    // activity block ignores it; credits/model/tool honor it), so a tap
    // refetches. The store's last ask is the highlight authority.
    let active = PLATFORM_RANGES
        .iter()
        .any(|r| *r == view.platform_range)
        .then_some(view.platform_range.as_str())
        .unwrap_or("7d");
    let mut pills = div().flex().gap_2().px_4().pb_1().flex_none();
    for range in PLATFORM_RANGES {
        let rt = runtime.clone();
        pills = pills.child(pill(
            SharedString::from(format!("usage-platform-range-{range}")),
            &range_label(range),
            range == active,
            theme,
            move || {
                rt.send(UiCommand::UsageRange {
                    platform: true,
                    range: range.to_string(),
                })
            },
        ));
    }
    col = col.child(pills);

    if !view.platform_supported {
        // The bridge predates the route. Deliberately NOT the whole-page
        // "unsupported" state: every other section still works.
        return col
            .child(config_empty(
                &t!("zconfig.usagePlatformOldBridge"),
                theme,
            ))
            .into_any_element();
    }
    let Some(usage) = &view.platform else {
        // No payload yet and none in flight: the frame's spinner covered the
        // first load; a failed load stays blank (the TS page renders null —
        // its configError is never surfaced here).
        return col.into_any_element();
    };
    if let Some(error) = &view.platform_error {
        return col.child(config_empty(error, theme)).into_any_element();
    }
    if usage.kind != "success" {
        let text = match usage.kind.as_str() {
            "auth_error" => t!("zconfig.usagePlatformAuth").into_owned(),
            "rate_limited" => t!("zconfig.usagePlatformBusy").into_owned(),
            _ => t!("quota.unavailable").into_owned(),
        };
        return col.child(config_empty(&text, theme)).into_any_element();
    }

    // ---- activity (fixed trailing 365 days, range-independent) ----
    if let Some(activity) = &usage.activity {
        let summary = &activity.summary;
        let mut block = config_block(None, false, theme)
            .child(config_row(
                &t!("zconfig.usageTotal"),
                fmt_tokens(summary.total_tokens as f64),
                None,
                theme,
            ))
            .child(config_row(
                &t!("zconfig.usagePeakDay"),
                fmt_tokens(summary.peak_daily_tokens as f64),
                summary.peak_daily_tokens_date.as_deref().map(date_tail),
                theme,
            ))
            .child(config_row(
                &t!("zconfig.usageDuration"),
                fmt_duration(summary.total_usage_duration_ms),
                None,
                theme,
            ))
            .child(config_row(
                &t!("zconfig.usageStreak"),
                fill(
                    &t!("zconfig.usageStreakValue"),
                    &[
                        ("current", &summary.current_streak_days.to_string()),
                        ("longest", &summary.longest_streak_days.to_string()),
                    ],
                ),
                None,
                theme,
            ));
        if !activity.heatmap.weeks.is_empty() {
            block = config_block(Some(&t!("zconfig.usageActivity")), true, theme)
                .child(heatmap(&activity.heatmap))
                // The caption dates the grid: a sparse-looking year with a
                // recent start beats an unexplained wall of squares.
                .child(caption(
                    format!(
                        "{} ~ {}",
                        activity.heatmap.start_date.clone().unwrap_or_default(),
                        activity.heatmap.end_date.clone().unwrap_or_default()
                    ),
                    theme,
                ));
        }
        col = col.child(block);
    }

    // ---- credits summary (the model variant; the tool variant answers the
    //         same question less usefully) ----
    if let Some(model) = usage.detail.as_ref().and_then(|d| d.model.as_ref()) {
        let block = config_block(Some(&t!("zconfig.usageCredits")), true, theme)
            .child(metric_row(
                &t!("zconfig.usageCacheHit"),
                model
                    .cache_hit_rate
                    .map(|v| format!("{v:.1}%"))
                    .unwrap_or_else(|| "–".to_string()),
                model.cache_hit_rate_trend,
                theme,
            ))
            .child(metric_row(
                &t!("zconfig.usageTotalCredits"),
                fmt_tokens(model.total_credits),
                model.total_credits_trend,
                theme,
            ))
            .child(metric_row(
                &t!("zconfig.usageAvgCredits"),
                fmt_tokens(model.average_daily_credits),
                model.average_daily_credits_trend,
                theme,
            ));
        col = col.child(block);
    }

    // ---- per-day (or per-hour today) aggregate ----
    if let Some(series) = &usage.series {
        if !series.totals.is_empty() {
            let title = match series.granularity {
                protocol::settings::PlatformSeriesGranularity::Hour => {
                    t!("zconfig.usageByHour").into_owned()
                }
                protocol::settings::PlatformSeriesGranularity::Day => {
                    t!("zconfig.usageByDay").into_owned()
                }
            };
            let title = title.as_str();
            let caption_text = format!(
                "{} ~ {}",
                series.x_time.first().cloned().unwrap_or_default(),
                series.x_time.last().cloned().unwrap_or_default()
            );
            col = col.child(
                config_block(Some(title), true, theme)
                    .child(series_bars(&series.totals))
                    .child(caption(caption_text, theme)),
            );
        }
    }

    // ---- by model ----
    let models = usage.models.as_deref().unwrap_or_default();
    let model_total: u64 = models.iter().map(|m| m.total_tokens).sum();
    let mut block = config_block(Some(&t!("zconfig.usageByModel")), true, theme);
    if models.is_empty() {
        block = block.child(config_empty(&t!("zconfig.usageNoData"), theme));
    } else {
        for m in models {
            // Share of the window's tokens — models compared against each
            // other, like the local tab's bar.
            let share = if model_total > 0 {
                (m.total_tokens as f64 / model_total as f64).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let mut value = fmt_tokens(m.total_tokens as f64);
            if let Some(credits) = m.total_credits {
                value.push_str(&format!(" · {} cr", fmt_tokens(credits)));
            }
            block = block.child(model_row(&m.name, &value, share, theme));
        }
    }
    col = col.child(block);

    // ---- by tool ----
    if let Some(tools) = usage.tools.as_ref() {
        if !tools.is_empty() {
            let mut block = config_block(Some(&t!("zconfig.usageByTool")), true, theme);
            for tool in tools {
                let value = match tool.total_credits {
                    Some(credits) => {
                        format!("{} · {} cr", tool.total_usage_count, fmt_tokens(credits))
                    }
                    None => tool.total_usage_count.to_string(),
                };
                block = block.child(config_row(&tool.name, value, None, theme));
            }
            col = col.child(block);
        }
    }

    col.into_any_element()
}

/// The 365-day activity grid: one column per (Sunday-start) week. The strip
/// scrolls horizontally when the year outgrows the pane (overflow-x-auto).
fn heatmap(map: &protocol::settings::PlatformUsageHeatmap) -> impl IntoElement {
    let mut grid = div()
        .id("usage-heatmap")
        .flex()
        .gap(px(2.))
        .px_4()
        .pt_3()
        .pb_1()
        .overflow_x_scroll();
    for week in &map.weeks {
        let mut column = div().flex().flex_col().gap(px(2.)).flex_none();
        for day in &week.days {
            // Level 0 (a recorded zero) and a missing day both read as
            // "nothing", but only level 0 earns a square — missing days keep
            // the week's shape.
            let square = div().size(px(9.)).rounded_sm().flex_none();
            column = column.child(match day {
                Some(cell) => square.bg(heatmap_level_color(cell.level)),
                None => square,
            });
        }
        grid = grid.child(column);
    }
    grid
}

/// One credits metric with its period-over-period trend (MetricRow).
fn metric_row(label: &str, value: String, trend: Option<f64>, theme: &Theme) -> gpui::Div {
    // A trend renders only when finite and nonzero (TS Number.isFinite);
    // up is emerald, down is sky.
    let trend_el: Option<gpui::Div> = trend.and_then(|t| {
        if !t.is_finite() || t == 0.0 {
            return None;
        }
        let up = t > 0.0;
        Some(
            div()
                .pl(px(6.))
                .text_size(px(12.))
                .text_color(if up { emerald_500(0.80) } else { sky_400(0.80) })
                .child(format!("{} {:.1}%", if up { "↑" } else { "↓" }, t.abs())),
        )
    });
    div()
        .px_4()
        .py_2()
        .flex()
        .items_baseline()
        .justify_between()
        .gap_3()
        .child(
            div()
                .flex_none()
                .text_size(px(13.))
                .text_color(theme.muted_foreground)
                .child(label.to_string()),
        )
        .child(
            div()
                .flex()
                .flex_none()
                .items_baseline()
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme.foreground)
                        .child(value),
                )
                .when_some(trend_el, |this, t| this.child(t)),
        )
}

/// Compact bars over the selected range — max-height relative, not to scale
/// (SeriesBars: min 6% for a nonzero day, a 2% floor for zero).
fn series_bars(totals: &[u64]) -> gpui::Div {
    const HEIGHT: f32 = 64.0;
    let max = totals.iter().copied().max().unwrap_or(0) as f64;
    let mut row = div()
        .flex()
        .items_end()
        .gap(px(3.))
        .px_4()
        .pt_3()
        .h(px(HEIGHT));
    for value in totals {
        let height = if *value > 0 && max > 0.0 {
            ((*value as f64 / max) as f32 * HEIGHT).max(HEIGHT * 0.06)
        } else {
            HEIGHT * 0.02
        };
        row = row.child(
            div()
                .min_w(px(3.))
                .flex_1()
                .rounded_sm()
                .bg(white_alpha(0.25))
                .h(px(height)),
        );
    }
    row
}

// ---------------------------------------------------------------------------
// Local tab
// ---------------------------------------------------------------------------

fn render_local_tab(view: &UsagePageView, runtime: &ProtocolRuntime, theme: &Theme) -> AnyElement {
    let mut col = div().flex().flex_col();

    let active = LOCAL_RANGES
        .iter()
        .any(|r| *r == view.local_range)
        .then_some(view.local_range.as_str())
        .unwrap_or("7d");
    let mut pills = div().flex().gap_2().px_4().pb_1().flex_none();
    for range in LOCAL_RANGES {
        let rt = runtime.clone();
        pills = pills.child(pill(
            SharedString::from(format!("usage-local-range-{range}")),
            &range_label(range),
            range == active,
            theme,
            move || {
                rt.send(UiCommand::UsageRange {
                    platform: false,
                    range: range.to_string(),
                })
            },
        ));
    }
    col = col.child(pills);

    if view.local.as_ref().is_some_and(|u| !u.available) {
        // No agent database is a normal state on a machine that never ran
        // one, not an error — say so plainly rather than showing an empty
        // chart.
        return col
            .child(config_empty(&t!("zconfig.usageNoData"), theme))
            .into_any_element();
    }
    // TS renders these blocks with `usage?.… ?? 0` — a payload that has not
    // landed (or failed) paints zeroes, exactly like the TS page.
    let (total_tokens, request_count) = match &view.local {
        Some(u) => (u.summary.total_tokens, u.summary.request_count),
        None => (0, 0),
    };
    col = col.child(
        config_block(None, false, theme)
            .child(config_row(
                &t!("zconfig.usageTotal"),
                fmt_tokens(total_tokens as f64),
                None,
                theme,
            ))
            .child(config_row(
                &t!("zconfig.usageRequests"),
                request_count.to_string(),
                None,
                theme,
            )),
    );

    let models = view.local.as_ref().map(|u| u.models.as_slice()).unwrap_or(&[]);
    let mut block = config_block(Some(&t!("zconfig.usageByModel")), true, theme);
    if models.is_empty() {
        block = block.child(config_empty(&t!("zconfig.usageNoData"), theme));
    } else {
        for m in models {
            // The db column is nullable (older rows predate it), so a null
            // id renders as a placeholder rather than a blank line the user
            // cannot attribute.
            let name = m
                .model_id
                .clone()
                .unwrap_or_else(|| t!("zconfig.usageUnknownModel").into_owned());
            let mut value = fmt_tokens(m.total_tokens as f64);
            value.push_str(&format!(" · {:.0}%", m.share * 100.0));
            block = block.child(model_row(&name, &value, m.share.clamp(0.0, 1.0), theme));
        }
    }
    col = col.child(block);

    let daily = view.local.as_ref().map(|u| u.daily.as_slice()).unwrap_or(&[]);
    if !daily.is_empty() {
        let mut block = config_block(Some("By day"), true, theme);
        for day in daily {
            let models = day
                .models
                .iter()
                .map(|m| {
                    format!(
                        "{} {}",
                        m.model_id.clone().unwrap_or_default(),
                        fmt_tokens(m.total_tokens as f64)
                    )
                })
                .collect::<Vec<_>>()
                .join(" · ");
            block = block.child(
                div()
                    .flex()
                    .items_baseline()
                    .justify_between()
                    .gap_3()
                    .px_4()
                    .py_2()
                    .child(
                        div()
                            .flex_none()
                            .font_family(theme.mono_font_family.clone())
                            .text_size(px(12.))
                            .text_color(theme.muted_foreground.opacity(0.7))
                            .child(day.date.clone()),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(theme.muted_foreground)
                            .child(models),
                    ),
            );
        }
        col = col.child(block);
    }
    col.into_any_element()
}

// ---------------------------------------------------------------------------
// Shared row/block primitives (ConfigPage.tsx ports)
// ---------------------------------------------------------------------------

/// One grouped block of rows; `divided` draws the top rule the side panels
/// use between sections (ConfigBlock).
fn config_block(title: Option<&str>, divided: bool, theme: &Theme) -> gpui::Div {
    let mut block = div().flex().flex_col();
    if divided {
        block = block.mt_2().border_t_1().border_color(theme.border);
    }
    if let Some(title) = title {
        block = block.child(
            div()
                .px_4()
                .pt_3()
                .pb_1()
                .text_size(px(10.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.muted_foreground.opacity(0.7))
                .child(title.to_uppercase()),
        );
    }
    block
}

/// A read-only label/value row, the shape most configuration data takes
/// (ConfigRow).
fn config_row(label: &str, value: String, hint: Option<String>, theme: &Theme) -> gpui::Div {
    div()
        .px_4()
        .py_2()
        .flex()
        .flex_col()
        .child(
            div()
                .flex()
                .items_baseline()
                .justify_between()
                .gap_3()
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
        )
        .when_some(hint, |this, h| {
            this.child(
                div()
                    .pt_1()
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground.opacity(0.7))
                    .child(h),
            )
        })
}

/// Empty-state line for a section with nothing in it (ConfigEmpty).
fn config_empty(text: &str, theme: &Theme) -> gpui::Div {
    div()
        .px_4()
        .py_8()
        .text_center()
        .text_size(px(13.))
        .text_color(theme.muted_foreground.opacity(0.7))
        .child(text.to_string())
}

/// The faint caption line under a chart (heatmap / series date range).
fn caption(text: String, theme: &Theme) -> gpui::Div {
    div()
        .px_4()
        .pb_3()
        .pt_1()
        .text_size(px(12.))
        .text_color(theme.muted_foreground.opacity(0.7))
        .child(text)
}

/// One model row: name + value line, and the share bar beneath (a flat
/// neutral fill — models compared against each other, not against a limit).
fn model_row(name: &str, value: &str, share: f64, theme: &Theme) -> gpui::Div {
    div()
        .px_4()
        .py_2()
        .flex()
        .flex_col()
        .child(
            div()
                .flex()
                .items_baseline()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(13.))
                        .text_color(theme.muted_foreground)
                        .child(name.to_string()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px(13.))
                        .text_color(theme.foreground)
                        .child(value.to_string()),
                ),
        )
        .child(
            div()
                .mt_1()
                .h_1()
                .overflow_hidden()
                .rounded_full()
                .bg(white_alpha(0.08))
                .child(
                    div()
                        .h_full()
                        .rounded_full()
                        .bg(white_alpha(0.25))
                        .w(relative(share as f32)),
                ),
        )
}
