//! The plan-quota PAGE — the GLM coding-plan surface with its reset cards
//! (M3, requirements §4: "High-frequency configuration pages: … Quota
//! (Effect Class and Backend Restart semantics included)") — the desktop
//! port of `src/screens/config/QuotaPage.tsx`, semantics identical. It
//! opens from the settings drawer's "Plan quota" entry as a full page; the
//! rail gauge's drawer is the OTHER quota surface — the all-providers
//! display (`quota_card`), not this page.
//!
//! - GLM only: this screen is the coding-plan surface (the card windows and
//!   the quota they reset); other platforms' windows stay with the side
//!   panels' card, which the workbench does not render yet.
//! - Spending a card is the one irreversible action in the settings API, so
//!   the gesture always ends in a confirmation naming the card's window and
//!   expiry; below the usage threshold (90%, read on the card's OWN window —
//!   a 5-hour card against the 5-hour window, a weekly card against the
//!   weekly one) that confirmation is PRECEDED by a warning step.
//! - The provider + nonce of a spend are captured AT the confirmation, never
//!   re-read at send time: a provider switch mid-gesture must not burn
//!   another plan's card.
//! - Effect Class / Backend Restart presentation rides the shared config
//!   frame: the notice banner (spend outcomes, restart outcomes) and the
//!   restart note, armed only by a needs-restart write and never triggered
//!   automatically (CONTEXT.md).
//!
//! Strings come from the shared rust-i18n catalogs (`zconfig.*`,
//! `quota.*`, `common.*`, `notice.*`; requirements §2).

use gpui::prelude::FluentBuilder as _;
use gpui::{relative, AnyElement, Context, Hsla, div, px, rgb, white};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, Theme};
// The kit glob also brings gpui's self-alias `gpui` into scope — the
// established import shape (workbench.rs, system_status.rs).
use gpui_kit::*;
use protocol::settings::{ResetCardExpiry, ResetCardType};
use rust_i18n::t;

use crate::i18n::fill;
use crate::local_time::fmt_stamp;
use crate::runtime::{ProtocolRuntime, QuotaPageView, UiCommand};
use crate::ui_prefs::ui_text;
use crate::workbench::Workbench;

/// Below this window usage a spend asks for the extra warning step (TS
/// `SPEND_THRESHOLD_PCT`); the card still works — the note says what the
/// extra step is, not that the button is dead.
pub const SPEND_THRESHOLD_PCT: f64 = 90.0;

/// The spend-gesture view state (TS `SpendStage`): which window, and whether
/// the below-threshold warning has been answered. `warned: false` renders
/// the warning step; `true` renders the spend confirmation. At or above the
/// threshold a gesture starts already warned — its single confirmation is
/// the spend itself. Owned by the Workbench (TS page-local useState; it dies
/// with the page).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpendStage {
    pub reset_type: ResetCardType,
    pub warned: bool,
}

/// The used percentage of the ONE window a card type clears (TS
/// `windowUsedPercent`). A card's confirmation reads its OWN window — an
/// exhausted week must not let a 5-hour card skip its warning — and a window
/// the payload does not list reads as 0 (unknown usage keeps the warning).
pub fn window_used_percent(page: &QuotaPageView, reset_type: ResetCardType) -> f64 {
    page.glm_items
        .iter()
        .find(|it| it.key == reset_type.window_key())
        .map(|it| it.used_percent)
        .unwrap_or(0.0)
}

/// A team plan needs organization/project headers a headless process cannot
/// read, so its cards can never be spent from here (TS `isTeamPlan`).
fn is_team_plan(provider_id: &str) -> bool {
    provider_id.contains("-team-")
}

/// A short label for a coding-plan provider id (TS `providerLabel`): the
/// family and whether it is a personal or team plan, not the raw wire id.
fn provider_label(id: &str) -> String {
    let family = if id.contains(":zai-") { "Zai" } else { "Bigmodel" };
    if is_team_plan(id) {
        format!("{family} · {}", t!("zconfig.planTeam"))
    } else {
        format!("{family} · {}", t!("zconfig.planIndividual"))
    }
}

/// CLI parity (zcode-quota's `roundTenth`): cap the display at one decimal;
/// integral values print without the trailing `.0` (TS `fmtPercent`).
fn fmt_percent(pct: f64) -> String {
    let v = (pct * 10.0).round() / 10.0;
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v:.1}")
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

