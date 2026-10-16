//! The Workflows page (batch J, requirements §"workflow parity"): the
//! desktop port of `src/screens/config/WorkflowsPage.tsx` and its run-detail
//! events view, over the hub's machine-level `/api/workflow-overview` —
//! the page renders the answer as-is (one group per workspace plus the
//! GLOBAL group, the machine-wide live zone, the finished complement).
//!
//! Three layers, exactly like the TS page stack: the overview list, one
//! workflow (detail + journal history + start), one run (the journal events
//! rendered through `protocol::workflow_run_event_text`, cursor-paginated
//! via `afterSequence` + `hasMore`). Actions address the row's own instance
//! — the hub's `ownerInstanceId` annotation, falling back to the connected
//! tab's instance (the TS launch-memory fallback has no desktop analogue:
//! this client keeps no launch memory, so a row without an
//! `acpSessionId` stays read-only).
//!
//! The WorkflowGate sheet (J2) is the machine-level switch's three-position
//! picker (auto / on / off) over `PUT /settings/workflow-gate`.

use std::collections::BTreeMap;

use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, Context, Entity, Rgba, Window, div, px, rgb};
use gpui_kit::component;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _,
};
use gpui_kit::assets::IconName as KitIconName;
use gpui_kit::*;
use gpui_kit::TestSupportExt as _;
use protocol::settings::{GateOverride, WorkflowGateBlock, WorkflowGateModeSetting};
use protocol::workflow::{
    ConversationRunSummary, WorkflowArgDeclaration, WorkflowDetailResponse, WorkflowHubGroup,
    WorkflowRunEvent, WorkflowRunRow, WorkflowScope,
};
use protocol::workflow_args::{
    args_declaration, args_initial_values, parse_args_input, typed_args_report,
};
use protocol::workflow_run_event_text::{describe_run_event, status_word, RunEventTone, Translate};
use rust_i18n::t;

use crate::i18n::fill;
use crate::runtime::{UiCommand, WorkflowDetailView, WorkflowRunView, WorkflowsPageView};
use crate::workbench::Workbench;

// ---------------------------------------------------------------------------
// Small shared pieces
// ---------------------------------------------------------------------------

/// rust-i18n adapter for the protocol's flat-key i18next contract: `t!`
/// resolves the (dotted) key, `fill` interpolates the `{{var}}` markers the
/// shared catalogs carry.
pub(crate) struct Catalog;

impl Translate for Catalog {
    fn t(&self, key: &str, values: &[(&'static str, String)]) -> String {
        let text: String = t!(key).into_owned();
        let args: Vec<(&str, &str)> =
            values.iter().map(|(k, v)| (*k, v.as_str())).collect();
        fill(&text, &args)
    }
}

/// Journal status tokens for a run that is still flying.
pub(crate) fn run_is_active(status: &str) -> bool {
    status == "pending" || status == "running"
}

/// Desktop-parity status presentation (upstream run-status-presentation): a
/// colored dot ALWAYS pairs with the word. Success emerald, failure red,
/// live sky, pending an empty ring, and the stopped family
/// (stopped/cancelled/superseded/interrupted) deliberately NEUTRAL — a
/// stopped run is not an error. Unknown tokens fall back neutral.
fn status_dot(status: &str) -> Rgba {
    match status {
        "completed" | "ok" | "settled" => rgb(0x34d399),
        "errored" | "failed" => rgb(0xf87171),
        "running" | "dispatched" | "executing" => rgb(0x38bdf8),
        "pending" | "queued" | "waiting" => rgb(0x6b7280),
        _ => rgb(0x9ca3af),
    }
}

/// The status badge (TS `StatusBadge`): dot + localized word.
fn status_badge(status: Option<&str>, key: &str, theme: &component::Theme) -> AnyElement {
    let token = status.unwrap_or("");
    let word: SharedString = if token.is_empty() {
        "?".into()
    } else {
        status_word(&Catalog, Some(token)).into()
    };
    div()
        .id(SharedString::from(format!("wf-badge-{key}")))
        .test_support()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(5.))
        .px(px(6.))
        .py(px(1.))
        .rounded_md()
        .bg(theme.secondary)
        .text_size(px(10.))
        .text_color(theme.muted_foreground)
        .child(
            div()
                .size(px(5.))
                .flex_none()
                .rounded_full()
                .bg(status_dot(token)),
        )
        .child(word)
        .into_any_element()
}

/// TS `fmtStamp` renders LOCAL `MM-DD HH:MM`; Rust std has no
/// local-timezone conversion and the workspace pins no date crate, so this
/// renders UTC with an explicit suffix (the same trade the sibling config
/// pages make). Pure civil-from-days arithmetic (Howard Hinnant).
fn fmt_stamp_utc(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, mi) = (rem / 3600, (rem % 3600) / 60);
    let z = days + 719_468;
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{m:02}-{d:02} {h:02}:{mi:02} UTC")
}

/// Compact token count (TS `fmtTokens`): `1.2M`, `34K`, `870`.
fn fmt_tokens(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    let (unit, suffix) = if n < 1_000_000 {
        (1_000u64, "K")
    } else {
        (1_000_000u64, "M")
    };
    let scaled = n as f64 / unit as f64;
    if scaled >= 100.0 {
        format!("{}{}", scaled.round(), suffix)
    } else {
        format!("{scaled:.1}{suffix}")
    }
}

/// The run row's meta line: stamp · tokens · artifact count (TS shape).
fn run_meta_line(r: &WorkflowRunRow) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(at) = r.updated_at {
        parts.push(fmt_stamp_utc(at));
    }
    if let Some(tokens) = r.spent_tokens {
        parts.push(fmt_tokens(tokens));
    }
    if let Some(artifacts) = &r.artifacts {
        if !artifacts.is_empty() {
            parts.push(format!("{}◆", artifacts.len()));
        }
    }
    parts.join(" · ")
}

/// The switch's current position (TS `gatePosition`): the `override` field
/// names the position that produced it (absent = auto). "on" covers both
/// enabled override modes — an env-set onDemand reads as on too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GatePosition {
    Auto,
    On,
    Off,
}

pub(crate) fn gate_position(gate: Option<&WorkflowGateBlock>) -> GatePosition {
    match gate.map(|g| g.r#override) {
        Some(Some(GateOverride::Disabled)) => GatePosition::Off,
        Some(Some(_)) => GatePosition::On,
        _ => GatePosition::Auto,
    }
}

/// The position's label key (the config panel's switch row subtitle).
pub(crate) fn gate_position_label(pos: GatePosition) -> String {
    match pos {
        GatePosition::Auto => t!("zconfig.workflowGateAuto").into_owned(),
        GatePosition::On => t!("zconfig.workflowGateOn").into_owned(),
        GatePosition::Off => t!("zconfig.workflowGateOff").into_owned(),
    }
}

// ---------------------------------------------------------------------------
// The page frame + overview body
// ---------------------------------------------------------------------------

/// The Workflows page: one frame (back / title / refresh / notice), three
/// layers (run > workflow > overview). The open forms replace the page
/// while open — the workbench's render decides.
pub(crate) fn render_workflows_page(wb: &Workbench, cx: &Context<Workbench>) -> AnyElement {
    let theme = cx.theme();
    let page = wb
        .snap
        .workflows_page
        .clone()
        .expect("render_workflows_page with no open page");
    let runtime = wb.runtime.clone();

    // The header's back target follows the layer.
    let (title, back): (String, UiCommand) = if page.run.is_some() {
        (
            page.run
                .as_ref()
                .and_then(|r| r.name.clone())
                .unwrap_or_else(|| t!("zconfig.workflows").into_owned()),
            UiCommand::CloseWorkflowRun,
        )
    } else if let Some(d) = &page.detail {
        (d.name.clone(), UiCommand::CloseWorkflowDetail)
    } else {
        (t!("zconfig.workflows").into_owned(), UiCommand::CloseWorkflows)
    };

    let mut column = div()
        .id("workflows-page")
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .overflow_x_hidden();

    let runtime2 = runtime.clone();
    column = column.child(
        div()
            .flex()
            .items_center()
            .gap_1()
            .h_9()
            .px_3()
            .flex_none()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.title_bar)
            .child(
                Button::new("workflows-back")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ArrowLeft)
                    .tooltip("Back")
                    .on_click(move |_, _, _| runtime.send(back.clone())),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(theme.foreground)
                    .truncate()
                    .min_w_0()
                    .child(title),
            )
            .child(div().flex_1())
            .child(
                Button::new("workflows-refresh")
                    .ghost()
                    .xsmall()
                    .icon(IconName::RefreshCw)
                    .tooltip("Reload")
                    .disabled(page.loading)
                    .on_click(move |_, _, _| runtime2.send(UiCommand::RefreshWorkflows)),
            ),
    );

    // The notice banner (write outcomes land here, like every config page).
    if let Some(notice) = wb.snap.config_notice.as_deref() {
        let runtime3 = wb.runtime.clone();
        column = column.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .mx_3()
                .mt_2()
                .px_3()
                .py_2()
                .flex_none()
                .rounded_md()
                .bg(theme.secondary)
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .text_size(px(11.))
                        .text_color(theme.foreground)
                        .child(
                            // A `notice.*` key resolves through the shared
                            // catalog; plain failure text passes through.
                            t!(notice).into_owned(),
                        ),
                )
                .child(
                    Button::new("workflows-notice-dismiss")
                        .ghost()
                        .xsmall()
                        .label("×")
                        .tooltip("Dismiss")
                        .on_click(move |_, _, _| {
                            runtime3.send(UiCommand::DismissConfigNotice)
                        }),
                ),
        );
    }

    let body = if let Some(run) = &page.run {
        render_run_body(wb, run, &page, cx)
    } else if let Some(detail) = &page.detail {
        render_detail_body(wb, detail, &page, cx)
    } else if page.error.is_some() && page.overview.is_none() {
        state_card(&t!("zconfig.loadFailed"), page.error.as_deref().unwrap_or(""), theme)
    } else if page.overview.is_none() {
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .items_center()
            .justify_center()
            .child(Spinner::new())
            .into_any_element()
    } else {
        render_overview_body(wb, &page, cx)
    };
    column.child(body).into_any_element()
}

