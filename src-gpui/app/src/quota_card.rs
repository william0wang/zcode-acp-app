//! The side panels' account quota card — the port of
//! `src/components/QuotaSection.tsx`, rendered by BOTH the settings drawer
//! (TS SettingsPanel) and the session panel (TS SessionPanel):
//!
//! - hub-level data (`/api/quota`, the plan-quota page's own read), never
//!   session-scoped — the two panels paint the same driver state, and a
//!   refresh from either (or from the plan page) repaints all three;
//! - one row implementation shared with the plan page
//!   (`quota_page::quota_bar_row`), so the surfaces cannot drift apart;
//! - the sections the hub reports, in the CLI card's order: GLM Coding Plan,
//!   Opencode Go, Ollama Cloud, plus the CLIProxy pool the server added
//!   after the TS card was written (bridge 0.66; the protocol decodes it,
//!   so hiding it would repeat the very gap this card closes);
//! - status prose for the non-success kinds, verbatim from the CLI card.
//!
//! Hidden entirely until the first read answers (TS `if (!quotaUnavailable
//! && !usageStats) return null`); the panels' open kicks that read (the TS
//! mount effect).

use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, div, px};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Sizable as _, Theme};
use gpui_kit::{assets::IconName as KitIconName, *};
use protocol::quota::AccountUsageStats;
use rust_i18n::t;

use crate::local_time::fmt_stamp;
use crate::quota_page::{faint, quota_bar_row};
use crate::runtime::{ProtocolRuntime, QuotaCardView, UiCommand};
use crate::ui_prefs::ui_text;

/// Status lines, verbatim from the CLI card's non-success prose (TS
/// `GLM_STATUS`).
fn glm_status(kind: &str) -> &'static str {
    match kind {
        "auth_error" => "🔒 Quota auth expired — re-login in the ZCode app",
        "rate_limited" => "⏳ Quota service busy, try again shortly",
        _ => "⚠ Quota info unavailable",
    }
}

/// TS `GO_STATUS`.
fn go_status(kind: &str) -> &'static str {
    match kind {
        "auth_error" => "auth expired — refresh your opencode.ai cookie",
        _ => "unavailable",
    }
}

/// TS `OC_STATUS` (the CLI card's Ollama Cloud prose).
fn oc_status(kind: &str) -> &'static str {
    match kind {
        "auth_error" => "auth expired — refresh your Ollama Cloud API key",
        _ => "unavailable",
    }
}