/// amber-300 (#FCD34D) — the warning pair's text color.
fn amber_text() -> Hsla {
    rgb(0xFCD34D).into()
}

/// amber-500 (#F59E0B) at `opacity` — the warning box/button fills.
fn amber_fill(opacity: f32) -> Hsla {
    let c: Hsla = rgb(0xF59E0B).into();
    c.opacity(opacity)
}

/// emerald-700 (#047857) at 85% — the card-row check glyph, following the
/// accent's step down (batch U; solid bright emeralds glare on the dark
/// canvas).
fn emerald_check() -> Hsla {
    let c: Hsla = rgb(0x047857).into();
    c.opacity(0.85)
}

/// TS text-faint — one step dimmer than text-dim (muted_foreground).
pub(crate) fn faint(theme: &Theme) -> gpui::Hsla {
    theme.muted_foreground.opacity(0.75)
}

/// The `notice.*` i18n keys the store surfaces, resolved through the shared
/// catalog (`t!` falls back to its input — plain failure text passes
/// through verbatim).
fn notice_text(notice: &str) -> String {
    t!(notice).into_owned()
}

// ---------------------------------------------------------------------------
// Row primitives (QuotaRow / ConfigBlock / ConfigEmpty ports)
// ---------------------------------------------------------------------------

/// One quota bar line (TS `QuotaRow`): mono label + `NN% · reset` (or the
/// MCP row's absolute `used / total`) over a 4px heat bar.
fn quota_row(theme: &Theme, item: &protocol::quota::QuotaItem) -> AnyElement {
    // CLI parity: only the MCP limit renders its absolute used/total; every
    // other row stays on `NN%`.
    let mcp_counts = match (&item.used_count, &item.total_count) {
        (Some(used), Some(total)) if item.key == "mcp" => Some((*used, *total)),
        _ => None,
    };
    quota_bar_row(
        theme,
        &item.label,
        item.used_percent,
        item.next_reset_time,
        mcp_counts,
    )
}

/// The one row implementation both quota surfaces paint (TS exports
/// `QuotaRow` for the same reason — the side-panel card and the plan page
/// cannot drift apart visually): `counts` renders `used / total` instead of
/// the percent.
pub(crate) fn quota_bar_row(
    theme: &Theme,
    label: &str,
    percent: f64,
    reset_ms: Option<u64>,
    counts: Option<(u64, u64)>,
) -> AnyElement {
    let clamped = percent.clamp(0.0, 100.0);
    let value = match counts {
        Some((used, total)) => format!("{used} / {total}"),
        None => format!("{}%", fmt_percent(percent)),
    };
    let line = match reset_ms {
        Some(reset) => format!("{value} · {}", fmt_stamp(reset)),
        None => value,
    };
    div()
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
                        .text_size(ui_text(12.))
                        .text_color(theme.muted_foreground)
                        .child(label.to_string()),
                )
                .child(
                    div()
                        .flex_none()
                        .truncate()
                        .font_family(theme.mono_font_family.clone())
                        .text_size(ui_text(12.))
                        .text_color(faint(theme))
                        .child(line),
                ),
        )
        .child(
            div()
                .mt(px(4.))
                .h(px(4.))
                .overflow_hidden()
                .rounded_full()
                .bg(white().opacity(0.08))
                .child(
                    div()
                        .h_full()
                        .rounded_full()
                        .w(relative(clamped as f32 / 100.0))
                        .bg(heat_color(clamped)),
                ),
        )
        .into_any_element()
}

/// One grouped block; `divided` draws the top rule (TS `ConfigBlock`).
fn config_block(theme: &Theme, title: Option<&str>, divided: bool) -> gpui::Div {
    div().when(divided, |this| {
        this.mt(px(8.)).border_t_1().border_color(theme.border)
    }).when_some(title, |this, title| {
        this.child(
            div()
                .px(px(16.))
                .pb(px(4.))
                .pt(px(12.))
                .text_size(ui_text(12.))
                .text_color(faint(theme))
                .child(title.to_uppercase()),
        )
    })
}

/// The empty-state line for a section with nothing in it (TS `ConfigEmpty`).
fn config_empty(theme: &Theme, text: &str) -> AnyElement {
    div()
        .px(px(16.))
        .py(px(32.))
        .text_center()
        .text_size(ui_text(13.))
        .text_color(faint(theme))
        .child(text.to_string())
        .into_any_element()
}