/// The unsupported / error card (the shared config-page shape).
fn state_card(title: &str, detail: &str, theme: &component::Theme) -> AnyElement {
    div()
        .m_3()
        .px_4()
        .py_6()
        .rounded_xl()
        .bg(theme.title_bar)
        .border_1()
        .border_color(theme.border)
        .flex()
        .flex_col()
        .items_center()
        .child(
            div()
                .text_size(px(12.))
                .text_color(theme.foreground)
                .child(title.to_string()),
        )
        .child(
            div()
                .pt_2()
                .text_size(px(11.))
                .text_color(theme.muted_foreground)
                .child(detail.to_string()),
        )
        .into_any_element()
}

/// The plain empty-state card (TS `ConfigEmpty`).
fn empty_card(text: &str, theme: &component::Theme) -> AnyElement {
    div()
        .m_3()
        .px_4()
        .py_8()
        .rounded_xl()
        .bg(theme.title_bar)
        .border_1()
        .border_color(theme.border)
        .flex()
        .justify_center()
        .child(
            div()
                .text_size(px(12.))
                .text_color(theme.muted_foreground)
                .child(text.to_string()),
        )
        .into_any_element()
}

/// The bordered card a block's rows sit in (TS `ConfigBlock`).
fn rows_card(theme: &component::Theme) -> gpui::Div {
    div()
        .m_3()
        .rounded_xl()
        .bg(theme.title_bar)
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
}

fn block_title(label: &str, theme: &component::Theme) -> gpui::Div {
    div()
        .px_4()
        .pt_3()
        .pb_1()
        .text_size(px(11.))
        .text_color(theme.foreground)
        .child(label.to_string())
}

/// The overview list: the live zone, the finished complement, the group
/// cards, and the create-via-conversation control.
fn render_overview_body(wb: &Workbench, page: &WorkflowsPageView, cx: &Context<Workbench>) -> AnyElement {
    let theme = cx.theme();
    let overview = page.overview.as_ref().expect("caller checked");
    let mut list = div()
        .id("workflows-list")
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .pb_4();

    // The live zone: the machine-wide in-flight runs, each row carrying
    // the hub's owner annotation so stop/detail address the right bridge.
    if !overview.active_runs.is_empty() {
        let mut block = rows_card(theme).child(block_title(
            &t!("zconfig.workflowActiveTitle"),
            theme,
        ));
        for (index, r) in overview.active_runs.iter().enumerate() {
            block = block.child(render_run_zone_row(
                r,
                index,
                "active",
                page.action_instance.as_deref(),
                page.busy,
                wb,
                cx,
            ));
        }
        list = list.child(block);
    }

    // The finished complement (top-level "where did my finished runs go").
    if !overview.recent_runs.is_empty() {
        let mut block = rows_card(theme).child(block_title(
            &t!("zconfig.workflowRecentTitle"),
            theme,
        ));
        for (index, r) in overview.recent_runs.iter().enumerate() {
            block = block.child(render_run_zone_row(
                r,
                index,
                "recent",
                page.action_instance.as_deref(),
                page.busy,
                wb,
                cx,
            ));
        }
        list = list.child(block);
    }

    let all_empty = overview.groups.iter().all(|g| {
        g.error.is_none() && g.workflows.is_empty() && g.invalid.is_empty()
    });
    if all_empty {
        list = list.child(empty_card(&t!("zconfig.workflowsEmpty"), theme));
    } else {
        for (index, group) in overview.groups.iter().enumerate() {
            list = list.child(render_group_card(group, index, wb, cx));
        }
    }

    // Create via conversation: the bridge hands back the desktop's
    // prefilled prompt; the app stages it and never auto-sends.
    let runtime = wb.runtime.clone();
    let can_create = page.action_instance.is_some();
    let create_instance = page.action_instance.clone();
    list = list.child(
        div().px_3().child(
            Button::new("workflows-create-chat")
                .secondary()
                .small()
                .w_full()
                .icon(IconName::Plus)
                .label(t!("zconfig.workflowCreateChat").into_owned())
                .disabled(!can_create || page.busy)
                .on_click(move |_, _, _| {
                    if let Some(instance_id) = create_instance.clone() {
                        runtime.send(UiCommand::WorkflowCreateViaChat { instance_id });
                    }
                }),
        ),
    );
    if !can_create {
        list = list.child(
            div()
                .px_4()
                .pt_1()
                .text_size(px(10.))
                .text_color(theme.muted_foreground)
                .child(t!("zconfig.workflowNeedInstance").into_owned()),
        );
    }
    list.into_any_element()
}

/// One row of the active/recent zones: label, stamp, badge, and the
/// Detail/Stop (active) or Detail/dismiss (recent) controls.
fn render_run_zone_row(
    r: &WorkflowRunRow,
    index: usize,
    zone: &str,
    action_instance: Option<&str>,
    busy: bool,
    wb: &Workbench,
    cx: &Context<Workbench>,
) -> AnyElement {
    let theme = cx.theme();
    let label = r
        .name
        .clone()
        .unwrap_or_else(|| r.run_id.chars().take(12).collect());
    // The hub's owner annotation is the live truth; the connected tab's
    // instance is the last resort. No acpSessionId → read-only row.
    let row_instance = r
        .owner_instance_id
        .clone()
        .or_else(|| action_instance.map(str::to_string));
    let row_session = r.acp_session_id.clone();
    let key = format!("{zone}-{index}");
    let slug_run = slug_run_id(&r.run_id);

    let mut card = div().flex().flex_col().px_4().py_3().gap_2();
    let mut line = div().flex().items_center().gap_2().min_w_0().child(
        div()
            .flex_1()
            .min_w_0()
            .text_size(px(12.))
            .text_color(theme.foreground)
            .truncate()
            .child(label.clone()),
    );
    if let Some(at) = r.updated_at {
        line = line.child(
            div()
                .flex_none()
                .text_size(px(10.))
                .text_color(theme.muted_foreground)
                .child(fmt_stamp_utc(at)),
        );
    }
    line = line.child(status_badge(Some(&r.status), &key, theme));
    card = card.child(line);

    if let (Some(instance_id), Some(session_id)) = (row_instance.clone(), row_session.clone()) {
        let mut actions = div().flex().flex_wrap().gap(px(6.));
        let runtime = wb.runtime.clone();
        let (run_id, name, session_detail, instance_detail) = (
            r.run_id.clone(),
            r.name.clone(),
            session_id.clone(),
            instance_id.clone(),
        );
        actions = actions.child(
            Button::new(SharedString::from(format!("wf-{key}-detail")))
                .secondary()
                .outline()
                .xsmall()
                .label(t!("zconfig.workflowRunDetail").into_owned())
                .on_click(move |_, _, _| {
                    runtime.send(UiCommand::OpenWorkflowRun {
                        instance_id: instance_detail.clone(),
                        session_id: session_detail.clone(),
                        run_id: run_id.clone(),
                        name: name.clone(),
                    });
                }),
        );
        if zone == "active" {
            let runtime = wb.runtime.clone();
            let (run_id, session_stop, instance_stop) =
                (r.run_id.clone(), session_id.clone(), instance_id.clone());
            actions = actions.child(
                Button::new(SharedString::from(format!("wf-{key}-stop-{slug_run}")))
                    .danger()
                    .outline()
                    .xsmall()
                    .label(t!("zconfig.workflowStopRun").into_owned())
                    .disabled(busy)
                    .on_click(move |_, _, _| {
                        runtime.send(UiCommand::WorkflowStopRun {
                            run_id: run_id.clone(),
                            session_id: session_stop.clone(),
                            instance_id: instance_stop.clone(),
                        });
                    }),
            );
        } else {
            let runtime = wb.runtime.clone();
            let (run_id, instance_dismiss) = (r.run_id.clone(), instance_id.clone());
            actions = actions.child(
                Button::new(SharedString::from(format!("wf-{key}-dismiss-{slug_run}")))
                    .ghost()
                    .xsmall()
                    .label(t!("zconfig.workflowDismissRun").into_owned())
                    .disabled(busy)
                    .on_click(move |_, _, _| {
                        runtime.send(UiCommand::DismissWorkflowRun {
                            instance_id: instance_dismiss.clone(),
                            run_id: run_id.clone(),
                        });
                    }),
            );
        }
        card = card.child(actions);
    }
    card.into_any_element()
}

/// Element-id-safe slug for run ids (ids cannot carry '/', ':', …).
fn slug_run_id(run_id: &str) -> String {
    run_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect()
}