fn capitalise(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// TS `SectionLabel`: the uppercase tracking caption; `divided` draws the
/// top rule between provider sections.
fn section_label(theme: &Theme, title: String, divided: bool) -> AnyElement {
    div()
        .px(px(16.))
        .pt(px(12.))
        .pb(px(2.))
        .when(divided, |this| this.mt(px(8.)).border_t_1().border_color(theme.border))
        .text_size(ui_text(12.))
        .text_color(faint(theme))
        .child(title.to_uppercase())
        .into_any_element()
}

/// The non-success status line under a section label.
fn status_line(theme: &Theme, text: &str) -> AnyElement {
    div()
        .px(px(16.))
        .py(px(4.))
        .text_size(ui_text(13.))
        .text_color(faint(theme))
        .child(text.to_string())
        .into_any_element()
}

/// The provider sections (TS QuotaSection's success branch).
fn render_sections(theme: &Theme, usage: &AccountUsageStats) -> Vec<AnyElement> {
    let mut out = Vec::new();

    // GLM Coding Plan — always present (the plan this product is for).
    let glm_title = match usage.glm.level.as_deref().filter(|l| !l.is_empty()) {
        Some(level) => format!("GLM Coding Plan · {}", capitalise(level)),
        None => "GLM Coding Plan".to_string(),
    };
    out.push(section_label(theme, glm_title, false));
    let mut glm = div().id("quota-card-glm").test_support().flex().flex_col();
    if usage.glm.kind == "success" {
        for it in usage.glm.items.iter().flatten() {
            // CLI parity: only the MCP limit renders its absolute
            // used/total; every other row stays on `NN%`.
            let counts = match (it.used_count, it.total_count) {
                (Some(used), Some(total)) if it.key == "mcp" => Some((used, total)),
                _ => None,
            };
            glm = glm.child(quota_bar_row(
                theme,
                &it.label,
                it.used_percent,
                it.next_reset_time,
                counts,
            ));
        }
    } else {
        glm = glm.child(status_line(theme, glm_status(&usage.glm.kind)));
    }
    out.push(glm.into_any_element());

    if usage.opencode.kind != "not_configured" {
        out.push(section_label(theme, "Opencode Go".to_string(), true));
        let mut go = div().id("quota-card-go").test_support().flex().flex_col();
        if usage.opencode.kind == "success" {
            for w in usage.opencode.windows.iter().flatten() {
                go = go.child(quota_bar_row(
                    theme,
                    &w.label,
                    w.usage_percent,
                    Some(w.resets_at),
                    None,
                ));
            }
        } else {
            go = go.child(status_line(theme, go_status(&usage.opencode.kind)));
        }
        out.push(go.into_any_element());
    }

    if let Some(oc) = usage.ollama.as_ref().filter(|o| o.kind != "not_configured") {
        out.push(section_label(theme, "Ollama Cloud".to_string(), true));
        let mut ollama = div().id("quota-card-ollama").test_support().flex().flex_col();
        if oc.kind == "success" {
            for w in oc.windows.iter().flatten() {
                ollama = ollama.child(quota_bar_row(
                    theme,
                    &w.label,
                    w.usage_percent,
                    w.resets_at,
                    None,
                ));
            }
        } else {
            ollama = ollama.child(status_line(theme, oc_status(&oc.kind)));
        }
        out.push(ollama.into_any_element());
    }

    // The CLIProxy pool (bridge 0.66): the combined per-window rows — the
    // per-account breakdown stays with the CLI, like GLM's per-model detail.
    if let Some(cp) = usage.cliproxy.as_ref().filter(|c| c.kind != "not_configured") {
        out.push(section_label(theme, "CLIProxy Gemini".to_string(), true));
        let mut cliproxy = div().id("quota-card-cliproxy").test_support().flex().flex_col();
        if cp.kind == "success" {
            for w in cp.combined.iter().flatten() {
                cliproxy = cliproxy.child(quota_bar_row(
                    theme,
                    &w.window,
                    w.used_percent,
                    w.resets_at,
                    None,
                ));
            }
        } else {
            cliproxy = cliproxy.child(status_line(theme, go_status(&cp.kind)));
        }
        out.push(cliproxy.into_any_element());
    }

    out
}

/// The card, or `None` while nothing has answered yet (the TS early
/// return): header with the last-refresh stamp and the refresh control,
/// then the unavailable note or the provider sections.
pub(crate) fn render_quota_card(
    view: &QuotaCardView,
    runtime: &ProtocolRuntime,
    theme: &Theme,
) -> Option<AnyElement> {
    if !view.quota_unavailable && view.usage.is_none() {
        return None;
    }    let refresh_runtime = runtime.clone();
    let mut header = div()
        .mt(px(8.))
        .flex()
        .items_center()
        .justify_between()
        .border_t_1()
        .border_color(theme.border)
        .px(px(16.))
        .pt(px(12.))
        .pb(px(2.))
        .child(
            div()
                .text_size(ui_text(12.))
                .text_color(faint(theme))
                .child(t!("quota.title").to_uppercase()),
        );
    let mut trailing = div().flex().items_center().gap(px(6.));
    if let Some(at) = view.usage_stats_at {
        trailing = trailing.child(
            div()
                .id("quota-card-updated")
                .test_support()
                .text_size(ui_text(10.))
                .font_family(theme.mono_font_family.clone())
                .text_color(faint(theme))
                .child(format!("{} {}", t!("quota.updated"), fmt_stamp(at))),
        );
    }
    trailing = trailing.child(
        Button::new("quota-card-refresh")
            .ghost()
            .xsmall()
            .icon(KitIconName::RefreshCw)
            .tooltip(t!("quota.refresh").into_owned())
            .on_click(move |_, _, _| refresh_runtime.send(UiCommand::RefreshQuota)),
    );
    header = header.child(trailing);

    let mut card = div()
        .id("quota-card")
        .test_support()
        .flex()
        .flex_col()
        .min_w_0()
        .child(header);
    if view.quota_unavailable {
        card = card.child(
            div()
                .id("quota-card-unavailable")
                .test_support()
                .px(px(16.))
                .py(px(4.))
                .text_size(ui_text(13.))
                .text_color(faint(theme))
                .child(t!("quota.unavailable").into_owned()),
        );
    } else if let Some(usage) = view.usage.as_ref() {
        card = card.children(render_sections(theme, usage));
    }
    Some(card.into_any_element())
}

/// The quota DRAWER's body (the rail gauge's surface): the mobile side
/// panels' card scaled up to a drawer — every provider section the hub
/// reports, the unavailable note, and the last-refresh footer. The drawer
/// frame (workbench `render_drawer`) owns the title and the refresh control.
pub(crate) fn render_quota_drawer_body(view: &QuotaCardView, theme: &Theme) -> AnyElement {
    let mut body = div()
        .id("quota-drawer-body")
        .test_support()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .overflow_y_scroll()
        .pb_4();
    if view.quota_unavailable {
        body = body.child(status_line(theme, &t!("quota.unavailable").into_owned()));
    } else if let Some(usage) = view.usage.as_ref() {
        body = body.children(render_sections(theme, usage));
    } else {
        // Nothing answered yet — the drawer's open kicked the read (the TS
        // mount effect); the frame's quiet stand-in until it lands.
        body = body.child(status_line(theme, &t!("zconfig.loading").into_owned()));
    }
    if let Some(at) = view.usage_stats_at {
        body = body.child(
            div()
                .id("quota-drawer-updated")
                .test_support()
                .px(px(16.))
                .pt(px(12.))
                .pb(px(8.))
                .text_size(ui_text(10.))
                .font_family(theme.mono_font_family.clone())
                .text_color(faint(theme))
                .child(format!("{} {}", t!("quota.updated"), fmt_stamp(at))),
        );
    }
    body.into_any_element()
}