/// The Effect Class outcome surface: the config pages replace the chat
/// surface (the only other banner host), so the frame renders the banner
/// itself (TS ConfigPageFrame's rationale).
fn notice_banner(theme: &Theme, notice: &str, runtime: &ProtocolRuntime) -> AnyElement {
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
                .text_size(ui_text(13.))
                .text_color(theme.muted_foreground)
                .child(notice_text(notice)),
        )
        .child(
            Button::new("quota-notice-dismiss")
                .ghost()
                .xsmall()
                .icon(IconName::CircleX)
                .tooltip("Dismiss")
                .on_click(move |_, _, _| runtime.send(UiCommand::DismissConfigNotice)),
        )
        .into_any_element()
}

/// The Backend Restart affordance (TS `RestartNote`), shown only while a
/// needs-restart write is pending — restarting cancels in-flight turns, so
/// it is never automatic, and with no session attached there is no bridge to
/// name so the note says so.
fn restart_note(theme: &Theme, restart_available: bool, runtime: &ProtocolRuntime) -> AnyElement {
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
                .text_size(ui_text(13.))
                .text_color(theme.muted_foreground)
                .child(t!("zconfig.restartPending").into_owned()),
        )
        .child(
            div()
                .pt(px(4.))
                .text_size(ui_text(12.))
                .text_color(faint(theme))
                .child(t!("zconfig.restartWarning").into_owned()),
        );
    if restart_available {
        let runtime = runtime.clone();
        note = note.child(
            Button::new("quota-restart-backend")
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
                .text_size(ui_text(12.))
                .text_color(theme.warning_foreground)
                .child(t!("zconfig.restartNoInstance").into_owned()),
        );
    }
    note.into_any_element()
}

// ---------------------------------------------------------------------------
// The card lists
// ---------------------------------------------------------------------------

/// One window's card inventory (TS `CardList`): how many are left, when each
/// expires, and the spend control. A card carries an expiry and nothing
/// else, so this is all there is to show; soonest-expiring first, because
/// that is the order they should be spent in.
struct CardListArgs<'a> {
    title: String,
    cards: &'a [ResetCardExpiry],
    last_used: Option<u64>,
    reset_type: ResetCardType,
    /// The window reads at or above the threshold (drives the footnote).
    above_threshold: bool,
}