/// One group card: the global workflows, or one workspace's project
/// workflows — name + last-run badge, two-line description, the footer of
/// last-run time plus arg-name chips, and the invalid-file rows.
fn render_group_card(
    group: &WorkflowHubGroup,
    index: usize,
    wb: &Workbench,
    cx: &Context<Workbench>,
) -> AnyElement {
    let theme = cx.theme();
    let label = if group.scope == WorkflowScope::Global {
        t!("zconfig.workflowScope_global").into_owned()
    } else {
        group
            .workspace
            .split('/')
            .filter(|s| !s.is_empty())
            .next_back()
            .unwrap_or(&group.workspace)
            .to_string()
    };

    if group.error.is_some() {
        return rows_card(theme)
            .child(block_title(&label, theme))
            .child(
                div()
                    .px_4()
                    .pb_3()
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child(t!("zconfig.workflowInstanceUnavailable").into_owned()),
            )
            .into_any_element();
    }
    // An empty project group renders nothing — the noise-free form of
    // "this project has no saved workflows".
    if group.workflows.is_empty() && group.invalid.is_empty() {
        return div().into_any_element();
    }

    let mut block = rows_card(theme).child(block_title(&label, theme));
    for w in &group.workflows {
        let last = group.last_runs.get(&w.name);
        let name = w.name.clone();
        let scope = group.scope;
        let instance_id = group.instance_id.clone();
        let card = div()
            .id(SharedString::from(format!("wf-group-{index}-{}", slug_run_id(&w.name))))
            .test_support()
            .flex()
            .flex_col()
            .gap(px(3.))
            .px_4()
            .py_3()
            .cursor_pointer()
            .hover(|this| this.bg(theme.secondary.opacity(0.6)))
            .on_click({
                let runtime = wb.runtime.clone();
                move |_, _, _| {
                    runtime.send(UiCommand::OpenWorkflowDetail {
                        scope,
                        name: name.clone(),
                        instance_id: instance_id.clone(),
                    });
                }
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(12.))
                            .text_color(theme.foreground)
                            .truncate()
                            .child(w.name.clone()),
                    )
                    .when_some(
                        last.map(|l| status_badge(Some(&l.status), &format!("group-{index}-{}", slug_run_id(&w.name)), theme)),
                        |this, badge| this.child(badge),
                    )
                    .child(
                        div()
                            .size_3()
                            .flex_none()
                            .text_color(theme.muted_foreground)
                            .child(Icon::new(IconName::ChevronRight)),
                    ),
            )
            .when_some(w.description.clone(), |this, description| {
                this.child(
                    div()
                        .text_size(px(10.))
                        .text_color(theme.muted_foreground)
                        .child(description),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(6.))
                    .min_w_0()
                    .when_some(
                        last.and_then(|l| l.updated_at).map(|at| {
                            div()
                                .text_size(px(10.))
                                .text_color(theme.muted_foreground)
                                .child(fmt_stamp_utc(at))
                                .into_any_element()
                        }),
                        |this, stamp| this.child(stamp),
                    )
                    .children({
                        // The last run's arg names, first three (TS shape).
                        let arg_names: Vec<String> = last
                            .and_then(|l| l.args.as_ref())
                            .map(|args| args.keys().cloned().collect())
                            .unwrap_or_default();
                        arg_names
                            .into_iter()
                            .take(3)
                            .map(|a| {
                                div()
                                    .px(px(6.))
                                    .py(px(1.))
                                    .rounded_md()
                                    .bg(theme.secondary)
                                    .font_family(theme.mono_font_family.clone())
                                    .text_size(px(10.))
                                    .text_color(theme.muted_foreground)
                                    .child(a)
                                    .into_any_element()
                            })
                    }),
            );
        block = block.child(card);
    }
    for e in &group.invalid {
        block = block.child(
            div().px_4().py_2().child(
                div()
                    .w_full()
                    .font_family(theme.mono_font_family.clone())
                    .text_size(px(10.))
                    .text_color(theme.muted_foreground)
                    .truncate()
                    .child(e.path.clone().unwrap_or_default()),
            ),
        );
    }
    block.into_any_element()
}

// ---------------------------------------------------------------------------
// The workflow detail body
// ---------------------------------------------------------------------------

fn render_detail_body(
    wb: &Workbench,
    detail: &WorkflowDetailView,
    page: &WorkflowsPageView,
    cx: &Context<Workbench>,
) -> AnyElement {
    let theme = cx.theme();
    let mut list = div()
        .id("workflow-detail-list")
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .pb_4();

    if detail.loading && detail.detail.is_none() {
        return div()
            .flex_1()
            .min_h_0()
            .flex()
            .items_center()
            .justify_center()
            .child(Spinner::new())
            .into_any_element();
    }

    // The identity card: scope chip (+ the instance for a project scope),
    // description, whenToUse, the file path.
    let mut card = rows_card(theme).child(
        div()
            .flex()
            .items_center()
            .gap_2()
            .min_w_0()
            .px_4()
            .pt_3()
            .child(
                div()
                    .flex_none()
                    .px(px(6.))
                    .py(px(1.))
                    .rounded_md()
                    .bg(theme.secondary)
                    .text_size(px(10.))
                    .text_color(theme.muted_foreground)
                    .child(if detail.scope == WorkflowScope::Global {
                        t!("zconfig.workflowScope_global").into_owned()
                    } else {
                        t!("zconfig.workflowScope_project").into_owned()
                    }),
            )
            .when(detail.scope == WorkflowScope::Project, |this| {
                this.child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .font_family(theme.mono_font_family.clone())
                        .text_size(px(10.))
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(detail.instance_id.clone()),
                )
            }),
    );
    if let Some(d) = &detail.detail {
        card = card
            .when_some(meta_string(d, "description"), |this, text| {
                this.child(
                    div()
                        .px_4()
                        .py_2()
                        .text_size(px(11.))
                        .text_color(theme.muted_foreground)
                        .child(text),
                )
            })
            .when_some(meta_string(d, "whenToUse"), |this, text| {
                this.child(
                    div()
                        .px_4()
                        .pb_2()
                        .text_size(px(10.))
                        .text_color(theme.muted_foreground)
                        .child(text),
                )
            })
            .when_some(d.path.clone(), |this, path| {
                this.child(
                    div()
                        .px_4()
                        .pb_3()
                        .w_full()
                        .font_family(theme.mono_font_family.clone())
                        .text_size(px(10.))
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(path),
                )
            });
    }
    list = list.child(card);

    // The start + promote controls.
    let wb_handle = cx.entity();
    let (scope, name, instance_id) =
        (detail.scope, detail.name.clone(), detail.instance_id.clone());
    let mut controls = div().px_3().child(
        Button::new("workflow-start-open")
            .primary()
            .small()
            .w_full()
            .icon(IconName::Play)
            .label(t!("zconfig.workflowStart").into_owned())
            .on_click(move |_, window, cx| {
                let (scope, name, instance_id) = (scope, name.clone(), instance_id.clone());
                wb_handle.update(cx, |this, cx| {
                    this.open_workflow_start_form(scope, &name, &instance_id, window, cx);
                });
            }),
    );
    if detail.scope == WorkflowScope::Project {
        // Promote-to-global rides the chat-prefill path (no dedicated REST).
        let runtime = wb.runtime.clone();
        let name = detail.name.clone();
        controls = controls.child(
            div().pt_2().child(
                Button::new("workflow-promote-global")
                    .secondary()
                    .outline()
                    .small()
                    .w_full()
                    .icon(IconName::ArrowUp)
                    .label(t!("zconfig.workflowPromoteGlobal").into_owned())
                    .on_click(move |_, _, _| {
                        runtime.send(UiCommand::WorkflowPromoteGlobal { name: name.clone() });
                    }),
            ),
        );
    }
    list = list.child(controls);

    // The run history: rows with the summary-joined status/resumable
    // verdicts; a row without an acpSessionId stays read-only.
    let mut runs_block = rows_card(theme).child(block_title(&t!("zconfig.workflowRunsTitle"), theme));
    match &detail.runs {
        None => {}
        Some(runs) if runs.is_empty() => {
            runs_block = runs_block.child(
                div()
                    .px_4()
                    .pb_4()
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child(t!("zconfig.workflowRunsEmpty").into_owned()),
            );
        }
        Some(runs) => {
            for r in runs {
                runs_block = runs_block.child(render_detail_run_row(
                    r,
                    detail,
                    page.busy,
                    wb,
                    cx,
                ));
            }
        }
    }
    list = list.child(runs_block);
    list.into_any_element()
}