fn card_list(
    theme: &Theme,
    page: &QuotaPageView,
    confirm: &Option<SpendStage>,
    runtime: &ProtocolRuntime,
    wb: &gpui::Entity<Workbench>,
    args: CardListArgs<'_>,
) -> AnyElement {
    let CardListArgs {
        title,
        cards,
        last_used,
        reset_type,
        above_threshold,
    } = args;
    let busy = page.busy;
    // Soonest-expiring first — the order they should be spent in.
    let mut sorted: Vec<ResetCardExpiry> = cards.to_vec();
    sorted.sort_by_key(|c| c.expire_at);
    let next = sorted.first();

    let mut column = div().px(px(16.)).py(px(12.)).child(
        div()
            .flex()
            .items_baseline()
            .justify_between()
            .gap(px(12.))
            .child(
                div()
                    .flex_none()
                    .text_size(ui_text(13.))
                    .text_color(theme.muted_foreground)
                    .child(title.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .font_family(theme.mono_font_family.clone())
                    .text_size(ui_text(13.))
                    .text_color(theme.foreground)
                    .child(sorted.len().to_string()),
            ),
    );

    if sorted.is_empty() {
        column = column.child(
            div()
                .pt(px(4.))
                .text_size(ui_text(13.))
                .text_color(faint(theme))
                .child(t!("zconfig.resetNone").into_owned()),
        );
    } else {
        let mut list = div().pt(px(4.)).flex().flex_col();
        for (index, card) in sorted.iter().enumerate() {
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .py(px(2.))
                    .text_size(ui_text(13.))
                    .text_color(faint(theme))
                    .child(
                        div()
                            .flex_none()
                            .text_color(emerald_check())
                            .child("✓"),
                    )
                    .child(
                        div()
                            .font_family(theme.mono_font_family.clone())
                            .child(format!("{} {}", t!("zconfig.resetExpires"), fmt_stamp(card.expire_at))),
                    )
                    .when(index == 0, |this| {
                        this.child(
                            div()
                                .ml_auto()
                                .flex_none()
                                .text_size(ui_text(10.))
                                .text_color(faint(theme))
                                .child(t!("zconfig.resetNextUp").into_owned()),
                        )
                    }),
            );
        }
        column = column.child(list);
    }

    if let Some(used_at) = last_used {
        column = column.child(
            div()
                .pt(px(4.))
                .text_size(ui_text(12.))
                .text_color(faint(theme))
                .child(format!("{} {}", t!("zconfig.resetLastUsed"), fmt_stamp(used_at))),
        );
    }

    // A denied opportunity's countdown (TS `resetNextTryAt`) — shown only
    // while it lies in the future. Nothing arms it today: the page spends
    // straight off the status inventory, so the field stays a seam.
    if let Some(at) = page.next_try_at.filter(|at| *at > page.now_ms) {
        column = column.child(
            div()
                .pt(px(4.))
                .text_size(ui_text(12.))
                .text_color(amber_text())
                .child(format!("{} {}", t!("zconfig.resetRetryAt"), fmt_stamp(at))),
        );
    }

    if !sorted.is_empty() {
        let stage = confirm.filter(|c| c.reset_type == reset_type);
        column = column.child(match stage {
            // The spend confirmation names the card, not a generic "are you
            // sure": a spend is irreversible and the user commits to a
            // specific window and expiry.
            Some(SpendStage { warned: true, .. }) => {
                let expires = next
                    .map(|c| fmt_stamp(c.expire_at))
                    .unwrap_or_default();
                spend_confirm_box(
                    theme,
                    runtime,
                    wb,
                    reset_type,
                    fill(
                        &t!("zconfig.resetConfirm"),
                        &[
                            ("title", &title.to_lowercase()),
                            ("expires", &expires),
                        ],
                    ),
                    busy,
                )
            }
            // The pre-step below the threshold: the card works, but the
            // allowance is nowhere near spent — say so before the user
            // commits to anything.
            Some(_) => warn_box(theme, wb, reset_type),
            None => use_card_button(theme, wb, reset_type, busy),
        });
    }

    // Below this card's OWN threshold the card still works — the note says
    // what the extra step is, not that the button is dead.
    if !above_threshold {
        column = column.child(
            div()
                .pt(px(6.))
                .text_size(ui_text(12.))
                .text_color(faint(theme))
                .child(fill(
                    &t!("zconfig.resetBelowThreshold"),
                    &[("pct", &format!("{pct}", pct = SPEND_THRESHOLD_PCT as i64))],
                )),
        );
    }
    column.into_any_element()
}

/// The "Use a card" gesture starter (TS `askSpend`): a window at or above
/// the threshold starts already warned; below it the first step is the
/// warning.
fn use_card_button(
    theme: &Theme,
    wb: &gpui::Entity<Workbench>,
    reset_type: ResetCardType,
    busy: bool,
) -> AnyElement {
    let wb = wb.clone();
    div()
        .id(match reset_type {
            ResetCardType::FiveHour => "quota-use-five-hour",
            ResetCardType::Week => "quota-use-week",
        })
        .mt(px(8.))
        .w_full()
        .rounded_lg()
        .bg(white().opacity(0.08))
        .px(px(12.))
        .py(px(8.))
        .flex()
        .justify_center()
        .text_size(ui_text(13.))
        .text_color(theme.foreground)
        .when(busy, |this| this.opacity(0.4))
        .when(!busy, |this| {
            this.cursor_pointer()
                .hover(|h| h.bg(white().opacity(0.12)))
                .on_click(move |_, _, cx| {
                    wb.update(cx, |wb, cx| wb.quota_ask_spend(reset_type, cx));
                })
        })
        .child(t!("zconfig.resetUse").into_owned())
        .into_any_element()
}

/// The below-threshold warning step: Cancel, or Continue into the spend
/// confirmation.
fn warn_box(theme: &Theme, wb: &gpui::Entity<Workbench>, reset_type: ResetCardType) -> AnyElement {
    let cancel_wb = wb.clone();
    let continue_wb = wb.clone();
    div()
        .mt(px(8.))
        .rounded_lg()
        .bg(amber_fill(0.1))
        .px(px(12.))
        .py(px(10.))
        .child(
            div()
                .text_size(ui_text(13.))
                .text_color(amber_text())
                .child(fill(
                    &t!("zconfig.resetBelowWarn"),
                    &[("pct", &format!("{pct}", pct = SPEND_THRESHOLD_PCT as i64))],
                )),
        )
        .child(
            div()
                .mt(px(8.))
                .flex()
                .gap(px(8.))
                .child(
                    div()
                        .id(match reset_type {
                            ResetCardType::FiveHour => "quota-warn-cancel-5h",
                            ResetCardType::Week => "quota-warn-cancel-week",
                        })
                        .flex_1()
                        .rounded_lg()
                        .px(px(12.))
                        .py(px(8.))
                        .flex()
                        .justify_center()
                        .text_size(ui_text(13.))
                        .text_color(faint(theme))
                        .cursor_pointer()
                        .hover(|h| h.bg(white().opacity(0.06)))
                        .on_click(move |_, _, cx| {
                            cancel_wb.update(cx, |wb, cx| wb.quota_cancel_spend(cx));
                        })
                        .child(t!("common.cancel").into_owned()),
                )
                .child(
                    div()
                        .id(match reset_type {
                            ResetCardType::FiveHour => "quota-warn-continue-5h",
                            ResetCardType::Week => "quota-warn-continue-week",
                        })
                        .flex_1()
                        .rounded_lg()
                        .bg(amber_fill(0.2))
                        .px(px(12.))
                        .py(px(8.))
                        .flex()
                        .justify_center()
                        .text_size(ui_text(13.))
                        .text_color(amber_text())
                        .cursor_pointer()
                        .hover(|h| h.bg(amber_fill(0.3)))
                        .on_click(move |_, _, cx| {
                            continue_wb
                                .update(cx, |wb, cx| wb.quota_warn_continue(reset_type, cx));
                        })
                        .child(t!("zconfig.resetWarnContinue").into_owned()),
                ),
        )
        .into_any_element()
}

/// The spend confirmation: Cancel, or "Spend it" — both halves of the spend
/// (provider + nonce) are captured HERE, while the dialog is open and the
/// user cannot switch provider underneath.
fn spend_confirm_box(
    theme: &Theme,
    _runtime: &ProtocolRuntime,
    wb: &gpui::Entity<Workbench>,
    reset_type: ResetCardType,
    text: String,
    busy: bool,
) -> AnyElement {
    let cancel_wb = wb.clone();
    let spend_wb = wb.clone();
    div()
        .mt(px(8.))
        .rounded_lg()
        .bg(white().opacity(0.05))
        .px(px(12.))
        .py(px(10.))
        .child(
            div()
                .text_size(ui_text(13.))
                .text_color(theme.muted_foreground)
                .child(text),
        )
        .child(
            div()
                .mt(px(8.))
                .flex()
                .gap(px(8.))
                .child(
                    div()
                        .id(match reset_type {
                            ResetCardType::FiveHour => "quota-spend-cancel-5h",
                            ResetCardType::Week => "quota-spend-cancel-week",
                        })
                        .flex_1()
                        .rounded_lg()
                        .px(px(12.))
                        .py(px(8.))
                        .flex()
                        .justify_center()
                        .text_size(ui_text(13.))
                        .text_color(faint(theme))
                        .cursor_pointer()
                        .hover(|h| h.bg(white().opacity(0.06)))
                        .on_click(move |_, _, cx| {
                            cancel_wb.update(cx, |wb, cx| wb.quota_cancel_spend(cx));
                        })
                        .child(t!("common.cancel").into_owned()),
                )
                .child(
                    div()
                        .id(match reset_type {
                            ResetCardType::FiveHour => "quota-spend-commit-5h",
                            ResetCardType::Week => "quota-spend-commit-week",
                        })
                        .flex_1()
                        .rounded_lg()
                        .bg(white().opacity(0.1))
                        .px(px(12.))
                        .py(px(8.))
                        .flex()
                        .justify_center()
                        .text_size(ui_text(13.))
                        .text_color(theme.foreground)
                        .when(busy, |this| this.opacity(0.6))
                        .when(!busy, |this| {
                            this.cursor_pointer()
                                .hover(|h| h.bg(white().opacity(0.15)))
                                .on_click(move |_, _, cx| {
                                    spend_wb.update(cx, |wb, cx| {
                                        wb.quota_commit_spend(reset_type, cx)
                                    });
                                })
                        })
                        .child(if busy {
                            t!("zconfig.saving").into_owned()
                        } else {
                            t!("zconfig.resetSpend").into_owned()
                        }),
                ),
        )
        .into_any_element()
}