/// The detail payload's prose meta field (TS reads `meta.description` /
/// `meta.whenToUse` as strings; anything else renders as nothing).
fn meta_string(d: &WorkflowDetailResponse, key: &str) -> Option<String> {
    d.meta
        .as_ref()
        .and_then(|m| m.get(key))
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn render_detail_run_row(
    r: &WorkflowRunRow,
    detail: &WorkflowDetailView,
    busy: bool,
    wb: &Workbench,
    cx: &Context<Workbench>,
) -> AnyElement {
    let theme = cx.theme();
    let row_session = r.acp_session_id.clone();
    let row_instance = detail.instance_id.clone();
    let summary: Option<&ConversationRunSummary> = row_session
        .as_ref()
        .and_then(|sid| detail.summaries.get(sid))
        .and_then(|list| list.iter().find(|s| s.run_id == r.run_id));
    let status = summary.map(|s| s.status.as_str()).unwrap_or(r.status.as_str());
    let label = r
        .name
        .clone()
        .unwrap_or_else(|| detail.name.clone());
    let slug = slug_run_id(&r.run_id);

    let mut card = div().flex().flex_col().px_4().py_3().gap_2();
    let mut line = div()
        .flex()
        .items_center()
        .gap_2()
        .min_w_0()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.foreground)
                        .truncate()
                        .child(label),
                )
                .when(
                    !run_meta_line(r).is_empty(),
                    |this| {
                        this.child(
                            div()
                                .text_size(px(10.))
                                .text_color(theme.muted_foreground)
                                .truncate()
                                .child(run_meta_line(r)),
                        )
                    },
                )
                // Lineage chips (the bridge's joined fields, §3.2).
                .when_some(r.resumed_from.clone(), |this, from| {
                    this.child(
                        div()
                            .text_size(px(10.))
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(format!(
                                "{} …{}",
                                t!("zconfig.workflowLineageFrom"),
                                tail8(&from)
                            )),
                    )
                })
                .when_some(r.superseded_by.clone(), |this, by| {
                    this.child(
                        div()
                            .text_size(px(10.))
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(format!(
                                "{} …{}",
                                t!("zconfig.workflowLineageBy"),
                                tail8(&by)
                            )),
                    )
                }),
        )
        .child(status_badge(Some(status), &format!("detail-run-{slug}"), theme));
    // Dismissal hides one settled run from every bridge-served list.
    if !run_is_active(status) {
        let runtime = wb.runtime.clone();
        let run_id = r.run_id.clone();
        let instance_id = row_instance.clone();
        line = line.child(
            Button::new(SharedString::from(format!("wf-detail-dismiss-{slug}")))
                .ghost()
                .xsmall()
                .label("×")
                .tooltip(t!("zconfig.workflowDismissRun").into_owned())
                .disabled(busy)
                .on_click(move |_, _, _| {
                    runtime.send(UiCommand::DismissWorkflowRun {
                        instance_id: instance_id.clone(),
                        run_id: run_id.clone(),
                    });
                }),
        );
    }
    card = card.child(line);

    if let Some(session_id) = row_session {
        let mut actions = div().flex().flex_wrap().gap(px(6.));
        let runtime = wb.runtime.clone();
        let (run_id, name, session_detail, instance_detail) = (
            r.run_id.clone(),
            r.name.clone().unwrap_or_else(|| detail.name.clone()),
            session_id.clone(),
            row_instance.clone(),
        );
        actions = actions.child(
            Button::new(SharedString::from(format!("wf-detail-open-{slug}")))
                .secondary()
                .outline()
                .xsmall()
                .label(t!("zconfig.workflowRunDetail").into_owned())
                .on_click(move |_, _, _| {
                    runtime.send(UiCommand::OpenWorkflowRun {
                        instance_id: instance_detail.clone(),
                        session_id: session_detail.clone(),
                        run_id: run_id.clone(),
                        name: Some(name.clone()),
                    });
                }),
        );
        let runtime2 = wb.runtime.clone();
        let (session_open, instance_open) = (session_id.clone(), row_instance.clone());
        actions = actions.child(
            Button::new(SharedString::from(format!("wf-detail-session-{slug}")))
                .secondary()
                .outline()
                .xsmall()
                .label(t!("zconfig.workflowOpenSession").into_owned())
                .on_click(move |_, _, _| {
                    runtime2.send(UiCommand::OpenSession {
                        instance_id: instance_open.clone(),
                        session_id: session_open.clone(),
                    });
                }),
        );
        if summary.is_some_and(|s| s.resumable) {
            let runtime3 = wb.runtime.clone();
            let (run_id, session_resume, name) =
                (r.run_id.clone(), session_id.clone(), r.name.clone());
            actions = actions.child(
                Button::new(SharedString::from(format!("wf-detail-resume-{slug}")))
                    .secondary()
                    .xsmall()
                    .label(t!("zconfig.workflowResume").into_owned())
                    .disabled(busy)
                    .on_click(move |_, _, _| {
                        runtime3.send(UiCommand::WorkflowResumeRun {
                            run_id: run_id.clone(),
                            session_id: session_resume.clone(),
                            name: name.clone(),
                            instance_id: row_instance.clone(),
                        });
                    }),
            );
        }
        card = card.child(actions);
    } else {
        card = card.child(
            div()
                .text_size(px(10.))
                .text_color(theme.muted_foreground)
                .child(t!("zconfig.workflowRunUnknownSession").into_owned()),
        );
    }
    card.into_any_element()
}

/// The last 8 chars of an id (the lineage chips' short form).
fn tail8(id: &str) -> String {
    id.chars().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect()
}

// ---------------------------------------------------------------------------
// The run body (journal events)
// ---------------------------------------------------------------------------

fn render_run_body(
    wb: &Workbench,
    run: &WorkflowRunView,
    page: &WorkflowsPageView,
    cx: &Context<Workbench>,
) -> AnyElement {
    let theme = cx.theme();
    let mut list = div()
        .id("workflow-run-list")
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .pb_4();

    // The read-only degradation note (the polls stopped believing the
    // session is reachable).
    if run.session_dead {
        list = list.child(
            div()
                .m_3()
                .px_4()
                .py_3()
                .rounded_xl()
                .bg(theme.title_bar)
                .border_1()
                .border_color(theme.border)
                .text_size(px(11.))
                .text_color(theme.warning_foreground)
                .child(t!("zconfig.workflowSessionLost").into_owned()),
        );
    }

    // The status line + actions.
    let summary_status = run.summary.as_ref().map(|s| s.status.as_str());
    let mut header = div()
        .flex()
        .items_center()
        .gap_2()
        .min_w_0()
        .m_3()
        .px_4()
        .py_3()
        .rounded_xl()
        .bg(theme.title_bar)
        .border_1()
        .border_color(theme.border)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div().flex().items_center().gap_2().min_w_0()
                        .child(status_badge(
                            summary_status,
                            "run-header",
                            theme,
                        ))
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .truncate()
                                .text_size(px(10.))
                                .text_color(theme.muted_foreground)
                                .child({
                                    let mut parts: Vec<String> = Vec::new();
                                    if let Some(at) = run.summary.as_ref().and_then(|s| s.updated_at) {
                                        parts.push(fmt_stamp_utc(at));
                                    }
                                    if let Some(reason) =
                                        run.summary.as_ref().and_then(|s| s.stop_reason.clone())
                                    {
                                        parts.push(reason);
                                    }
                                    if let Some(msg) =
                                        run.summary.as_ref().and_then(|s| s.failure_message.clone())
                                    {
                                        parts.push(msg);
                                    }
                                    parts.join(" · ")
                                }),
                        ),
                )
                .when_some(
                    run.summary.as_ref().and_then(|s| s.resumed_from.clone()),
                    |this, from| {
                        this.child(
                            div()
                                .text_size(px(10.))
                                .text_color(theme.muted_foreground)
                                .truncate()
                                .child(format!(
                                    "{} …{}",
                                    t!("zconfig.workflowLineageFrom"),
                                    tail8(&from)
                                )),
                        )
                    },
                ),
        );
    if run.active && !run.session_dead {
        let wb_handle = cx.entity();
        let (run_id, session_id, instance_id) =
            (run.run_id.clone(), run.session_id.clone(), run.instance_id.clone());
        header = header.child(
            Button::new("workflow-run-amend-open")
                .secondary()
                .outline()
                .xsmall()
                .flex_none()
                .icon(KitIconName::SlidersHorizontal)
                .tooltip(t!("zconfig.workflowConfigure").into_owned())
                .on_click(move |_, window, cx| {
                    let (run_id, session_id, instance_id) =
                        (run_id.clone(), session_id.clone(), instance_id.clone());
                    wb_handle.update(cx, |this, cx| {
                        this.open_workflow_amend_form(&run_id, &session_id, &instance_id, window, cx);
                    });
                }),
        );
        let runtime = wb.runtime.clone();
        let (run_id, session_id, instance_id) =
            (run.run_id.clone(), run.session_id.clone(), run.instance_id.clone());
        header = header.child(
            Button::new("workflow-run-stop")
                .danger()
                .outline()
                .xsmall()
                .flex_none()
                .label(t!("zconfig.workflowStopRun").into_owned())
                .disabled(page.busy)
                .on_click(move |_, _, _| {
                    runtime.send(UiCommand::WorkflowStopRun {
                        run_id: run_id.clone(),
                        session_id: session_id.clone(),
                        instance_id: instance_id.clone(),
                    });
                }),
        );
    }
    if run.summary.as_ref().is_some_and(|s| s.resumable) {
        let runtime = wb.runtime.clone();
        let (run_id, session_id, instance_id, name) = (
            run.run_id.clone(),
            run.session_id.clone(),
            run.instance_id.clone(),
            run.name.clone(),
        );
        header = header.child(
            Button::new("workflow-run-resume")
                .secondary()
                .small()
                .flex_none()
                .label(t!("zconfig.workflowResume").into_owned())
                .on_click(move |_, _, _| {
                    runtime.send(UiCommand::WorkflowResumeRun {
                        run_id: run_id.clone(),
                        session_id: session_id.clone(),
                        name: name.clone(),
                        instance_id: instance_id.clone(),
                    });
                }),
        );
    }
    list = list.child(header);

    // The journal: each event one row — the protocol renderer's label +
    // detail, never raw JSON by default.
    match &run.events {
        None => {
            if run.active {
                list = list.child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .py_8()
                        .child(Spinner::new()),
                );
            }
        }
        Some(events) if events.is_empty() => {
            list = list.child(empty_card(&t!("zconfig.workflowEventsEmpty"), theme));
        }
        Some(events) => {
            let mut block = rows_card(theme);
            for ev in events {
                block = block.child(render_event_row(ev, theme));
            }
            list = list.child(block);
        }
    }

    // The cursor's next page: `afterSequence` past the highest sequence
    // held, gated on the server's `hasMore`.
    if run.has_more && run.events.is_some() {
        let runtime = wb.runtime.clone();
        list = list.child(
            div().px_3().child(
                Button::new("workflow-run-load-more")
                    .secondary()
                    .small()
                    .w_full()
                    .label(t!("zconfig.workflowLoadMore").into_owned())
                    .disabled(run.events_busy)
                    .on_click(move |_, _, _| runtime.send(UiCommand::WorkflowRunLoadMore)),
            ),
        );
    }
    list.into_any_element()
}

/// One journal row: the readable label + detail (the protocol renderer),
/// styled by tone; a failed row reads red, an unknown kind monospace.
fn render_event_row(ev: &WorkflowRunEvent, theme: &component::Theme) -> AnyElement {
    let desc = describe_run_event(&Catalog, ev);
    let label_color = if desc.mono_label.unwrap_or(false) {
        theme.muted_foreground
    } else if desc.tone == RunEventTone::Failed {
        theme.danger_foreground
    } else {
        theme.foreground
    };
    div()
        .id(SharedString::from(format!("wf-event-{}", ev.sequence)))
        .test_support()
        .flex()
        .items_baseline()
        .gap_2()
        .px_4()
        .py_2()
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .flex_none()
                .max_w(px(240.))
                .truncate()
                .font_family(if desc.mono_label.unwrap_or(false) {
                    theme.mono_font_family.clone()
                } else {
                    theme.font_family.clone()
                })
                .text_size(px(11.))
                .text_color(label_color)
                .child(desc.label),
        )
        .when_some(desc.detail, |this, detail| {
            this.child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .text_size(px(10.))
                    .text_color(theme.muted_foreground)
                    .child(detail),
            )
        })
        .into_any_element()
}

// ---------------------------------------------------------------------------
// The forms (impl Workbench — the TS sheets)
// ---------------------------------------------------------------------------

/// One typed field of the start form (TS WorkflowArgsForm): exactly one of
/// the three controls is live, keyed by the declared type.
pub(crate) struct WorkflowStartField {
    pub(crate) name: String,
    pub(crate) decl: WorkflowArgDeclaration,
    /// string / number raw-text input.
    pub(crate) input: Option<Entity<InputState>>,
    /// JSON-typed raw-text area.
    pub(crate) area: Option<Entity<TextareaState>>,
    /// Booleans always carry a value; prefilled from a `default: true`.
    pub(crate) bool_value: bool,
}

/// The start form's state (TS `StartSheet` + `WorkflowArgsForm`), owned by
/// the workbench so it survives re-renders.
pub(crate) struct WorkflowStartForm {
    pub(crate) scope: WorkflowScope,
    pub(crate) name: String,
    pub(crate) instance_id: String,
    /// The typed fields (declaration present); empty + `free_json` = the
    /// free-form fallback.
    pub(crate) fields: Vec<WorkflowStartField>,
    pub(crate) free_json: Option<Entity<TextareaState>>,
}

impl WorkflowStartForm {
    /// The declaration this form was built from (None = free-form).
    fn decls(&self) -> Option<Vec<(String, WorkflowArgDeclaration)>> {
        if self.fields.is_empty() {
            return None;
        }
        // The fields Vec already carries the declaration order (open built
        // it from the ordered args list) — keep it, the report's args object
        // follows the same order on the wire.
        Some(
            self.fields
                .iter()
                .map(|f| (f.name.clone(), f.decl.clone()))
                .collect(),
        )
    }
}

/// The amend form's state (TS `AmendSheet`): three-state per field — empty
/// = keep, clear = send `null` (revert to default), value = set.
pub(crate) struct WorkflowAmendForm {
    pub(crate) run_id: String,
    pub(crate) session_id: String,
    pub(crate) instance_id: String,
    pub(crate) model: Entity<InputState>,
    pub(crate) concurrency: Entity<InputState>,
    pub(crate) clear_model: bool,
    pub(crate) clear_concurrency: bool,
}

impl Workbench {
    /// Opens the start sheet (TS `setStarting(true)`): the typed form when
    /// the workflow declares args, the free-form JSON textarea otherwise.
    pub fn open_workflow_start_form(
        &mut self,
        scope: WorkflowScope,
        name: &str,
        instance_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The declaration comes from the open detail page's payload.
        let meta = self
            .snap
            .workflows_page
            .as_ref()
            .and_then(|p| p.detail.as_ref())
            .and_then(|d| d.detail.as_ref())
            .and_then(|d| d.meta.clone())
            .map(|m| serde_json::Value::Object(m.into_iter().collect()));
        let decls = args_declaration(meta.as_ref());

        let mut subs = Vec::new();
        let (fields, free_json) = if let Some(decls) = &decls {
            let initial = args_initial_values(decls);
            let mut fields = Vec::new();
            for (fname, decl) in decls {
                let field_type = decl.r#type.clone().unwrap_or_else(|| "string".to_string());
                let input = (field_type == "string" || field_type == "number").then(|| {
                    cx.new(|cx| {
                        let mut state = InputState::new(window, cx);
                        if field_type == "number" {
                            state = state.placeholder("42");
                        }
                        state
                    })
                });
                let area =
                    (field_type != "string" && field_type != "number" && field_type != "boolean")
                        .then(|| cx.new(|cx| TextareaState::new(window, cx)));
                let bool_value = initial.get(fname) == Some(&serde_json::Value::Bool(true));
                if let Some(entity) = &input {
                    let sub = cx.subscribe(entity, |_, _, _: &InputEvent, cx| cx.notify());
                    subs.push(sub);
                }
                if let Some(entity) = &area {
                    let sub = cx.subscribe(entity, |_, _, _: &InputEvent, cx| cx.notify());
                    subs.push(sub);
                }
                fields.push(WorkflowStartField {
                    name: fname.to_string(),
                    decl: decl.clone(),
                    input,
                    area,
                    bool_value,
                });
            }
            (fields, None)
        } else {
            let area = cx.new(|cx| {
                TextareaState::new(window, cx).placeholder("{\"target\": \"src/\"}")
            });
            subs.push(cx.subscribe(&area, |_, _, _: &InputEvent, cx| cx.notify()));
            (Vec::new(), Some(area))
        };

        self.workflow_start_form = Some(WorkflowStartForm {
            scope,
            name: name.to_string(),
            instance_id: instance_id.to_string(),
            fields,
            free_json,
        });
        self.workflow_form_subs = subs;
        cx.notify();
    }

    /// Opens the run-settings amendment sheet (TS `setAmending(true)`):
    /// submit-only — nothing prefilled until the upstream schema extension
    /// lands (§3.1).
    pub fn open_workflow_amend_form(
        &mut self,
        run_id: &str,
        session_id: &str,
        instance_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let model = cx.new(|cx| {
            InputState::new(window, cx).placeholder("zhipu/glm-4.7")
        });
        let concurrency = cx.new(|cx| InputState::new(window, cx).placeholder("4"));
        // Typing into a field un-arms its clear (TS semantics — the two
        // states stay mutually coherent); every keystroke also re-renders
        // the Save's disabled state.
        let sub_m = cx.subscribe(&model, |wb, entity, _: &InputEvent, cx| {
            let typed = !entity.read(cx).value().trim().is_empty();
            if typed {
                if let Some(form) = &mut wb.workflow_amend_form {
                    form.clear_model = false;
                }
            }
            cx.notify();
        });
        let sub_c = cx.subscribe(&concurrency, |wb, entity, _: &InputEvent, cx| {
            let typed = !entity.read(cx).value().trim().is_empty();
            if typed {
                if let Some(form) = &mut wb.workflow_amend_form {
                    form.clear_concurrency = false;
                }
            }
            cx.notify();
        });
        self.workflow_amend_form = Some(WorkflowAmendForm {
            run_id: run_id.to_string(),
            session_id: session_id.to_string(),
            instance_id: instance_id.to_string(),
            model,
            concurrency,
            clear_model: false,
            clear_concurrency: false,
        });
        self.workflow_form_subs = vec![sub_m, sub_c];
        cx.notify();
    }

    /// Closes whichever workflow form is open (a header back).
    pub(crate) fn close_workflow_form(&mut self, cx: &mut Context<Self>) {
        self.workflow_start_form = None;
        self.workflow_amend_form = None;
        self.workflow_form_subs.clear();
        cx.notify();
    }

    /// TS `canSubmit` for the start form: the typed report's error (a
    /// malformed number / JSON field, a missing required field) or the
    /// free-form parse error keeps the submit disabled.
    fn workflow_start_can_submit(&self, cx: &Context<Self>) -> bool {
        let Some(form) = &self.workflow_start_form else {
            return false;
        };
        match form.decls() {
            Some(decls) => {
                let (values, json_texts) = self.workflow_start_values(form, cx);
                typed_args_report(&decls, &values, &json_texts).error.is_none()
            }
            None => {
                parse_args_input(
                    &form
                        .free_json
                        .as_ref()
                        .map(|a| a.read(cx).value().to_string())
                        .unwrap_or_default(),
                )
                .error
                .is_none()
            }
        }
    }

    /// Flips one typed boolean field (the desktop contract: booleans always
    /// carry a value — unchecked = false).
    fn toggle_workflow_start_bool(&mut self, name: &str, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.workflow_start_form {
            for f in &mut form.fields {
                if f.name == name {
                    f.bool_value = !f.bool_value;
                }
            }
        }
        cx.notify();
    }

    /// The typed form's raw inputs as the report's arguments: string and
    /// number fields hand their raw text (the report owes JS `Number()`
    /// semantics), booleans their value, JSON fields their text.
    fn workflow_start_values(
        &self,
        form: &WorkflowStartForm,
        cx: &Context<Self>,
    ) -> (
        BTreeMap<String, serde_json::Value>,
        BTreeMap<String, String>,
    ) {
        let mut values = BTreeMap::new();
        let mut json_texts = BTreeMap::new();
        for f in &form.fields {
            let field_type = f.decl.r#type.clone().unwrap_or_else(|| "string".to_string());
            match field_type.as_str() {
                "boolean" => {
                    values.insert(f.name.clone(), serde_json::Value::Bool(f.bool_value));
                }
                "string" | "number" => {
                    if let Some(input) = &f.input {
                        values.insert(
                            f.name.clone(),
                            serde_json::Value::String(input.read(cx).value().to_string()),
                        );
                    }
                }
                _ => {
                    if let Some(area) = &f.area {
                        json_texts
                            .insert(f.name.clone(), area.read(cx).value().to_string());
                    }
                }
            }
        }
        (values, json_texts)
    }