/// The provider picker (TS QuotaPage's chip row): the hub lists the provider
/// ids that COULD own cards regardless of the account, so the picker is the
/// norm. Team plans are offered too — selectable, then explained, because
/// hiding them would look like the plan is missing. Nothing is
/// pre-selected: guessing which plan the account holds would show an empty
/// inventory and read as "I have no cards".
fn provider_picker(
    theme: &Theme,
    page: &QuotaPageView,
    runtime: &ProtocolRuntime,
) -> AnyElement {
    let mut row = div().flex().flex_wrap().gap(px(8.)).px(px(16.)).pb(px(8.));
    for (index, id) in page.eligible.iter().enumerate() {
        let selected = page.provider_id.as_deref() == Some(id.as_str());
        let runtime = runtime.clone();
        let provider_id = id.clone();
        let busy = page.busy;
        row = row.child(
            div()
                .id(gpui::SharedString::from(format!("quota-provider-{index}")))
                .rounded_lg()
                .px(px(10.))
                .py(px(6.))
                .text_size(ui_text(12.))
                .when(selected, |this| {
                    this.bg(white().opacity(0.12)).text_color(theme.foreground)
                })
                .when(!selected, |this| {
                    this.text_color(faint(theme))
                        .cursor_pointer()
                        .hover(|h| h.bg(white().opacity(0.06)))
                        .when(!busy, |this| {
                            this.on_click(move |_, _, _| {
                                runtime.send(UiCommand::QuotaPickProvider {
                                    provider_id: provider_id.clone(),
                                });
                            })
                        })
                })
                .when(busy, |this| this.opacity(0.5))
                .child(provider_label(id)),
        );
    }
    row.into_any_element()
}

// ---------------------------------------------------------------------------
// The page (impl Workbench, like the Models page)
// ---------------------------------------------------------------------------

impl Workbench {
    /// The spend gesture's start (TS `askSpend`): the threshold is read on
    /// the card's OWN window, so the two lists can sit on different sides of
    /// it.
    pub(crate) fn quota_ask_spend(&mut self, reset_type: ResetCardType, cx: &mut Context<Self>) {
        let warned = self
            .snap
            .quota_page
            .as_ref()
            .map(|page| window_used_percent(page, reset_type) >= SPEND_THRESHOLD_PCT)
            .unwrap_or(false);
        self.quota_confirm = Some(SpendStage { reset_type, warned });
        cx.notify();
    }

    /// The warning step's Continue: the same gesture, now warned.
    pub(crate) fn quota_warn_continue(&mut self, reset_type: ResetCardType, cx: &mut Context<Self>) {
        self.quota_confirm = Some(SpendStage {
            reset_type,
            warned: true,
        });
        cx.notify();
    }

    pub(crate) fn quota_cancel_spend(&mut self, cx: &mut Context<Self>) {
        self.quota_confirm = None;
        cx.notify();
    }

    /// The confirmation's commit (TS `spend`): capture BOTH halves of the
    /// spend now, while the dialog is open and the user cannot switch
    /// provider underneath — spending is irreversible, so the wrong provider
    /// is the one failure mode worth engineering against.
    pub(crate) fn quota_commit_spend(&mut self, reset_type: ResetCardType, cx: &mut Context<Self>) {
        let Some(page) = self.snap.quota_page.as_ref() else {
            return;
        };
        let provider_id = page.provider_id.clone().unwrap_or_default();
        let nonce = page.nonce.clone().unwrap_or_default();
        // TS `spend`: an unspendable pair aborts WITHOUT closing the dialog.
        if provider_id.is_empty() || nonce.is_empty() {
            return;
        }
        self.quota_confirm = None;
        self.runtime.send(UiCommand::QuotaSpendCard {
            provider_id,
            nonce,
            reset_type,
        });
        cx.notify();
    }