    /// The start form's submit: dispatch the validated args (empty object =
    /// no args key at all — the driver decides).
    fn submit_workflow_start_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &self.workflow_start_form else {
            return;
        };
        let args = match form.decls() {
            Some(decls) => {
                let (values, json_texts) = self.workflow_start_values(form, cx);
                typed_args_report(&decls, &values, &json_texts).args
            }
            None => parse_args_input(
                &form
                    .free_json
                    .as_ref()
                    .map(|a| a.read(cx).value().to_string())
                    .unwrap_or_default(),
            )
            .args
            .filter(|m| !m.is_empty()),
        };
        let (scope, name, instance_id) =
            (form.scope, form.name.clone(), form.instance_id.clone());
        self.runtime.send(UiCommand::StartWorkflow {
            scope,
            name,
            instance_id,
            args: args.map(serde_json::Value::Object),
        });
        self.close_workflow_form(cx);
    }

    /// The amend form's three-state outcome + validity: `(model, concurrency,
    /// invalid)` where each field is `None` = keep, `Some(None)` = clear,
    /// `Some(Some(v))` = set; `invalid` covers an unparseable concurrency.
    fn workflow_amend_state(
        &self,
        cx: &Context<Self>,
    ) -> (
        Option<Option<String>>,
        Option<Option<u64>>,
        bool,
    ) {
        let Some(form) = &self.workflow_amend_form else {
            return (None, None, false);
        };
        let model_text = form.model.read(cx).value().trim().to_string();
        let model = if form.clear_model {
            Some(None)
        } else if model_text.is_empty() {
            None
        } else {
            Some(Some(model_text))
        };
        let conc_text = form.concurrency.read(cx).value().trim().to_string();
        let mut invalid = false;
        let concurrency = if form.clear_concurrency {
            Some(None)
        } else if conc_text.is_empty() {
            None
        } else {
            match conc_text.parse::<u64>() {
                Ok(n) if n >= 1 => Some(Some(n)),
                _ => {
                    invalid = true;
                    None
                }
            }
        };
        (model, concurrency, invalid)
    }

    /// TS `canSubmit` for the amend sheet: disabled until at least one
    /// field contributes a change (an empty delta is a 422 by contract).
    fn workflow_amend_can_submit(&self, cx: &Context<Self>) -> bool {
        let Some(form) = &self.workflow_amend_form else {
            return false;
        };
        let (model, concurrency, invalid) = self.workflow_amend_state(cx);
        let empty =
            model.is_none() && concurrency.is_none() && !form.clear_concurrency && !form.clear_model;
        !empty && !invalid
    }

    /// The amend form's submit: build the three-state body (the key's
    /// PRESENCE is the wire discriminator) and dispatch.
    fn submit_workflow_amend_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &self.workflow_amend_form else {
            return;
        };
        let (model, concurrency, _) = self.workflow_amend_state(cx);
        let body = protocol::workflow::AmendRunSettingsBody {
            subagent_model: model,
            max_concurrency: concurrency,
        };
        let (run_id, session_id, instance_id) = (
            form.run_id.clone(),
            form.session_id.clone(),
            form.instance_id.clone(),
        );
        self.runtime.send(UiCommand::AmendWorkflowRun {
            instance_id,
            session_id,
            run_id,
            body,
        });
        // TS closes the sheet on submit BEFORE the await
        // (WorkflowRunDetail.tsx): the page's notice banner — which the open
        // form was REPLACING — then carries the outcome (a refusal explains
        // itself there; keeping the form open on failure hid that banner).
        self.workflow_amend_form = None;
        self.workflow_form_subs.clear();
        cx.notify();
    }

    /// The start sheet (TS `StartSheet`): the typed form when the workflow
    /// declares its args, the free-form JSON textarea otherwise.
    pub fn render_workflow_start_form(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let Some(form) = &self.workflow_start_form else {
            return div().into_any_element();
        };
        let can_submit = self.workflow_start_can_submit(cx);
        let mut column = div()
            .id("workflow-start-form")
            .test_support()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_x_hidden();
        column = column.child(form_header(
            &form.name,
            "workflow-form-back",
            theme,
            cx,
        ));

        let mut body = div().p_4().flex().flex_col().gap_3().max_w(rems(46.));
        if let Some(decls) = form.decls() {
            let (values, json_texts) = self.workflow_start_values(form, cx);
            let report = typed_args_report(&decls, &values, &json_texts);
            for f in &form.fields {
                body = body.child(start_field_row(f, theme, cx));
            }
            if let Some(error) = report.error {
                body = body.child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.danger_foreground)
                        .child(error),
                );
            }
        } else if let Some(area) = &form.free_json {
            let parsed = parse_args_input(&area.read(cx).value().to_string());
            body = body.child(
                div().flex().flex_col().gap_1().child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.foreground)
                        .child(t!("zconfig.workflowStartArgs").into_owned()),
                )
                .child(
                    div().text_size(px(10.)).text_color(theme.muted_foreground).child(
                        parsed
                            .error
                            .clone()
                            .unwrap_or_else(|| t!("zconfig.workflowStartArgsHint").into_owned()),
                    ),
                )
                .child(Textarea::new(area).small()),
            );
        }
        let wb = cx.entity();
        column
            .child(
                div()
                    .id("workflow-start-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body.child(
                        Button::new("workflow-start-submit")
                            .primary()
                            .w_full()
                            .label(t!("zconfig.workflowStart").into_owned())
                            .disabled(!can_submit)
                            .on_click(move |_, _, cx| {
                                wb.update(cx, |this, cx| this.submit_workflow_start_form(cx));
                            }),
                    )),
            )
            .into_any_element()
    }

    /// The amend sheet (TS `AmendSheet`): submit-only, three-state fields.
    pub fn render_workflow_amend_form(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let Some(form) = &self.workflow_amend_form else {
            return div().into_any_element();
        };
        let can_submit = self.workflow_amend_can_submit(cx);
        let (_, _, invalid) = self.workflow_amend_state(cx);
        let amend_busy = self
            .snap
            .workflows_page
            .as_ref()
            .and_then(|p| p.run.as_ref())
            .is_some_and(|r| r.amend_busy);
        let mut column = div()
            .id("workflow-amend-form")
            .test_support()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_x_hidden();
        column = column.child(form_header(
            &t!("zconfig.workflowConfigure").into_owned(),
            "workflow-form-back",
            theme,
            cx,
        ));

        let wb2 = cx.entity();
        let wb3 = cx.entity();
        let wb4 = cx.entity();
        let mut body = div().p_4().flex().flex_col().gap_3().max_w(rems(46.));
        body = body
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(theme.muted_foreground)
                    .child(t!("zconfig.workflowAmendHint").into_owned()),
            )
            // Subagent model: empty = keep, clear = revert to default. Arming
            // the clear empties the text; typing un-arms the clear (TS
            // semantics — the two states are mutually coherent).
            .child(
                div().flex().flex_col().gap_1()
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.foreground)
                            .child(t!("zconfig.workflowSubagentModel").into_owned()),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(theme.muted_foreground)
                            .child(t!("zconfig.workflowSubagentModelHint").into_owned()),
                    )
                    .child(
                        div()
                            .font_family(theme.mono_font_family.clone())
                            .child(Input::new(&form.model).small().disabled(form.clear_model)),
                    )
                    .child(
                        div().flex().items_center().gap_2().child(
                            Switch::new("workflow-amend-clear-model").checked(form.clear_model).on_click(
                                move |_, window, cx| {
                                    wb3.update(cx, |this, cx| {
                                        if let Some(form) = &mut this.workflow_amend_form {
                                            form.clear_model = !form.clear_model;
                                            if form.clear_model {
                                                form.model.update(cx, |s, cx| {
                                                    s.set_value(String::new(), window, cx)
                                                });
                                            }
                                        }
                                        cx.notify();
                                    });
                                },
                            ),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(theme.muted_foreground)
                                .child(t!("zconfig.workflowClearModel").into_owned()),
                        ),
                    ),
            )
            // Max concurrency: the same three states, integer ≥ 1.
            .child(
                div().flex().flex_col().gap_1()
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.foreground)
                            .child(t!("zconfig.workflowMaxConcurrency").into_owned()),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(theme.muted_foreground)
                            .child(if invalid {
                                t!("zconfig.workflowConcurrencyInvalid").into_owned()
                            } else {
                                t!("zconfig.workflowConcurrencyHint").into_owned()
                            }),
                    )
                    .child(
                        Input::new(&form.concurrency)
                            .small()
                            .disabled(form.clear_concurrency),
                    )
                    .child(
                        div().flex().items_center().gap_2().child(
                            Switch::new("workflow-amend-clear-concurrency")
                                .checked(form.clear_concurrency)
                                .on_click(move |_, window, cx| {
                                    wb4.update(cx, |this, cx| {
                                        if let Some(form) = &mut this.workflow_amend_form {
                                            form.clear_concurrency = !form.clear_concurrency;
                                            if form.clear_concurrency {
                                                form.concurrency.update(cx, |s, cx| {
                                                    s.set_value(String::new(), window, cx)
                                                });
                                            }
                                        }
                                        cx.notify();
                                    });
                                }),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(theme.muted_foreground)
                                .child(t!("zconfig.workflowClearConcurrency").into_owned()),
                        ),
                    ),
            );
        column
            .child(
                div()
                    .id("workflow-amend-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body.child(
                        Button::new("workflow-amend-submit")
                            .primary()
                            .w_full()
                            .label(t!("common.save").into_owned())
                            .disabled(!can_submit || amend_busy)
                            .on_click(move |_, _, cx| {
                                wb2.update(cx, |this, cx| this.submit_workflow_amend_form(cx));
                            }),
                    )),
            )
            .into_any_element()
    }

    /// The WorkflowGate sheet (J2): the machine-level switch's three
    /// positions. Overlay — later siblings paint above, so the workbench
    /// mounts it last.
    pub fn render_workflow_gate_sheet(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let gate = self.snap.workflow_gate.as_ref();
        let position = gate_position(gate);

        // The remote verdict is only readable while no override shadows it;
        // an unresolved fetch reports "unknown" (fail-closed), never "off".
        let remote_state = match gate {
            None => t!("zconfig.workflowGateStateUnknown").into_owned(),
            Some(g) if g.source == "override" => {
                t!("zconfig.workflowGateStateUnknown").into_owned()
            }
            Some(g) if g.enabled => t!("zconfig.workflowGateStateOn").into_owned(),
            Some(g) if g.mode == "unknown" => t!("zconfig.workflowGateStateUnknown").into_owned(),
            Some(_) => t!("zconfig.workflowGateStateOff").into_owned(),
        };

        let options: [(GatePosition, WorkflowGateModeSetting, String, String); 3] = [
            (
                GatePosition::Auto,
                WorkflowGateModeSetting::Auto,
                t!("zconfig.workflowGateAuto").into_owned(),
                fill(
                    &t!("zconfig.workflowGateRemote").into_owned(),
                    &[("state", remote_state.as_str())],
                ),
            ),
            (
                GatePosition::On,
                WorkflowGateModeSetting::AlwaysOn,
                t!("zconfig.workflowGateOn").into_owned(),
                t!("zconfig.workflowGateScope").into_owned(),
            ),
            (
                GatePosition::Off,
                WorkflowGateModeSetting::Disabled,
                t!("zconfig.workflowGateOff").into_owned(),
                t!("zconfig.workflowGateScope").into_owned(),
            ),
        ];

        // The write addresses the ACTIVE TAB's bridge — the per-instance
        // settings mount is the only writable one. Without one the sheet
        // still explains the positions; the rows simply stay the current
        // position (nothing to write through).
        let write_target = self.gate_target_instance();
        let gate_busy = self.snap.workflow_gate_busy;
        let runtime = self.runtime.clone();
        let wb = cx.entity();
        let mut card = div()
            .id("workflow-gate-card")
            .flex()
            .flex_col()
            .gap_1()
            .w_full()
            .max_w(px(420.))
            .p_4()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.title_bar)
            // The card swallows backdrop clicks (the sheet's own close).
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .pb_2()
                    .text_size(px(13.))
                    .text_color(theme.foreground)
                    .child(t!("zconfig.workflowGate").into_owned()),
            );
        for (pos, mode, label, hint) in options {
            let active = pos == position;
            let wb2 = wb.clone();
            let target = write_target.clone();
            let runtime = runtime.clone();
            card = card.child(
                div()
                    .id(SharedString::from(format!(
                        "workflow-gate-opt-{}",
                        match pos {
                            GatePosition::Auto => "auto",
                            GatePosition::On => "on",
                            GatePosition::Off => "off",
                        }
                    )))
                    .test_support()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py(px(10.))
                    .rounded_md()
                    .bg(if active {
                        theme.secondary
                    } else {
                        gpui::transparent_black()
                    })
                    .when(!gate_busy, |this| {
                        this.cursor_pointer()
                            .hover(|this| this.bg(theme.secondary.opacity(0.6)))
                    })
                    .when(!gate_busy, move |this| {
                        this.on_click(move |_, _, cx| {
                        wb2.update(cx, |this, cx| {
                            this.workflow_gate_sheet = false;
                            cx.notify();
                        });
                        // Only a POSITION CHANGE writes (the TS `if
                        // (!active)` guard) — re-picking the current
                        // position is a no-op, not an empty override.
                            if !active {
                                if let Some(instance_id) = target.clone() {
                                    runtime.send(UiCommand::SetWorkflowGate { instance_id, mode });
                                }
                            }
                        })
                    })
                    .when(gate_busy, |this| this.opacity(0.6))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(if active {
                                        theme.foreground
                                    } else {
                                        theme.muted_foreground
                                    })
                                    .truncate()
                                    .child(label),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(theme.muted_foreground)
                                    .truncate()
                                    .child(hint),
                            ),
                    )
                    .when(active, |this| {
                        this.child(
                            div()
                                .size_3()
                                .flex_none()
                                .text_color(theme.muted_foreground)
                                .child(Icon::new(IconName::Check)),
                        )
                    }),
            );
        }
        if write_target.is_none() {
            card = card.child(
                div()
                    .pt_2()
                    .text_size(px(10.))
                    .text_color(theme.warning_foreground)
                    .child(t!("zconfig.workflowNeedInstance").into_owned()),
            );
        }
        div()
            .id("workflow-gate-sheet")
            .test_support()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .p_6()
            .bg(gpui::black().opacity(0.5))
            .on_click({
                let wb = wb.clone();
                move |_, _, cx| {
                    wb.update(cx, |this, cx| {
                        this.workflow_gate_sheet = false;
                        cx.notify();
                    });
                }
            })
            .child(card)
            .into_any_element()
    }

    /// The instance a gate write addresses: the active tab's bridge (the
    /// per-instance settings mount is the only writable one). Read from the
    /// SNAPSHOT-level field, not the page's `action_instance` — the sheet's
    /// only entry lives in the config panel, which renders with the
    /// workflows page closed, so the page-scoped field is `None` on every
    /// real open (the TS store writes the active session's instance).
    fn gate_target_instance(&self) -> Option<String> {
        self.snap.active_instance.clone()
    }
}

/// The shared form header (back + title) — the TS sheets' frame top.
fn form_header(
    title: &str,
    back_id: &'static str,
    theme: &component::Theme,
    cx: &Context<Workbench>,
) -> gpui::Div {
    let wb = cx.entity();
    div()
        .flex()
        .items_center()
        .gap_1()
        .h_9()
        .px_3()
        .flex_none()
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.title_bar)
        .child(
            Button::new(back_id)
                .ghost()
                .xsmall()
                .icon(IconName::ArrowLeft)
                .tooltip("Back")
                .on_click(move |_, _, cx| {
                    wb.update(cx, |this, cx| this.close_workflow_form(cx));
                }),
        )
        .child(
            div()
                .text_size(px(13.))
                .text_color(theme.foreground)
                .truncate()
                .min_w_0()
                .child(title.to_string()),
        )
}

/// One typed start-form field (TS WorkflowArgsForm's rows): label + required
/// marker + description, then the declared type's control.
fn start_field_row(
    f: &WorkflowStartField,
    theme: &component::Theme,
    cx: &Context<Workbench>,
) -> AnyElement {
    let field_type = f.decl.r#type.clone().unwrap_or_else(|| "string".to_string());
    let mut column = div().flex().flex_col().gap_1();
    let mut label = f.name.clone();
    if f.decl.required.unwrap_or(false) {
        label.push_str(" *");
    }
    column = column.child(
        div()
            .text_size(px(11.))
            .text_color(theme.foreground)
            .child(label),
    );
    if let Some(description) = f.decl.description.clone() {
        column = column.child(
            div()
                .text_size(px(10.))
                .text_color(theme.muted_foreground)
                .child(description),
        );
    }
    match field_type.as_str() {
        "boolean" => {
            let wb = cx.entity();
            let name = f.name.clone();
            column
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "wf-start-bool-{}",
                            slug_run_id(&f.name)
                        )))
                        .test_support()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            Switch::new(SharedString::from(format!(
                                "wf-start-switch-{}",
                                slug_run_id(&f.name)
                            )))
                            .checked(f.bool_value)
                            .on_click(move |_, _, cx| {
                                let name = name.clone();
                                wb.update(cx, |this, cx| {
                                    this.toggle_workflow_start_bool(&name, cx);
                                });
                            }),
                        ),
                )
                .into_any_element()
        }
        "string" | "number" => match &f.input {
            Some(input) => column
                .child(
                    div()
                        .font_family(if field_type == "string" {
                            theme.mono_font_family.clone()
                        } else {
                            theme.font_family.clone()
                        })
                        .child(Input::new(input).small()),
                )
                .into_any_element(),
            None => column.into_any_element(),
        },
        _ => match &f.area {
            Some(area) => column.child(Textarea::new(area).small()).into_any_element(),
            None => column.into_any_element(),
        },
    }
}