    /// The plan-quota PAGE (the settings drawer's "Plan quota" entry): a
    /// full page of its own below the status bar (the TS ConfigPageFrame —
    /// back header, refresh, notice banner, restart note, then the
    /// unsupported / error / allowance+cards states). The rail gauge's
    /// all-providers drawer is the OTHER quota surface (`quota_card`).
    pub fn render_quota_page(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let page = self.snap.quota_page.clone().unwrap_or_default();
        let runtime = self.runtime.clone();
        let wb = cx.entity();

        // Header: back, title, refresh.
        let back_runtime = runtime.clone();
        let refresh_runtime = runtime.clone();
        let header = div()
            .flex()
            .items_center()
            .gap(px(4.))
            .px(px(8.))
            .pb(px(8.))
            .child(
                Button::new("quota-back")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ArrowLeft)
                    .tooltip("Back to the workbench")
                    .on_click(move |_, _, _| back_runtime.send(UiCommand::CloseQuota)),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .text_size(ui_text(14.))
                    .text_color(theme.foreground)
                    .child(t!("zconfig.quota").into_owned()),
            )
            .child(
                Button::new("quota-page-refresh")
                    .ghost()
                    .xsmall()
                    .icon(IconName::RefreshCw)
                    .tooltip("Refresh the allowance")
                    .on_click(move |_, _, _| refresh_runtime.send(UiCommand::RefreshQuota)),
            );

        // ---- the allowance itself (GLM only — see the module doc) ----
        let allowance: Vec<AnyElement> = if page.quota_unavailable {
            vec![config_empty(&theme, &t!("quota.unavailable"))]
        } else if page.glm_kind.as_deref() == Some("success") {
            page.glm_items
                .iter()
                .map(|item| quota_row(&theme, item))
                .collect()
        } else if page.loading && page.glm_kind.is_none() {
            // The frame's spinner-while-nothing-shown: a refresh over an
            // already-populated list keeps the list on screen.
            vec![config_empty(&theme, &t!("zconfig.loading"))]
        } else {
            vec![config_empty(&theme, &t!("quota.unavailable"))]
        };

        // ---- reset cards ----
        let team_blocked = page
            .provider_id
            .as_deref()
            .is_some_and(is_team_plan);
        let cards_body: Vec<AnyElement> = if !page.credentials {
            // The credential store is encrypted per machine; not being able
            // to read it is a normal state, distinct from having no plan.
            vec![config_empty(&theme, &t!("zconfig.resetUnavailable"))]
        } else if page.provider_id.is_none() {
            vec![config_empty(&theme, &t!("zconfig.resetPickProvider"))]
        } else if team_blocked {
            // Not a missing feature — the request cannot work from a
            // headless process, so offer the desktop app instead of a dead
            // button.
            vec![config_empty(&theme, &t!("zconfig.resetTeamBlocked"))]
        } else if page.busy && page.cards.is_none() {
            vec![config_empty(&theme, &t!("zconfig.resetLoading"))]
        } else if let Some(cards) = &page.cards {
            vec![
                card_list(
                    &theme,
                    &page,
                    &self.quota_confirm,
                    &runtime,
                    &wb,
                    CardListArgs {
                        title: t!("zconfig.resetFiveHour").into_owned(),
                        cards: &cards.available_five_hour,
                        last_used: cards.latest_five_hour.as_ref().map(|u| u.used_at),
                        reset_type: ResetCardType::FiveHour,
                        above_threshold: window_used_percent(
                            &page,
                            ResetCardType::FiveHour,
                        ) >= SPEND_THRESHOLD_PCT,
                    },
                ),
                card_list(
                    &theme,
                    &page,
                    &self.quota_confirm,
                    &runtime,
                    &wb,
                    CardListArgs {
                        title: t!("zconfig.resetWeek").into_owned(),
                        cards: &cards.available_week,
                        last_used: cards.latest_week.as_ref().map(|u| u.used_at),
                        reset_type: ResetCardType::Week,
                        above_threshold: window_used_percent(&page, ResetCardType::Week)
                            >= SPEND_THRESHOLD_PCT,
                    },
                ),
            ]
        } else {
            vec![config_empty(&theme, &t!("zconfig.resetUnavailable"))]
        };