// ---------------------------------------------------------------------------
// Headless UI integration tests (batch J) — the chip_tests paradigm:
// explicit imports, an inert runtime, a real Workbench in a test window,
// element-id assertions and real clicks.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod workflows_tests {
    use super::WorkflowStartForm;
    use crate::runtime::{
        Phase, ProtocolRuntime, UiCommand, WorkflowDetailView, WorkflowRunView,
        WorkflowsPageView, WorkbenchSnapshot,
    };
    use crate::workbench::Workbench;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{
        px, App, AppContext as _, Bounds, Entity, Point, Size, TestAppContext, WindowBounds,
        WindowOptions,
    };
    use protocol::settings::{GateOverride, WorkflowGateBlock, WorkflowGateModeSetting};
    use protocol::workflow::{
        ConversationRunSummary, WorkflowDetailResponse, WorkflowEntry, WorkflowHubGroup,
        WorkflowOverviewResponse, WorkflowRunEvent, WorkflowRunRow, WorkflowScope,
    };
    use std::collections::BTreeMap;

    fn run_row(run_id: &str, name: &str, status: &str, session: Option<&str>) -> WorkflowRunRow {
        WorkflowRunRow {
            run_id: run_id.to_string(),
            name: Some(name.to_string()),
            status: status.to_string(),
            stop_reason: None,
            created_at: Some(1),
            updated_at: Some(1),
            spent_tokens: Some(1200),
            parent_session_id: None,
            acp_session_id: session.map(str::to_string),
            tool_call_id: None,
            args: None,
            cwd: None,
            artifacts: None,
            resumed_from: None,
            superseded_by: None,
            owner_instance_id: Some("inst-a".to_string()),
        }
    }

    fn overview_fixture() -> WorkflowOverviewResponse {
        let mut last_runs = BTreeMap::new();
        last_runs.insert("deploy".to_string(), run_row("run-9", "deploy", "completed", Some("s9")));
        WorkflowOverviewResponse {
            ok: true,
            groups: vec![WorkflowHubGroup {
                instance_id: "inst-a".to_string(),
                scope: WorkflowScope::Global,
                workspace: String::new(),
                workflows: vec![WorkflowEntry {
                    name: "deploy".to_string(),
                    description: Some("Deploy the app".to_string()),
                    scope: Some("global".to_string()),
                }],
                invalid: Vec::new(),
                last_runs,
                error: None,
            }],
            active_runs: vec![run_row("run-1", "deploy", "running", Some("s1"))],
            recent_runs: vec![run_row("run-2", "deploy", "completed", Some("s2"))],
        }
    }

    fn page_view(overview: Option<WorkflowOverviewResponse>) -> WorkflowsPageView {
        WorkflowsPageView {
            loading: false,
            error: None,
            overview,
            detail: None,
            run: None,
            busy: false,
            action_instance: Some("inst-a".to_string()),
        }
    }

    fn detail_view(meta: BTreeMap<String, serde_json::Value>) -> WorkflowDetailView {
        WorkflowDetailView {
            scope: WorkflowScope::Global,
            name: "deploy".to_string(),
            instance_id: "inst-a".to_string(),
            loading: false,
            detail: Some(WorkflowDetailResponse {
                ok: true,
                name: "deploy".to_string(),
                path: Some("/tmp/deploy.js".to_string()),
                scope: WorkflowScope::Global,
                meta: Some(meta),
                script: Some("export default {}".to_string()),
            }),
            runs: Some(vec![run_row("run-3", "deploy", "cancelled", Some("s3"))]),
            summaries: BTreeMap::from([(
                "s3".to_string(),
                vec![ConversationRunSummary {
                    run_id: "run-3".to_string(),
                    tool_call_id: None,
                    label: None,
                    updated_at: Some(2),
                    status: "cancelled".to_string(),
                    stop_reason: None,
                    resumed_from: None,
                    superseded_by: None,
                    failure_code: None,
                    failure_message: None,
                    resumable: true,
                }],
            )])
            .into_iter()
            .collect(),
        }
    }

    fn run_view(events: Vec<WorkflowRunEvent>, has_more: bool) -> WorkflowRunView {
        WorkflowRunView {
            instance_id: "inst-a".to_string(),
            session_id: "s1".to_string(),
            run_id: "run-1".to_string(),
            name: Some("deploy".to_string()),
            summary: Some(ConversationRunSummary {
                run_id: "run-1".to_string(),
                tool_call_id: None,
                label: None,
                updated_at: Some(3),
                status: "running".to_string(),
                stop_reason: None,
                resumed_from: None,
                superseded_by: None,
                failure_code: None,
                failure_message: None,
                resumable: false,
            }),
            events: Some(events),
            events_busy: false,
            has_more,
            session_dead: false,
            active: true,
            amend_busy: false,
        }
    }

    fn event(sequence: u64, kind: &str, payload: serde_json::Value) -> WorkflowRunEvent {
        WorkflowRunEvent {
            sequence,
            r#type: kind.to_string(),
            payload: payload
                .as_object()
                .cloned()
                .unwrap_or_default(),
            truncated: None,
        }
    }

    fn open_workbench(
        cx: &mut TestAppContext,
        runtime: ProtocolRuntime,
        snap: WorkbenchSnapshot,
    ) -> (gpui_kit::AnyWindowHandle, Entity<Workbench>) {
        cx.update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: Size {
                            width: px(800.),
                            height: px(600.),
                        },
                    })),
                    ..Default::default()
                },
                cx,
                |window, cx| {
                    let runtime = runtime.clone();
                    cx.new(|cx| Workbench::new(window, cx, runtime.clone(), snap))
                },
            )
            .expect("open test window")
        })
    }

    /// The overview paints the group card with its last-run badge, the live
    /// zone's row is actionable (Stop), and clicking a group card opens
    /// that workflow on the group's own instance.
    #[gpui_kit::test]
    fn workflows_overview_groups_navigate(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let runtime = ProtocolRuntime::inert();
        let mut snap = WorkbenchSnapshot::initial();
        snap.phase = Phase::Connected;
        snap.workflows_page = Some(page_view(Some(overview_fixture())));
        let (handle, _wb) = open_workbench(cx, runtime.clone(), snap);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            // The group card with its last-run badge, the live zone row, and
            // the finished row all render.
            assert!(window.find("wf-group-0-deploy").visible());
            assert!(window.find("wf-badge-active-0").visible());
            assert!(window.find("wf-active-0-stop-run-1").visible());
            assert!(window.find("wf-recent-0-dismiss-run-2").visible());
            // Clicking the card dispatches the detail open on the group's
            // instance (the hub's group annotation, not the connected one).
            window.click("wf-group-0-deploy", cx);
            let sent = runtime.recorded_commands();
            assert!(
                sent.iter().any(|c| matches!(
                    c,
                    UiCommand::OpenWorkflowDetail { scope, name, instance_id }
                        if *scope == WorkflowScope::Global
                            && name == "deploy"
                            && instance_id == "inst-a"
                )),
                "the card click must reach the runtime, got {sent:?}"
            );
        })
        .unwrap();
    }

    /// The gate sheet (J2): three positions render, the current one is
    /// marked, and picking "on" writes `PUT /settings/workflow-gate` with
    /// alwaysOn on the active tab's bridge. Opened the PRODUCTION way — the
    /// config panel row, with the workflows page CLOSED (the dock and the
    /// page are exclusive surfaces) — so the write target must come from
    /// the snapshot-level `active_instance`, not the page's own state.
    #[gpui_kit::test]
    fn workflow_gate_sheet_picks_position(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let runtime = ProtocolRuntime::inert();
        let mut snap = WorkbenchSnapshot::initial();
        snap.phase = Phase::Connected;
        snap.active_instance = Some("inst-a".to_string());
        // A probed verdict with no override: the AUTO position is active.
        snap.workflow_gate = Some(WorkflowGateBlock {
            enabled: false,
            mode: "env".to_string(),
            source: "remote".to_string(),
            r#override: None,
        });
        let (handle, wb) = open_workbench(cx, runtime.clone(), snap);
        wb.update(cx, |wb, cx| {
            wb.workflow_gate_sheet = true;
            cx.notify();
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("workflow-gate-sheet").visible());
            assert!(window.find("workflow-gate-opt-auto").visible());
            assert!(window.find("workflow-gate-opt-on").visible());
            assert!(window.find("workflow-gate-opt-off").visible());
            window.click("workflow-gate-opt-on", cx);
            let sent = runtime.recorded_commands();
            assert!(
                sent.iter().any(|c| matches!(
                    c,
                    UiCommand::SetWorkflowGate { instance_id, mode }
                        if instance_id == "inst-a"
                            && *mode == WorkflowGateModeSetting::AlwaysOn
                )),
                "picking on must write the gate override, got {sent:?}"
            );
            // The override verdict's own spelling maps back to its position.
            let block = WorkflowGateBlock {
                enabled: true,
                mode: "alwaysOn".to_string(),
                source: "override".to_string(),
                r#override: Some(GateOverride::AlwaysOn),
            };
            assert_eq!(
                super::gate_position(Some(&block)),
                super::GatePosition::On
            );
        })
        .unwrap();
    }

    /// The run view renders the journal through the protocol renderer, and
    /// the cursor's "Load more" dispatches the append read.
    #[gpui_kit::test]
    fn workflow_run_events_render_and_paginate(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let runtime = ProtocolRuntime::inert();
        let events = vec![
            event(1, "run-started", serde_json::json!({})),
            event(
                2,
                "node-dispatched",
                serde_json::json!({ "actorName": "impl", "instance": "site@1" }),
            ),
        ];
        let mut snap = WorkbenchSnapshot::initial();
        snap.phase = Phase::Connected;
        let mut view = page_view(None);
        view.run = Some(run_view(events, true));
        snap.workflows_page = Some(view);
        let (handle, _wb) = open_workbench(cx, runtime.clone(), snap);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            // One row per journal event, the live-run header actions, and
            // the cursor's next-page control.
            assert!(window.find("wf-event-1").visible());
            assert!(window.find("wf-event-2").visible());
            assert!(window.find("workflow-run-stop").visible());
            assert!(window.find("workflow-run-amend-open").visible());
            assert!(window.find("workflow-run-load-more").visible());
            window.click("workflow-run-load-more", cx);
            let sent = runtime.recorded_commands();
            assert!(
                sent.iter().any(|c| matches!(c, UiCommand::WorkflowRunLoadMore)),
                "load more must dispatch the append read, got {sent:?}"
            );
        })
        .unwrap();
    }

    /// The start form (typed args): the required field gates the submit,
    /// typing into it arms the start, and the submit carries the validated
    /// args object (booleans always carry a value — unchecked = false).
    #[gpui_kit::test]
    fn workflow_start_form_validates_and_submits(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let runtime = ProtocolRuntime::inert();
        let mut snap = WorkbenchSnapshot::initial();
        snap.phase = Phase::Connected;
        let meta = BTreeMap::from([
            (
                "args".to_string(),
                serde_json::json!({
                    "target": { "type": "string", "required": true },
                    "verbose": { "type": "boolean", "default": true }
                }),
            ),
            (
                "description".to_string(),
                serde_json::json!("Deploy the app"),
            ),
        ]);
        let mut view = page_view(None);
        view.detail = Some(detail_view(meta));
        snap.workflows_page = Some(view);
        let (handle, wb) = open_workbench(cx, runtime.clone(), snap);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            // The Start control opens the typed form.
            window.click("workflow-start-open", cx);
            window.render_frame(cx);
            assert!(window.find("workflow-start-form").visible());
            // A disabled submit does not dispatch (target is required and
            // empty; the boolean pre-filled its declared default).
            window.click("workflow-start-submit", cx);
            assert!(
                !runtime
                    .recorded_commands()
                    .iter()
                    .any(|c| matches!(c, UiCommand::StartWorkflow { .. })),
                "an invalid form must not start anything"
            );
            // Type the way the subscription path would (set_value + notify).
            wb.update(cx, |wb, cx| {
                let form: &WorkflowStartForm =
                    wb.workflow_start_form.as_ref().expect("form open");
                let input = form
                    .fields
                    .iter()
                    .find(|f| f.name == "target")
                    .and_then(|f| f.input.clone())
                    .expect("target input");
                input.update(cx, |s, cx| s.set_value("src/", window, cx));
                cx.notify();
            });
            window.render_frame(cx);
            window.click("workflow-start-submit", cx);
            let sent = runtime.recorded_commands();
            assert!(
                sent.iter().any(|c| matches!(
                    c,
                    UiCommand::StartWorkflow { args, .. }
                        if *args == Some(serde_json::json!({
                            "target": "src/",
                            "verbose": true,
                        }))
                )),
                "the submit must carry the validated args, got {sent:?}"
            );
        })
        .unwrap();
    }
}