        // ConfigPageFrame state precedence: unsupported > error > content.
        let body = if page.supported == Some(false) {
            div()
                .id("quota-unsupported-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(
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
                                .text_size(ui_text(13.))
                                .text_color(theme.muted_foreground)
                                .child(t!("zconfig.unsupported").into_owned()),
                        )
                        .child(
                            div()
                                .pt(px(6.))
                                .text_size(ui_text(12.))
                                .text_color(faint(&theme))
                                .child(t!("zconfig.unsupportedHint").into_owned()),
                        ),
                )
                .into_any_element()
        } else if let Some(error) = &page.error {
            div()
                .id("quota-error-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(
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
                                .text_size(ui_text(13.))
                                .text_color(theme.muted_foreground)
                                .child(t!("zconfig.loadFailed").into_owned()),
                        )
                        .child(
                            div()
                                .pt(px(6.))
                                .text_size(ui_text(12.))
                                .text_color(faint(&theme))
                                .child(error.clone()),
                        ),
                )
                .into_any_element()
        } else {
            let mut scroll = div()
                .id("quota-body")
                .test_support()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(config_block(&theme, None, false).children(allowance))
                .child(
                    config_block(&theme, Some(&t!("zconfig.resetCards")), true)
                        // The hub lists the provider ids that COULD own
                        // cards; it does not know which one this account
                        // actually has, so the user picks.
                        .when(page.eligible.len() > 1, |this| {
                            this.child(provider_picker(&theme, &page, &runtime))
                        })
                        .children(cards_body),
                );
            if let Some(at) = page.usage_stats_at {
                scroll = scroll.child(
                    div()
                        .px(px(16.))
                        .pt(px(12.))
                        .pb(px(8.))
                        .flex()
                        .justify_center()
                        .text_size(ui_text(12.))
                        .text_color(faint(&theme))
                        .child(format!("{} {}", t!("quota.updated"), fmt_stamp(at))),
                );
            }
            scroll.into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .pt(px(8.))
            .child(header)
            .when_some(self.snap.config_notice.as_deref(), |this, notice| {
                this.child(notice_banner(&theme, notice, &runtime))
            })
            .when(self.snap.pending_restart, |this| {
                this.child(restart_note(
                    &theme,
                    // The restart addresses the ACTIVE TAB's bridge (the
                    // desktop's connected-session instance); with no tab open
                    // the note shows its no-instance hint instead.
                    self.snap.active_tab.is_some(),
                    &runtime,
                ))
            })
            .child(body)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    // Explicit imports, NOT `use super::*`: the module-level kit glob
    // re-exports GPUI's `test` macro, which would shadow Rust's built-in
    // #[test] (the kit's own doc note).
    use super::{
        fmt_percent, heat_rgb, is_team_plan, notice_text, provider_label,
        window_used_percent, QuotaPageView,
    };
    use protocol::settings::ResetCardType;

    #[test]
    fn percent_shapes_match_the_ts_rounding() {
        assert_eq!(fmt_percent(0.0), "0");
        assert_eq!(fmt_percent(12.34), "12.3");
        assert_eq!(fmt_percent(100.0), "100");
        assert_eq!(fmt_percent(87.06), "87.1");
    }

    #[test]
    fn heat_scale_anchors_green_yellow_red() {
        assert_eq!(heat_rgb(0.0), (34, 197, 94));
        assert_eq!(heat_rgb(50.0), (234, 179, 8));
        assert_eq!(heat_rgb(100.0), (239, 68, 68));
        assert_eq!(heat_rgb(140.0), (239, 68, 68));
    }

    #[test]
    fn provider_labels_show_family_and_plan_kind() {
        assert_eq!(
            provider_label("account:bigmodel-individual-coding-plan"),
            "Bigmodel · Personal"
        );
        assert_eq!(
            provider_label("account:zai-team-coding-plan"),
            "Zai · Team"
        );
        assert!(is_team_plan("account:bigmodel-team-coding-plan"));
        assert!(!is_team_plan("account:bigmodel-individual-coding-plan"));
    }

    #[test]
    fn window_usage_reads_the_cards_own_window_and_defaults_to_zero() {
        let item = |key: &str, used_percent: f64| protocol::quota::QuotaItem {
            key: key.into(),
            label: key.into(),
            used_percent,
            left_percent: None,
            used_count: None,
            total_count: None,
            next_reset_time: None,
            detail: None,
        };
        let mut page = QuotaPageView::default();
        assert_eq!(window_used_percent(&page, ResetCardType::FiveHour), 0.0);
        page.glm_items = vec![item("token_5h", 93.5), item("token_week", 12.0)];
        // An exhausted week must not let a 5-hour card skip its warning.
        assert_eq!(window_used_percent(&page, ResetCardType::FiveHour), 93.5);
        assert_eq!(window_used_percent(&page, ResetCardType::Week), 12.0);
    }

    #[test]
    fn notices_resolve_the_catalog_keys_and_pass_text_through() {
        assert_eq!(
            notice_text("notice.configResetSpent"),
            "Card spent — the window is reset"
        );
        assert_eq!(notice_text("plain failure text"), "plain failure text");
    }
}
