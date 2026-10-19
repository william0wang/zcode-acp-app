//! The Workflows page (batch J, requirements §"workflow parity"): the
//! desktop port of `src/screens/config/WorkflowsPage.tsx` and its run-detail
//! view, over the hub's machine-level `/api/workflow-overview` — the page
//! renders the answer as-is (one group per workspace plus the GLOBAL group,
//! the machine-wide live zone, the finished complement).
//!
//! Three layers, exactly like the TS page stack: the overview list, one
//! workflow (detail + journal history + start), one run. The run page
//! (batch L, `WorkflowRunDetail.tsx`) carries the three tabs — the journal
//! events rendered through `protocol::workflow_run_event_text` and sliced
//! per phase (`protocol::workflow_run_detail`), the artifacts with their
//! per-kind viewer, the workspace nodes with the expandable result — the
//! artifacts and the workspace loading lazily on first open. Actions address
//! the row's own instance — the hub's `ownerInstanceId` annotation, falling
//! back to the connected tab's instance (the TS launch-memory fallback has
//! no desktop analogue: this client keeps no launch memory, so a row without
//! an `acpSessionId` stays read-only).
//!
//! The WorkflowGate sheet (J2) is the machine-level switch's three-position
//! picker (auto / on / off) over `PUT /settings/workflow-gate`.

use std::collections::BTreeMap;
use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    canvas, point, pulsating_between, radians, size, Animation, AnyElement, Bounds, Context,
    Entity, Hsla, PathBuilder, Pixels, Rgba, Window, div, fill as quad_fill, px, rgb,
};
use gpui_kit::component;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _,
};
use gpui_kit::assets::IconName as KitIconName;
use gpui_kit::*;
use gpui_kit::TestSupportExt as _;
use protocol::settings::{GateOverride, WorkflowGateBlock, WorkflowGateModeSetting};
use protocol::workflow::{
    ConversationRunSummary, WorkflowArgDeclaration, WorkflowArtifact, WorkflowDetailResponse,
    WorkflowHubGroup, WorkflowRunEvent, WorkflowRunRow, WorkflowScope, WorkflowWorkspaceNode,
};
use protocol::workflow_args::{
    args_declaration, args_initial_values, parse_args_input, typed_args_report,
};
use protocol::workflow_run_detail::{
    args_text, artifact_is_preset, artifact_meta, artifact_spec, board_columns, chart_fmt,
    chart_model, fmt_duration, key_value_cards, latest_version, metrics_tiles, node_key,
    node_ref_line, node_result_body, node_summary_text, op_family, phase_paints, phase_rows,
    phase_slices, pretty_json, table_model, ArtifactDataItem, ChartKind, KeyValueCard,
    NodeResultBody, OpFamily, PhaseSlice, PhaseSlices,
};
use protocol::workflow_run_event_text::{
    describe_run_event, describe_run_event_with, node_state_word, newest_first, reduce_slice,
    status_word, ActorLine, DescribeOptions, NodeAgg, NodeState, RunEventTone, SliceFold,
    Translate,
};
use rust_i18n::t;

use crate::i18n::fill;
use crate::runtime::{
    UiCommand, WorkflowArtifactViewerView, WorkflowDetailView, WorkflowLaunchRef,
    WorkflowNodeResultView, WorkflowRunTab, WorkflowRunView, WorkflowsPageView,
};
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
        // rust-i18n answers a miss with the key itself; the protocol's
        // i18next contract expects the caller's defaultValue to surface
        // (statusWord's unknown-token passthrough shows raw tokens).
        if text == key {
            if let Some((_, fallback)) = values.iter().find(|(k, _)| *k == "defaultValue") {
                return fallback.clone();
            }
        }
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

    // The header's back target follows the layer. With an artifact viewer
    // open, back closes the viewer first (the TS back-gesture ladder:
    // overlay, then page) and the title is the artifact's.
    let (title, back): (String, UiCommand) = if let Some(run) = &page.run {
        if let Some(viewer) = &run.artifact {
            (
                viewer
                    .artifact
                    .title
                    .clone()
                    .unwrap_or_else(|| viewer.artifact.id.clone()),
                UiCommand::WorkflowRunCloseArtifact,
            )
        } else {
            (
                run.name
                    .clone()
                    .unwrap_or_else(|| t!("zconfig.workflows").into_owned()),
                UiCommand::CloseWorkflowRun,
            )
        }
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
                        .text_size(px(12.))
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
                .text_size(px(13.))
                .text_color(theme.foreground)
                .child(title.to_string()),
        )
        .child(
            div()
                .pt_2()
                .text_size(px(12.))
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
                .text_size(px(13.))
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
        .text_size(px(12.))
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
                page.launches.get(&r.run_id),
                page.session_runs.get(&r.run_id).map(|s| s.as_str()),
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
                page.launches.get(&r.run_id),
                // TS RecentRunsBlock has no conversationRuns join — the
                // third ladder is the ACTIVE zone's stop affordance only.
                None,
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
/// Detail/Stop (active) or Detail/dismiss (recent) controls. The session
/// ladder is the TS one: launch memory → the row's bridge-joined
/// `acpSessionId` → (active zone only) the conversationRuns join
/// `session_run` — a run that flies in the CURRENT session is actionable
/// even when neither memory nor the bridge could place it; the instance
/// ladder adds the hub's owner annotation ahead of launch memory, with the
/// connected tab's instance last. Recent rows additionally carry the
/// stamp · tokens · artifact-count chips line (TS RecentRunsBlock).
fn render_run_zone_row(
    r: &WorkflowRunRow,
    index: usize,
    zone: &str,
    action_instance: Option<&str>,
    busy: bool,
    launch: Option<&WorkflowLaunchRef>,
    session_run: Option<&str>,
    wb: &Workbench,
    cx: &Context<Workbench>,
) -> AnyElement {
    let theme = cx.theme();
    let label = r
        .name
        .clone()
        .unwrap_or_else(|| r.run_id.chars().take(12).collect());
    let row_instance: Option<String> = r
        .owner_instance_id
        .clone()
        .or_else(|| launch.map(|l| l.instance_id.clone()))
        .or_else(|| action_instance.map(str::to_string));
    let row_session: Option<String> = launch
        .map(|l| l.acp_session_id.clone())
        .or_else(|| r.acp_session_id.clone())
        .or_else(|| session_run.map(str::to_string));
    let key = format!("{zone}-{index}");
    let slug_run = slug_run_id(&r.run_id);

    let mut card = div().flex().flex_col().px_4().py_3().gap_2();
    let mut line = div().flex().items_center().gap_2().min_w_0().child(
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .min_w_0()
                    .text_size(px(13.))
                    .text_color(theme.foreground)
                    .truncate()
                    .child(label.clone()),
            )
            // The finished complement's chips line (TS shape): stamp ·
            // tokens · artifact count, only the parts the row carries.
            .when(zone == "recent" && !run_meta_line(r).is_empty(), |this| {
                this.child(
                    div()
                        .id(SharedString::from(format!("wf-{key}-meta")))
                        .test_support()
                        .min_w_0()
                        .text_size(px(10.))
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(run_meta_line(r)),
                )
            }),
    );
    // TS shape: the ACTIVE row's stamp rides the label line; the RECENT
    // row's stamp lives inside its chips line instead.
    if let Some(at) = r.updated_at.filter(|_| zone != "recent") {
        line = line.child(
            div()
                .flex_none()
                .text_size(px(10.))
                .text_color(theme.muted_foreground)
                .child(fmt_stamp_utc(at)),
        );
    }
    line = line.child(status_badge(Some(&r.status), &key, theme));

    // Dismissal hides one settled run from every bridge-served list; it
    // needs only an INSTANCE to address (TS `dismiss`'s ladder stops at
    // `iid`), so a recent row without a session still dismisses.
    if zone == "recent" {
        if let Some(instance_id) = row_instance.clone() {
            let runtime = wb.runtime.clone();
            let run_id = r.run_id.clone();
            line = line.child(
                Button::new(SharedString::from(format!("wf-{key}-dismiss-{slug_run}")))
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
    }
    card = card.child(line);

    // Detail/Stop need BOTH halves of the address (an openable session on
    // an instance that can act on it).
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
                            instance_id: Some(instance_stop.clone()),
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
                    .text_size(px(12.))
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
                            .text_size(px(13.))
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
    // description, whenToUse, the file path, and the meta edit entry.
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
    let meta_loaded = detail.detail.is_some();
    if let Some(d) = &detail.detail {
        card = card
            .when_some(meta_string(d, "description"), |this, text| {
                this.child(
                    div()
                        .px_4()
                        .py_2()
                        .text_size(px(12.))
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
                        .w_full()
                        .font_family(theme.mono_font_family.clone())
                        .text_size(px(10.))
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(path),
                )
            });
    }
    // Edit meta (TS `MetaSheet`): exactly the two prose fields; the button
    // needs the loaded detail (the merge base of the whole-meta replace).
    if meta_loaded {
        let wb_handle = cx.entity();
        card = card.child(
            div()
                .px_4()
                .py_2()
                .flex_none()
                .child(
                    Button::new("workflow-meta-edit-open")
                        .secondary()
                        .outline()
                        .xsmall()
                        .label(t!("zconfig.workflowEditDescription").into_owned())
                        .on_click(move |_, window, cx| {
                            wb_handle.update(cx, |this, cx| {
                                this.open_workflow_meta_form(window, cx);
                            });
                        }),
                ),
        );
    }
    list = list.child(card);

    // The start + scope-change + delete controls. The two scope moves are
    // BOTH present in the product, one direction per scope: project →
    // global rides the chat-prefill path (no dedicated REST), global →
    // project the REST move. Delete arms an inline two-step confirm (a
    // mislabeled tap must not delete a saved workflow).
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
    let mut scope_row = div().pt_2().flex().gap_2();
    if detail.scope == WorkflowScope::Project {
        // Promote-to-global rides the chat-prefill path (no dedicated REST).
        let runtime = wb.runtime.clone();
        let name = detail.name.clone();
        scope_row = scope_row.child(
            Button::new("workflow-promote-global")
                .secondary()
                .outline()
                .small()
                .flex_1()
                .icon(IconName::ArrowUp)
                .label(t!("zconfig.workflowPromoteGlobal").into_owned())
                .on_click(move |_, _, _| {
                    runtime.send(UiCommand::WorkflowPromoteGlobal { name: name.clone() });
                }),
        );
    } else {
        // global → this project: the REST move (TS `moveToProject`).
        let runtime = wb.runtime.clone();
        let (scope, name, instance_id) =
            (detail.scope, detail.name.clone(), detail.instance_id.clone());
        scope_row = scope_row.child(
            Button::new("workflow-move-project")
                .secondary()
                .outline()
                .small()
                .flex_1()
                .icon(IconName::ArrowDown)
                .label(t!("zconfig.workflowMoveToProject").into_owned())
                .disabled(page.busy)
                .on_click(move |_, _, _| {
                    runtime.send(UiCommand::MoveWorkflowToProject {
                        scope,
                        name: name.clone(),
                        instance_id: instance_id.clone(),
                    });
                }),
        );
    }
    if wb.workflow_delete_confirm {
        let wb_cancel = cx.entity();
        let runtime = wb.runtime.clone();
        let (scope, name, instance_id) =
            (detail.scope, detail.name.clone(), detail.instance_id.clone());
        scope_row = scope_row
            .child(
                Button::new("workflow-delete-cancel")
                    .ghost()
                    .xsmall()
                    .label(t!("common.cancel").into_owned())
                    .on_click(move |_, _, cx| {
                        wb_cancel.update(cx, |this, cx| this.cancel_workflow_delete(cx));
                    }),
            )
            .child(
                Button::new("workflow-delete-confirm")
                    .danger()
                    .small()
                    .flex_1()
                    .label(t!("zconfig.workflowConfirmDelete").into_owned())
                    .disabled(page.busy)
                    .on_click(move |_, _, _| {
                        runtime.send(UiCommand::DeleteWorkflow {
                            scope,
                            name: name.clone(),
                            instance_id: instance_id.clone(),
                        });
                    }),
            );
    } else {
        let wb_arm = cx.entity();
        scope_row = scope_row.child(
            Button::new("workflow-delete-arm")
                .secondary()
                .outline()
                .small()
                .flex_1()
                .icon(KitIconName::Trash)
                .label(t!("zconfig.workflowDelete").into_owned())
                .on_click(move |_, _, cx| {
                    wb_arm.update(cx, |this, cx| this.arm_workflow_delete(cx));
                }),
        );
    }
    controls = controls.child(scope_row);
    list = list.child(controls);

    // The script: read-only, folded behind an explicit Show/Hide (TS
    // `scriptOpen`) — the app never edits the script.
    if let Some(script) = detail
        .detail
        .as_ref()
        .and_then(|d| d.script.as_ref())
        .filter(|s| !s.is_empty())
    {
        let wb_toggle = cx.entity();
        let open = wb.workflow_script_open;
        let mut block = rows_card(theme).child(block_title(&t!("zconfig.workflowScript"), theme));
        block = block.child(
            Button::new("workflow-script-toggle")
                .ghost()
                .xsmall()
                .w_full()
                .text_left()
                .label(if open {
                    t!("zconfig.workflowHideScript").into_owned()
                } else {
                    t!("zconfig.workflowShowScript").into_owned()
                })
                .on_click(move |_, _, cx| {
                    wb_toggle.update(cx, |this, cx| this.toggle_workflow_script(cx));
                }),
        );
        if open {
            block = block.child(
                div()
                    .px_4()
                    .pb_3()
                    .min_w_0()
                    .child(pre_block(
                        "workflow-script-body",
                        script,
                        theme.muted_foreground,
                        theme,
                    )),
            );
        }
        list = list.child(block);
    }

    // The run history: rows with the summary-joined status/resumable
    // verdicts; the session ladder adds launch memory ahead of the
    // bridge-joined alias, so this app's own launches stay actionable.
    let mut runs_block = rows_card(theme).child(block_title(&t!("zconfig.workflowRunsTitle"), theme));
    match &detail.runs {
        None => {}
        Some(runs) if runs.is_empty() => {
            runs_block = runs_block.child(
                div()
                    .px_4()
                    .pb_4()
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child(t!("zconfig.workflowRunsEmpty").into_owned()),
            );
        }
        Some(runs) => {
            // Clear finished (TS `clearFinished`): one dismiss-batch POST
            // for every settled row — visible only while at least one
            // exists (an all-active history hides it, an empty one has no
            // block to clear).
            let finished: Vec<String> = runs
                .iter()
                .filter(|r| !run_is_active(&r.status))
                .map(|r| r.run_id.clone())
                .collect();
            if !finished.is_empty() {
                let runtime = wb.runtime.clone();
                let instance_id = detail.instance_id.clone();
                runs_block = runs_block.child(
                    Button::new("wf-clear-finished")
                        .ghost()
                        .xsmall()
                        .w_full()
                        .text_left()
                        .label(t!("zconfig.workflowClearFinished").into_owned())
                        .disabled(page.busy)
                        .on_click(move |_, _, _| {
                            runtime.send(UiCommand::DismissWorkflowRuns {
                                instance_id: instance_id.clone(),
                                run_ids: finished.clone(),
                            });
                        }),
                );
            }
            for r in runs {
                runs_block = runs_block.child(render_detail_run_row(
                    r,
                    detail,
                    page.launches.get(&r.run_id),
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

/// The whole-meta merge behind the edit form (TS `saveMeta`): the write
/// REPLACES the whole meta, so the untouched `rest` of the loaded meta
/// rides back in — with the OLD `whenToUse` stripped first (an empty new
/// value drops the key entirely, and the spread would silently carry the
/// stale one back). Description is required non-empty upstream.
pub(crate) fn merge_workflow_meta(
    existing: Option<&BTreeMap<String, serde_json::Value>>,
    description: &str,
    when_to_use: &str,
) -> Option<serde_json::Value> {
    let description = description.trim();
    if description.is_empty() {
        return None;
    }
    let mut next = serde_json::Map::new();
    if let Some(existing) = existing {
        for (k, v) in existing {
            if k != "whenToUse" && k != "description" {
                next.insert(k.clone(), v.clone());
            }
        }
    }
    next.insert("description".to_string(), serde_json::Value::String(description.to_string()));
    let when_to_use = when_to_use.trim();
    if !when_to_use.is_empty() {
        next.insert("whenToUse".to_string(), serde_json::Value::String(when_to_use.to_string()));
    }
    Some(serde_json::Value::Object(next))
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
    launch: Option<&WorkflowLaunchRef>,
    busy: bool,
    wb: &Workbench,
    cx: &Context<Workbench>,
) -> AnyElement {
    let theme = cx.theme();
    // The session ladder (TS): launch memory wins for runs this app started
    // (it knows the owning instance), then the bridge-joined alias (bridge
    // 0.57.0); the instance falls back to the page's own — a history row
    // carries no owner annotation.
    let row_session: Option<String> = launch
        .map(|l| l.acp_session_id.clone())
        .or_else(|| r.acp_session_id.clone());
    let row_instance: String = launch
        .map(|l| l.instance_id.clone())
        .unwrap_or_else(|| detail.instance_id.clone());
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
                        .text_size(px(13.))
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
// The run body (TS WorkflowRunDetail): header card, tab strip, one tab
// ---------------------------------------------------------------------------

fn render_run_body(
    wb: &Workbench,
    run: &WorkflowRunView,
    page: &WorkflowsPageView,
    cx: &Context<Workbench>,
) -> AnyElement {
    let theme = cx.theme();
    // The artifact viewer replaces the whole body while open (the TS
    // ViewerShell is a full-screen overlay); the page header's back closes
    // it.
    if let Some(viewer) = &run.artifact {
        return render_artifact_viewer(wb, viewer, cx);
    }
    let mut list = div()
        .id("workflow-run-list")
        .flex_1()
        .min_h_0()
        .min_w_0()
        .overflow_y_scroll()
        .overflow_x_hidden()
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
                .text_size(px(12.))
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
                        instance_id: Some(instance_id.clone()),
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

    // The tab strip (TS `tabs`): Progress / Artifacts / Workspace — the
    // two data tabs load lazily on their first open.
    list = list.child(render_run_tabs(run, wb, theme));

    match run.tab {
        WorkflowRunTab::Events => list = render_run_events_tab(list, wb, run, cx),
        WorkflowRunTab::Artifacts => list = render_run_artifacts_tab(list, wb, run, cx),
        WorkflowRunTab::Workspace => list = render_run_workspace_tab(list, wb, run, cx),
    }
    list.into_any_element()
}

/// The run page's tab pills (the usage page's segmented control shape).
fn render_run_tabs(run: &WorkflowRunView, wb: &Workbench, theme: &component::Theme) -> AnyElement {
    let tabs = [
        (WorkflowRunTab::Events, "events", "zconfig.workflowEvents"),
        (WorkflowRunTab::Artifacts, "artifacts", "zconfig.workflowArtifacts"),
        (WorkflowRunTab::Workspace, "workspace", "zconfig.workflowWorkspace"),
    ];
    let mut row = div()
        .flex()
        .gap(px(4.))
        .mx_3()
        .mb_2()
        .flex_none()
        .p(px(3.))
        .rounded_lg()
        .bg(theme.secondary);
    for (tab, slug, key) in tabs {
        let active = run.tab == tab;
        let runtime = wb.runtime.clone();
        row = row.child(
            div()
                .id(SharedString::from(format!("workflow-run-tab-{slug}")))
                .test_support()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .py(px(5.))
                .rounded_md()
                .text_size(px(12.))
                .cursor_pointer()
                .bg(if active {
                    theme.title_bar
                } else {
                    gpui::transparent_black()
                })
                .text_color(if active {
                    theme.foreground
                } else {
                    theme.muted_foreground
                })
                .font_weight(if active {
                    FontWeight::MEDIUM
                } else {
                    FontWeight::NORMAL
                })
                .child(t!(key).into_owned())
                .on_click(move |_, _, _| {
                    runtime.send(UiCommand::WorkflowRunSelectTab { tab });
                }),
        );
    }
    row.into_any_element()
}

// ---------------------------------------------------------------------------
// The events tab (the journal sliced per phase)
// ---------------------------------------------------------------------------

fn render_run_events_tab(
    mut list: gpui::Stateful<gpui::Div>,
    wb: &Workbench,
    run: &WorkflowRunView,
    cx: &Context<Workbench>,
) -> gpui::Stateful<gpui::Div> {
    let theme = cx.theme();

    // The cursor's next page loads NEWER events, which render on top — the
    // control rides above the cards (TS `workflowLoadNewer`).
    if run.has_more && run.events.is_some() {
        let runtime = wb.runtime.clone();
        list = list.child(
            div().px_3().pt_3().child(
                Button::new("workflow-run-load-newer")
                    .secondary()
                    .small()
                    .w_full()
                    .label(t!("zconfig.workflowLoadNewer").into_owned())
                    .disabled(run.events_busy)
                    .on_click(move |_, _, _| runtime.send(UiCommand::WorkflowRunLoadMore)),
            ),
        );
    }

    if let Some(events) = &run.events {
        if events.is_empty() {
            list = list.child(empty_card(&t!("zconfig.workflowEventsEmpty"), theme));
        } else {
            // Phase cards read newest-first: the newest slice is where the
            // run stands and sits on top; older phases fold beneath it. The
            // folds fold per slice (nodeHome/actorHome — replayed resume
            // prefixes must not double-render); the slices are reduced once
            // here so the head verdict and the cards share them.
            let sliced = phase_slices(events);
            let cards: Vec<(usize, &PhaseSlice, SliceFold)> = sliced
                .slices
                .iter()
                .enumerate()
                .map(|(i, s)| (i, s, reduce_slice(&Catalog, &s.events)))
                .collect();
            // The newest slice that actually paints is the visible head —
            // a just-entered phase owns nothing yet and must not leave the
            // content beneath it folded shut behind a card that never
            // renders.
            let head = cards
                .iter()
                .rev()
                .find(|(i, _, f)| phase_paints(f, &sliced, *i))
                .map(|(i, _, _)| *i);
            let mut block = rows_card(theme);
            let mut painted = 0usize;
            for (index, slice, fold) in cards.iter().rev() {
                let bare = cards.len() == 1 && slice.phase.is_none();
                let open = wb
                    .workflow_phase_open
                    .get(index)
                    .copied()
                    .unwrap_or(Some(*index) == head);
                if let Some(card) = render_phase_card(
                    wb, slice, fold, *index, &sliced, Some(*index) == head, run.active, bare,
                    open, cx,
                ) {
                    block = block.child(card);
                    painted += 1;
                }
            }
            if painted > 0 {
                list = list.child(block);
            }
        }
    }

    // The explicit Retry (TS: the first pull failed and the run settled).
    if run.events.is_none() && !run.session_dead && !run.active {
        let runtime = wb.runtime.clone();
        list = list.child(
            div().px_3().child(
                Button::new("workflow-run-retry")
                    .secondary()
                    .small()
                    .w_full()
                    .label(t!("zconfig.workflowRetry").into_owned())
                    .on_click(move |_, _, _| runtime.send(UiCommand::WorkflowRunRetryEvents)),
            ),
        );
    }
    // Still nothing while the run flies: the silent loading state.
    if run.events.is_none() && run.active {
        list = list.child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .py_8()
                .child(Spinner::new()),
        );
    }
    list
}

/// One phase's card (TS `PhaseSection`): the header carries the phase's
/// state and folds the body; only the visible head phase opens by default.
/// The body reads newest-first — the settle, the phase's journal rows, then
/// its steps, the hiring/usage footer last — so the latest message is the
/// first thing seen. Nodes/actors are filtered to the ones this slice OWNS
/// (`nodeHome`/`actorHome` — replayed resume prefixes must not
/// double-render).
fn render_phase_card(
    wb: &Workbench,
    slice: &PhaseSlice,
    fold: &SliceFold,
    slice_index: usize,
    sliced: &PhaseSlices,
    head: bool,
    running: bool,
    bare: bool,
    open: bool,
    cx: &Context<Workbench>,
) -> Option<AnyElement> {
    let theme = cx.theme();
    let is_own = |key: &str| sliced.node_home.get(key) == Some(&slice_index);
    let own_nodes: Vec<&NodeAgg> = fold.nodes.iter().filter(|n| is_own(&n.key)).collect();
    let own_actors: Vec<&ActorLine> = fold
        .actors
        .iter()
        .filter(|a| sliced.actor_home.get(&a.key) == Some(&slice_index))
        .collect();
    let journal = phase_rows(fold, &is_own);
    if !phase_paints(fold, sliced, slice_index) {
        return None;
    }

    // The body, newest first (TS PhaseSection): the settle closes the run,
    // then the phase-level journal rows, then the steps (each a fold of its
    // own), the hiring/usage footer last.
    let mut body = div().flex().flex_col();
    if let Some(ev) = &fold.settled {
        body = body.child(render_event_row(wb, ev, px(16.), false, cx));
    }
    for ev in &journal {
        body = body.child(render_event_row(wb, ev, px(16.), false, cx));
    }
    for n in &own_nodes {
        body = body.child(render_step_row(wb, n, cx));
    }
    if !own_actors.is_empty() {
        body = body.child(
            div()
                .px_4()
                .py(px(4.))
                .min_w_0()
                .text_size(px(10.))
                .text_color(theme.muted_foreground)
                .truncate()
                .child(format!(
                    "{} · {}",
                    t!("zconfig.wfEvActorCreated"),
                    own_actors
                        .iter()
                        .map(|a| a.label.as_str())
                        .collect::<Vec<_>>()
                        .join(" · ")
                )),
        );
    }
    if let Some(spent) = fold.spent_tokens {
        body = body.child(
            div()
                .px_4()
                .py(px(4.))
                .text_size(px(10.))
                .text_color(theme.muted_foreground)
                .child(format!(
                    "{} · {} tokens",
                    t!("zconfig.wfEvUsageUpdated"),
                    fmt_grouped(spent)
                )),
        );
    }

    let id = format!(
        "wf-phase-section-{slice_index}-{}",
        slice
            .phase
            .as_deref()
            .map(slug_run_id)
            .unwrap_or_else(|| "pre".to_string())
    );
    // The only slice and it has no phase: no card chrome, just the rows.
    if bare {
        return Some(
            div()
                .id(SharedString::from(id))
                .test_support()
                .flex()
                .flex_col()
                .child(body)
                .into_any_element(),
        );
    }

    let failed = own_nodes.iter().any(|n| n.state == NodeState::Failed)
        || fold
            .settled
            .as_ref()
            .is_some_and(|ev| describe_run_event(&Catalog, ev).tone == RunEventTone::Failed);
    let pulse = head && running && !failed;
    let dot = div()
        .size(px(8.))
        .flex_none()
        .rounded_full()
        .bg(if failed {
            theme.danger
        } else if head && running {
            theme.info
        } else {
            theme.success.opacity(0.8)
        });
    // The live head breathes (upstream animate-pulse); reduce-motion
    // renders it solid.
    let dot: AnyElement = if pulse && !cx.reduce_motion() {
        dot.with_animation(
            SharedString::from("wf-phase-card-pulse"),
            Animation::new(Duration::from_millis(1200))
                .repeat()
                .with_easing(pulsating_between(0.4, 1.0)),
            |this, delta| this.opacity(delta),
        )
        .into_any_element()
    } else {
        dot.into_any_element()
    };
    let name = slice
        .phase
        .clone()
        .unwrap_or_else(|| t!("zconfig.workflowPhasePrelude").into_owned());
    let title = match slice.ordinal {
        Some(n) if n >= 2 => format!("{name} ×{n}"),
        _ => name,
    };
    let wb_toggle = cx.entity();
    let open_now = open;
    let header = div()
        .id(SharedString::from(format!("wf-phase-head-{slice_index}")))
        .test_support()
        .flex()
        .items_center()
        .gap_2()
        .px_4()
        .py_2()
        .min_w_0()
        .cursor_pointer()
        .hover(|this| this.bg(theme.secondary.opacity(0.5)))
        .border_b_1()
        .border_color(if open {
            theme.border
        } else {
            gpui::transparent_black()
        })
        .on_click(move |_, _, cx| {
            wb_toggle.update(cx, |wb, cx| {
                wb.set_workflow_phase_open(slice_index, !open_now, cx)
            });
        })
        .child(dot)
        .child(
            div()
                .min_w_0()
                .flex_1()
                .font_family(theme.mono_font_family.clone())
                .text_size(px(13.))
                .text_color(if head {
                    theme.foreground
                } else {
                    theme.muted_foreground
                })
                .truncate()
                .child(title),
        )
        .when(!own_nodes.is_empty(), |this| {
            this.child(
                div()
                    .flex_none()
                    .text_size(px(10.))
                    .text_color(theme.muted_foreground)
                    .child(fill(
                        &t!("zconfig.workflowPhaseNodes"),
                        &[("n", &own_nodes.len().to_string())],
                    )),
            )
        })
        .child(
            Icon::new(IconName::ChevronRight)
                .small()
                .flex_none()
                .text_color(theme.muted_foreground)
                .when(open, |icon| icon.rotate(radians(std::f32::consts::FRAC_PI_2))),
        );
    let mut card = div()
        .id(SharedString::from(id))
        .test_support()
        .flex()
        .flex_col()
        .child(header);
    if open {
        card = card.child(body);
    }
    Some(card.into_any_element())
}

/// One step's row (TS `StepRow`): the folded line is state dot + identity +
/// activity + the outcome word; clicking opens the step's own detail — the
/// instruction head, the error, and its journal rows newest-first (the
/// instance ref omitted: the row already says whose they are).
fn render_step_row(wb: &Workbench, n: &NodeAgg, cx: &Context<Workbench>) -> AnyElement {
    let theme = cx.theme();
    let open = wb.workflow_step_open.contains(&n.key);
    let (dot_color, pulse) = match n.state {
        NodeState::Ok => (theme.success, false),
        NodeState::Failed => (theme.danger, false),
        NodeState::Cancelled => (theme.muted_foreground, false),
        NodeState::Waiting | NodeState::Repairing => (theme.warning, false),
        // queued / executing: the pulse dot already says "live".
        _ => (theme.info, true),
    };
    let word = node_state_word(&Catalog, n.state);
    let word_color = match n.state {
        NodeState::Failed => theme.danger_foreground,
        NodeState::Ok => theme.success_foreground.opacity(0.8),
        _ => theme.muted_foreground,
    };
    let title = n.label.clone().unwrap_or_else(|| match n.ordinal {
        Some(ord) => format!("{}@{}", n.site, ordinal_text(ord)),
        None => n.site.clone(),
    });
    let sub = n.activity.clone().or_else(|| n.head.clone());
    let dot = div()
        .size(px(6.))
        .flex_none()
        .rounded_full()
        .bg(dot_color);
    let dot: AnyElement = if pulse && !cx.reduce_motion() {
        dot.with_animation(
            SharedString::from("wf-node-line-pulse"),
            Animation::new(Duration::from_millis(1200))
                .repeat()
                .with_easing(pulsating_between(0.4, 1.0)),
            |this, delta| this.opacity(delta),
        )
        .into_any_element()
    } else {
        dot.into_any_element()
    };
    let wb_toggle = cx.entity();
    let toggle_key = n.key.clone();
    let open_now = open;
    div()
        .id(SharedString::from(format!("wf-node-line-{}", slug_run_id(&n.key))))
        .test_support()
        .flex()
        .flex_col()
        .px_4()
        .py(px(4.))
        .min_w_0()
        .child(
            div()
                .id(SharedString::from(format!(
                    "wf-step-head-{}",
                    slug_run_id(&n.key)
                )))
                .test_support()
                .flex()
                .items_center()
                .gap_2()
                .min_w_0()
                .cursor_pointer()
                .hover(|this| this.bg(theme.secondary.opacity(0.5)))
                .on_click(move |_, _, cx| {
                    let key = toggle_key.clone();
                    wb_toggle.update(cx, |wb, cx| {
                        wb.set_workflow_step_open(&key, !open_now, cx)
                    });
                })
                .child(dot)
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .text_size(px(12.))
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(match sub {
                            Some(sub) => format!("{title} — {sub}"),
                            None => title,
                        }),
                )
                .when_some(word, |this, word| {
                    this.child(
                        div()
                            .flex_none()
                            .text_size(px(10.))
                            .text_color(word_color)
                            .child(word),
                    )
                })
                .child(
                    Icon::new(IconName::ChevronRight)
                        .xsmall()
                        .flex_none()
                        .text_color(theme.muted_foreground)
                        .when(open, |icon| {
                            icon.rotate(radians(std::f32::consts::FRAC_PI_2))
                        }),
                ),
        )
        .when(!open && n.state == NodeState::Failed && n.error.is_some(), |this| {
            this.child(
                div()
                    .pl(px(14.))
                    .text_size(px(10.))
                    .text_color(theme.danger_foreground.opacity(0.9))
                    .truncate()
                    .child(n.error.clone().unwrap_or_default()),
            )
        })
        .when(open, |this| {
            // The step's own detail, newest first. The rows drop the
            // instance ref (the row already says whose they are) and sit
            // flush with the spine instead of the card's padding.
            let mut detail = div()
                .id(SharedString::from(format!(
                    "wf-step-body-{}",
                    slug_run_id(&n.key)
                )))
                .test_support()
                .flex()
                .flex_col()
                .min_w_0()
                .ml(px(3.))
                .mt_1()
                .pl_3()
                .border_l_1()
                .border_color(theme.border);
            if let Some(head_text) = &n.head {
                detail = detail.child(
                    div()
                        .min_w_0()
                        .pb_1()
                        .text_size(px(12.))
                        .text_color(theme.muted_foreground)
                        .child(head_text.clone()),
                );
            }
            if let Some(error) = &n.error {
                detail = detail.child(
                    div()
                        .min_w_0()
                        .pb_1()
                        .text_size(px(12.))
                        .text_color(theme.danger_foreground.opacity(0.9))
                        .child(error.clone()),
                );
            }
            let mut rows: Vec<&WorkflowRunEvent> = n.events.iter().collect();
            rows.sort_by(|a, b| newest_first(a, b));
            for ev in rows {
                detail = detail.child(render_event_row(wb, ev, px(0.), true, cx));
            }
            this.child(detail)
        })
        .into_any_element()
}

/// One journal row (TS `EventRow`): the readable label + detail (the
/// protocol renderer), the raw payload behind an explicit toggle. Sequence
/// numbers stay out of the default view — they are journal coordinates, not
/// information. `left_pad` aligns rows nested under a step's spine;
/// `omit_site` drops the instance ref there.
fn render_event_row(
    wb: &Workbench,
    ev: &WorkflowRunEvent,
    left_pad: Pixels,
    omit_site: bool,
    cx: &Context<Workbench>,
) -> AnyElement {
    let theme = cx.theme();
    let desc = describe_run_event_with(
        &Catalog,
        ev,
        DescribeOptions { omit_site },
    );
    let mono = desc.mono_label.unwrap_or(false);
    let label_color = if mono {
        theme.muted_foreground
    } else if desc.tone == RunEventTone::Failed {
        theme.danger_foreground
    } else {
        theme.foreground
    };
    let raw_key = format!("ev-{}", ev.sequence);
    let wb_handle = cx.entity();
    div()
        .id(SharedString::from(format!("wf-event-{}", ev.sequence)))
        .test_support()
        .flex()
        .flex_col()
        .min_w_0()
        .pl(left_pad)
        .pr_4()
        .py_2()
        .border_b_1()
        .border_color(theme.border)
        .cursor_pointer()
        .hover(|this| this.bg(theme.secondary.opacity(0.5)))
        .on_click({
            let key = raw_key.clone();
            move |_, _, cx| {
                wb_handle.update(cx, |wb, cx| wb.toggle_workflow_raw(&key, cx));
            }
        })
        .child(
            div()
                .flex()
                .items_baseline()
                .gap_2()
                .min_w_0()
                .child(
                    div()
                        .flex_none()
                        .max_w(px(240.))
                        .min_w_0()
                        .truncate()
                        .font_family(if mono {
                            theme.mono_font_family.clone()
                        } else {
                            theme.font_family.clone()
                        })
                        .text_size(px(12.))
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
                }),
        )
        .when(wb.workflow_raw_open.contains(&raw_key), |this| {
            this.child(render_raw_body(
                wb,
                &raw_key,
                &serde_json::Value::Object(ev.payload.clone()),
                cx,
            ))
        })
        .into_any_element()
}

/// The shared raw-JSON toggle (TS `RawData`): the escape hatch, never the
/// default view.
fn render_raw_body(
    wb: &Workbench,
    key: &str,
    value: &serde_json::Value,
    cx: &Context<Workbench>,
) -> AnyElement {
    let theme = cx.theme();
    let open = wb.workflow_raw_open.contains(key);
    let wb_handle = cx.entity();
    let key_owned = key.to_owned();
    div()
        .id(SharedString::from(format!("wf-raw-{key}")))
        .test_support()
        .mt_2()
        .child(
            div()
                .id(SharedString::from(format!("wf-raw-{key}-toggle")))
                .test_support()
                .flex()
                .items_center()
                .gap(px(4.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(theme.muted_foreground)
                .child(if open { "▾" } else { "▸" })
                .child(t!("zconfig.workflowRawData").into_owned())
                .on_click(move |_, _, cx| {
                    let key = key_owned.clone();
                    wb_handle.update(cx, |wb, cx| wb.toggle_workflow_raw(&key, cx));
                }),
        )
        .when(open, |this| {
            this.child(pre_block(
                &format!("wf-raw-{key}-body"),
                &pretty_json(value),
                theme.muted_foreground,
                theme,
            ))
        })
        .into_any_element()
}

// ---------------------------------------------------------------------------
// The artifacts tab
// ---------------------------------------------------------------------------

fn render_run_artifacts_tab(
    mut list: gpui::Stateful<gpui::Div>,
    wb: &Workbench,
    run: &WorkflowRunView,
    cx: &Context<Workbench>,
) -> gpui::Stateful<gpui::Div> {
    let theme = cx.theme();
    match &run.artifacts {
        Some(artifacts) if artifacts.is_empty() => {
            list = list.child(empty_card(&t!("zconfig.workflowArtifactsEmpty"), theme));
        }
        Some(artifacts) => {
            let mut block = rows_card(theme);
            for a in artifacts {
                let runtime = wb.runtime.clone();
                let artifact_id = a.id.clone();
                let slug = slug_run_id(&a.id);
                let meta = {
                    let mut parts = vec![a.kind.clone()];
                    if a.versions.as_ref().is_some_and(|v| !v.is_empty()) {
                        parts.push(format!("v{}", latest_version(a)));
                    }
                    if let Some(count) = a.item_count {
                        parts.push(count.to_string());
                    }
                    parts.join(" · ")
                };
                block = block.child(
                    div()
                        .id(SharedString::from(format!("wf-artifact-row-{slug}")))
                        .test_support()
                        .flex()
                        .items_start()
                        .gap_3()
                        .min_w_0()
                        .px_4()
                        .py_3()
                        .cursor_pointer()
                        .hover(|this| this.bg(theme.secondary.opacity(0.5)))
                        .on_click(move |_, _, _| {
                            runtime.send(UiCommand::WorkflowRunOpenArtifact { artifact_id: artifact_id.clone() });
                        })
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .text_size(px(13.))
                                        .text_color(theme.foreground)
                                        .truncate()
                                        .child(a.title.clone().unwrap_or_else(|| a.id.clone())),
                                )
                                .child(
                                    div()
                                        .text_size(px(10.))
                                        .text_color(theme.muted_foreground)
                                        .truncate()
                                        .child(meta),
                                ),
                        ),
                );
            }
            list = list.child(block);
        }
        None => {
            // Never landed: loading, or a failed lazy read (the banner
            // carries the failure) — re-picking the tab retries.
            if run.artifacts_busy {
                list = list.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .py_8()
                        .child(Spinner::new()),
                );
            } else {
                let runtime = wb.runtime.clone();
                list = list.child(
                    div().px_3().child(
                        Button::new("workflow-artifacts-retry")
                            .secondary()
                            .small()
                            .w_full()
                            .label(t!("zconfig.workflowRetry").into_owned())
                            .on_click(move |_, _, _| {
                                runtime.send(UiCommand::WorkflowRunSelectTab {
                                    tab: WorkflowRunTab::Artifacts,
                                })
                            }),
                    ),
                );
            }
        }
    }
    list
}

// ---------------------------------------------------------------------------
// The workspace tab
// ---------------------------------------------------------------------------

fn render_run_workspace_tab(
    mut list: gpui::Stateful<gpui::Div>,
    wb: &Workbench,
    run: &WorkflowRunView,
    cx: &Context<Workbench>,
) -> gpui::Stateful<gpui::Div> {
    let theme = cx.theme();
    match &run.nodes {
        Some(nodes) if nodes.is_empty() => {
            list = list.child(empty_card(&t!("zconfig.workflowWorkspaceEmpty"), theme));
        }
        Some(nodes) => {
            let mut block = rows_card(theme);
            for (i, n) in nodes.iter().enumerate() {
                block = block.child(render_workspace_row(wb, run, n, i, cx));
            }
            list = list.child(block);
        }
        None => {
            if run.nodes_busy {
                list = list.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .py_8()
                        .child(Spinner::new()),
                );
            } else {
                let runtime = wb.runtime.clone();
                list = list.child(
                    div().px_3().child(
                        Button::new("workflow-workspace-retry")
                            .secondary()
                            .small()
                            .w_full()
                            .label(t!("zconfig.workflowRetry").into_owned())
                            .on_click(move |_, _, _| {
                                runtime.send(UiCommand::WorkflowRunSelectTab {
                                    tab: WorkflowRunTab::Workspace,
                                })
                            }),
                    ),
                );
            }
        }
    }
    list
}

/// One workspace row (TS the workspace tab's node rows): the op tile scans
/// by color before text; the WHAT (call args) leads, the internal ref and
/// byte counts demote to a meta line. Tap expands the node's result.
fn render_workspace_row(
    wb: &Workbench,
    run: &WorkflowRunView,
    n: &WorkflowWorkspaceNode,
    index: usize,
    cx: &Context<Workbench>,
) -> AnyElement {
    let theme = cx.theme();
    let key = node_key(n, index);
    let slug = slug_run_id(&key);
    let dur = fmt_duration(n.created_at, n.updated_at);
    let exit_bad = n.summary.as_ref().is_some_and(|s| s.exit_code.unwrap_or(0) != 0);
    let runtime = wb.runtime.clone();
    let (site_id, ordinal) = (n.site_id.clone(), n.ordinal);
    let key_for_click = key.clone();
    let what = args_text(n)
        .or_else(|| n.op.clone())
        .unwrap_or_else(|| key.clone());
    let mut meta_parts = vec![node_ref_line(n)];
    if let Some(s) = &n.summary {
        meta_parts.push(node_summary_text(s));
    }
    let meta = meta_parts.join(" · ");

    let mut row = div()
        .id(SharedString::from(format!("wf-workspace-row-{slug}")))
        .test_support()
        .flex()
        .flex_col()
        .min_w_0()
        .px_4()
        .py(px(10.))
        .cursor_pointer()
        .hover(|this| this.bg(theme.secondary.opacity(0.5)))
        .on_click(move |_, _, _| {
            // TS `loadNode` returns early without a site/ordinal address.
            if let (Some(site), Some(ord)) = (site_id.clone(), ordinal) {
                runtime.send(UiCommand::WorkflowRunToggleNode {
                    key: key_for_click.clone(),
                    site_id: site,
                    ordinal: ord,
                });
            }
        })
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .min_w_0()
                .child(op_tile(n.op.as_deref(), theme))
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .min_w_0()
                                .text_size(px(12.))
                                .text_color(theme.foreground)
                                .truncate()
                                .child(what),
                        )
                        .when(!meta.is_empty(), |this| {
                            this.child(
                                div()
                                    .min_w_0()
                                    .font_family(theme.mono_font_family.clone())
                                    .text_size(px(10.))
                                    .text_color(if exit_bad {
                                        theme.danger_foreground.opacity(0.9)
                                    } else {
                                        theme.muted_foreground
                                    })
                                    .truncate()
                                    .child(meta),
                            )
                        }),
                )
                .when_some(dur, |this, dur| {
                    this.child(
                        div()
                            .flex_none()
                            .text_size(px(10.))
                            .text_color(theme.muted_foreground)
                            .child(format!("+{dur}")),
                    )
                })
                .child(status_badge(
                    n.status.as_deref(),
                    &format!("workspace-{slug}"),
                    theme,
                )),
        );
    if run.node_busy.as_deref() == Some(key.as_str()) {
        row = row.child(
            div()
                .id(SharedString::from(format!("wf-node-busy-{slug}")))
                .test_support()
                .pl(px(38.))
                .pt_1()
                .child(Spinner::new().xsmall()),
        );
    } else if let Some(result) = run.node_result.as_ref().filter(|r| r.key == key) {
        row = row.child(render_node_result(wb, result, cx));
    }
    row.into_any_element()
}

/// The op tile (TS `OP_TILE`): a small colored square keyed by the op
/// family — read/search/git/exec/write — carrying the op's first two
/// characters.
fn op_tile(op: Option<&str>, theme: &component::Theme) -> AnyElement {
    let (bg, fg) = match op_family(op) {
        OpFamily::Read => (theme.info.opacity(0.15), theme.info),
        OpFamily::Search => (theme.accent.opacity(0.15), theme.accent),
        OpFamily::Git => (theme.warning.opacity(0.15), theme.warning),
        OpFamily::Exec => (theme.success.opacity(0.15), theme.success),
        OpFamily::Write => (theme.danger.opacity(0.15), theme.danger),
        OpFamily::Other => (theme.secondary, theme.muted_foreground),
    };
    div()
        .flex_none()
        .size(px(28.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_lg()
        .bg(bg)
        .font_family(theme.mono_font_family.clone())
        .text_size(px(10.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(fg)
        .child(op.unwrap_or("?").chars().take(2).collect::<String>().to_uppercase())
        .into_any_element()
}

/// One expanded node result (TS `NodeResultView`): structured before raw —
/// strings render as text, world.run-shaped results split into exit code +
/// output streams, plain objects become key-value rows.
fn render_node_result(
    wb: &Workbench,
    result: &WorkflowNodeResultView,
    cx: &Context<Workbench>,
) -> AnyElement {
    let theme = cx.theme();
    let slug = slug_run_id(&result.key);
    let mut card = div()
        .id(SharedString::from(format!("wf-node-result-{slug}")))
        .test_support()
        .flex()
        .flex_col()
        .min_w_0()
        .mt_2()
        .p_3()
        .rounded_xl()
        .bg(theme.secondary)
        .border_1()
        .border_color(theme.border);
    // v4 node status enum: running | completed | failed — red is for failed
    // only; anything else (incl. unknown tokens) stays quiet.
    if result.status.as_deref() == Some("failed") {
        card = card.child(
            div()
                .pb(px(6.))
                .text_size(px(12.))
                .text_color(theme.danger_foreground)
                .child(result.status.clone().unwrap_or_default()),
        );
    }
    card = match node_result_body(&result.body) {
        NodeResultBody::Text(text) => card.child(pre_block(
            &format!("wf-node-result-{slug}-text"),
            &text,
            theme.muted_foreground,
            theme,
        )),
        NodeResultBody::Run {
            exit_code,
            stdout,
            stderr,
        } => card
            .when_some(exit_code, |this, code| {
                this.child(
                    div()
                        .pb(px(6.))
                        .font_family(theme.mono_font_family.clone())
                        .text_size(px(12.))
                        .text_color(if code != 0.0 {
                            theme.danger_foreground
                        } else {
                            theme.muted_foreground
                        })
                        .child(format!("exit {}", ordinal_text(code))),
                )
            })
            .when_some(stdout, |this, out| {
                this.child(pre_block(
                    &format!("wf-node-result-{slug}-stdout"),
                    &out,
                    theme.muted_foreground,
                    theme,
                ))
            })
            .when_some(stderr, |this, err| {
                this.child(pre_block(
                    &format!("wf-node-result-{slug}-stderr"),
                    &err,
                    theme.danger_foreground.opacity(0.9),
                    theme,
                ))
            }),
        NodeResultBody::Record(rows) => card.child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .children(rows.iter().map(|(k, v)| {
                    div()
                        .flex()
                        .items_baseline()
                        .gap_3()
                        .min_w_0()
                        .child(
                            div()
                                .flex_none()
                                .max_w(px(40.))
                                .text_size(px(12.))
                                .text_color(theme.muted_foreground)
                                .truncate()
                                .child(k.clone()),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .font_family(theme.mono_font_family.clone())
                                .text_size(px(12.))
                                .text_color(theme.foreground)
                                .child(v.clone()),
                        )
                })),
        ),
        NodeResultBody::Scalar(text) => card.child(
            div()
                .min_w_0()
                .font_family(theme.mono_font_family.clone())
                .text_size(px(12.))
                .text_color(theme.muted_foreground)
                .child(text),
        ),
    };
    if result.truncated {
        card = card.child(
            div()
                .pt_1()
                .text_size(px(10.))
                .text_color(theme.muted_foreground)
                .child(t!("zconfig.workflowTruncated").into_owned()),
        );
    }
    let raw_key = format!("node-{}", result.key);
    card.child(render_raw_body(wb, &raw_key, &result.body, cx))
        .into_any_element()
}

// ---------------------------------------------------------------------------
// The artifact viewer (TS ArtifactViewer + WorkflowArtifactViews)
// ---------------------------------------------------------------------------

fn render_artifact_viewer(
    wb: &Workbench,
    viewer: &WorkflowArtifactViewerView,
    cx: &Context<Workbench>,
) -> AnyElement {
    let theme = cx.theme();
    let kind = viewer.artifact.kind.as_str();
    let preset = artifact_is_preset(kind);
    let mut list = div()
        .id("workflow-artifact-viewer")
        .test_support()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .overflow_y_scroll()
        .overflow_x_hidden()
        .pb_4();

    // Nothing landed yet: the initial read is in flight.
    if viewer.busy && viewer.text.is_none() && viewer.items.is_none() {
        return div()
            .flex_1()
            .min_h_0()
            .flex()
            .items_center()
            .justify_center()
            .child(Spinner::new())
            .into_any_element();
    }

    let spec = artifact_spec(&viewer.artifact);
    let items = viewer.items.as_deref().unwrap_or(&[]);
    if kind == "markdown" && !viewer.binary && viewer.text.is_some() {
        // markdown: decode finished when the text landed (TS renders only
        // once the whole (first) chunk decoded).
        list = list.child(
            div()
                .id("wf-artifact-markdown")
                .test_support()
                .px_4()
                .pt_3()
                .child(TextView::markdown(
                    "wf-artifact-markdown",
                    viewer.text.clone().expect("checked above"),
                )),
        );
    } else if kind == "file" {
        if viewer.binary {
            let bytes = viewer
                .total_bytes
                .map(|b| b.to_string())
                .or_else(|| viewer.text.as_ref().map(|t| t.chars().count().to_string()))
                .unwrap_or_else(|| "0".to_owned());
            list = list.child(empty_card(
                &fill(&t!("zconfig.workflowBinary"), &[("bytes", bytes.as_str())]),
                theme,
            ));
        } else if let Some(text) = &viewer.text {
            list = list.child(
                div()
                    .id("wf-artifact-file")
                    .test_support()
                    .pt_3()
                    .child(pre_block("wf-artifact-file-body", text, theme.muted_foreground, theme)),
            );
        }
        if viewer.next_offset.is_some() {
            let runtime = wb.runtime.clone();
            list = list.child(
                div().px_3().pt_3().child(
                    Button::new("workflow-artifact-load-more")
                        .secondary()
                        .small()
                        .w_full()
                        .label(t!("zconfig.workflowLoadMore").into_owned())
                        .disabled(viewer.busy)
                        .on_click(move |_, _, _| runtime.send(UiCommand::WorkflowRunArtifactLoadMore)),
                ),
            );
        }
    } else if kind == "table" {
        list = list.child(render_table_view(items, spec, theme));
    } else if kind == "metrics" {
        list = list.child(render_metrics_tiles(items, spec, theme));
    } else if kind == "board" {
        list = list.child(render_board_view(items, spec, theme));
    } else if kind == "chart" {
        list = list.child(render_chart_view(items, spec, theme));
    } else {
        // Unknown kinds (and a binary markdown): a metadata card plus the
        // raw JSON — the shape is open, so new kinds still read.
        list = list.child(render_artifact_meta_card(wb, &viewer.artifact, theme, cx));
    }

    // The preset kinds keep the raw items one toggle away, plus their own
    // pagination.
    if preset {
        let raw: Vec<serde_json::Value> = items.iter().map(|it| it.item.clone()).collect();
        list = list.child(
            div().px_4().pt_1().child(render_raw_body(
                wb,
                "art-items",
                &serde_json::Value::Array(raw),
                cx,
            )),
        );
        if viewer.items_has_more {
            let runtime = wb.runtime.clone();
            list = list.child(
                div().px_3().pt_3().child(
                    Button::new("workflow-artifact-load-more")
                        .secondary()
                        .small()
                        .w_full()
                        .label(t!("zconfig.workflowLoadMore").into_owned())
                        .disabled(viewer.busy)
                        .on_click(move |_, _, _| {
                            runtime.send(UiCommand::WorkflowRunArtifactLoadMore)
                        }),
                ),
            );
        }
    }
    list.into_any_element()
}

/// The key-value fallback (TS `KeyValueCards`).
fn render_key_value_cards(cards: &[(u64, KeyValueCard)], theme: &component::Theme) -> AnyElement {
    let mut col = div().flex().flex_col().gap_2().px_3().pt_2();
    for (seq, card) in cards {
        col = col.child(match card {
            KeyValueCard::Text(text) => pre_block(
                &format!("wf-kv-text-{seq}"),
                text,
                theme.muted_foreground,
                theme,
            ),
            KeyValueCard::Record(rows) => div()
                .id(SharedString::from(format!("wf-kv-card-{seq}")))
                .test_support()
                .px_3()
                .py_2()
                .rounded_xl()
                .bg(theme.title_bar)
                .border_1()
                .border_color(theme.border)
                .flex()
                .flex_col()
                .gap(px(2.))
                .children(rows.iter().map(|(k, v)| kv_row(k, v, theme)))
                .into_any_element(),
        });
    }
    col.into_any_element()
}

fn kv_row(k: &str, v: &str, theme: &component::Theme) -> AnyElement {
    div()
        .flex()
        .items_baseline()
        .gap_3()
        .min_w_0()
        .child(
            div()
                .flex_none()
                .max_w(px(120.))
                .text_size(px(12.))
                .text_color(theme.muted_foreground)
                .truncate()
                .child(k.to_owned()),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .text_right()
                .font_family(theme.mono_font_family.clone())
                .text_size(px(12.))
                .text_color(theme.foreground)
                .child(v.to_owned()),
        )
        .into_any_element()
}

/// The table view (TS `TableView`), or the fallback when neither a spec nor
/// a first row names columns.
fn render_table_view(
    items: &[ArtifactDataItem],
    spec: Option<&serde_json::Value>,
    theme: &component::Theme,
) -> AnyElement {
    match table_model(items, spec) {
        Some(model) => {
            let mut col = div()
                .id("wf-table-view")
                .test_support()
                .flex()
                .flex_col()
                .px_3()
                .pt_2()
                .min_w_0()
                // A header row of declared columns, then one row per item;
                // cells truncate so a wide row cannot push the page wide.
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .px_2()
                        .py(px(6.))
                        .border_b_1()
                        .border_color(theme.border)
                        .children(
                            model
                                .columns
                                .iter()
                                .map(|c| {
                                    div()
                                        .min_w_0()
                                        .flex_1()
                                        .text_size(px(12.))
                                        .text_color(theme.foreground)
                                        .truncate()
                                        .child(c.clone())
                                        .into_any_element()
                                })
                                .collect::<Vec<_>>(),
                        ),
                );
            for (i, row) in model.rows.iter().enumerate() {
                col = col.child(
                    div()
                        .id(SharedString::from(format!("wf-table-row-{i}")))
                        .test_support()
                        .flex()
                        .gap_2()
                        .px_2()
                        .py(px(6.))
                        .border_b_1()
                        .border_color(theme.border.opacity(0.5))
                        .children(
                            row.iter()
                                .map(|cell| {
                                    div()
                                        .min_w_0()
                                        .flex_1()
                                        .truncate()
                                        .text_size(px(12.))
                                        .text_color(theme.muted_foreground)
                                        .child(cell.clone())
                                        .into_any_element()
                                })
                                .collect::<Vec<_>>(),
                        ),
                );
            }
            col.into_any_element()
        }
        None => render_key_value_cards(
            &key_value_cards(
                &items
                    .iter()
                    .filter(|it| !it.item.is_object())
                    .cloned()
                    .collect::<Vec<_>>(),
            ),
            theme,
        ),
    }
}

/// The metrics tiles (TS `MetricsTiles`), or the fallback.
fn render_metrics_tiles(
    items: &[ArtifactDataItem],
    spec: Option<&serde_json::Value>,
    theme: &component::Theme,
) -> AnyElement {
    match metrics_tiles(items, spec) {
        Some(tiles) => {
            // Two per row (TS grid-cols-2).
            let mut outer = div()
                .id("wf-metrics-view")
                .test_support()
                .flex()
                .flex_col()
                .gap_2()
                .px_3()
                .pt_2();
            for pair in tiles.chunks(2) {
                outer = outer.child(
                    div().flex().gap_2().children(pair.iter().map(|tile| {
                        div()
                            .flex()
                            .flex_1()
                            .flex_col()
                            .min_w_0()
                            .px_3()
                            .py(px(10.))
                            .rounded_xl()
                            .bg(theme.title_bar)
                            .border_1()
                            .border_color(theme.border)
                            .child(
                                div()
                                    .min_w_0()
                                    .text_size(px(12.))
                                    .text_color(theme.muted_foreground)
                                    .truncate()
                                    .child(tile.label.clone()),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .pt(px(2.))
                                    .font_family(theme.mono_font_family.clone())
                                    .text_size(px(15.))
                                    .text_color(theme.foreground)
                                    .truncate()
                                    .child(format!(
                                        "{}{}",
                                        tile.value,
                                        tile.unit
                                            .as_deref()
                                            .map(|u| format!(" {u}"))
                                            .unwrap_or_default()
                                    )),
                            )
                            .into_any_element()
                    })),
                );
            }
            outer.into_any_element()
        }
        None => render_key_value_cards(&key_value_cards(items), theme),
    }
}

/// The board view (TS `BoardView`): columns render as sections, one under
/// the other, in spec order; unknown statuses trail.
fn render_board_view(
    items: &[ArtifactDataItem],
    spec: Option<&serde_json::Value>,
    theme: &component::Theme,
) -> AnyElement {
    match board_columns(items, spec) {
        Some(columns) => {
            let mut outer = div()
                .id("wf-board-view")
                .test_support()
                .flex()
                .flex_col()
                .gap_3()
                .px_3()
                .pt_2();
            for col in &columns {
                let mut section = div()
                    .id(SharedString::from(format!("wf-board-col-{}", slug_run_id(&col.name))))
                    .test_support()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap_2()
                            .border_b_1()
                            .border_color(theme.border)
                            .pb_1()
                            .mb(px(6.))
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .text_size(px(13.))
                                    .text_color(theme.foreground)
                                    .truncate()
                                    .child(col.name.clone()),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(10.))
                                    .text_color(theme.muted_foreground)
                                    .child(col.cards.len().to_string()),
                            ),
                    );
                if col.cards.is_empty() {
                    section = section.child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.muted_foreground)
                            .child(t!("zconfig.workflowBoardEmpty").into_owned()),
                    );
                } else {
                    section = section.child(
                        div().flex().flex_col().gap(px(6.)).children(
                            col.cards.iter().map(|card| {
                                div()
                                    .id(SharedString::from(format!(
                                        "wf-board-card-{}",
                                        slug_run_id(&card.key)
                                    )))
                                    .test_support()
                                    .flex()
                                    .flex_col()
                                    .min_w_0()
                                    .px_3()
                                    .py_2()
                                    .rounded_xl()
                                    .bg(theme.title_bar)
                                    .border_1()
                                    .border_color(theme.border)
                                    .child(
                                        div()
                                            .min_w_0()
                                            .text_size(px(12.))
                                            .text_color(theme.foreground)
                                            .truncate()
                                            .child(card.title.clone()),
                                    )
                                    .children(
                                        card.details
                                            .iter()
                                            .map(|(k, v)| kv_row(k, v, theme))
                                            .collect::<Vec<_>>(),
                                    )
                            }),
                        ),
                    );
                }
                outer = outer.child(section);
            }
            outer.into_any_element()
        }
        None => render_key_value_cards(&key_value_cards(items), theme),
    }
}

/// The chart view (TS `ChartView`): a small inline canvas — grid, baseline,
/// series by kind — with the y-axis labels beside it, the category labels
/// and the latest-per-series legend under it.
fn render_chart_view(
    items: &[ArtifactDataItem],
    spec: Option<&serde_json::Value>,
    theme: &component::Theme,
) -> AnyElement {
    let Some(model) = chart_model(items, spec) else {
        return render_key_value_cards(&key_value_cards(items), theme);
    };
    let grid_color = theme.chart_grid;
    let baseline_color = theme.muted_foreground.opacity(0.35);
    let series_colors: [Hsla; 5] = [
        theme.info,
        theme.success,
        theme.danger,
        theme.warning,
        theme.accent,
    ];
    let m = model.clone();
    let plot = canvas(
        move |_, _, _| (),
        move |bounds, _, window, _| {
            // A small inset keeps frac-0/frac-1 marks from clipping.
            let inset = px(6.);
            let b = Bounds::new(
                point(bounds.origin.x + inset, bounds.origin.y + inset),
                size(bounds.size.width - inset * 2.0, bounds.size.height - inset * 2.0),
            );
            let plot_h = b.size.height;
            let plot_w = b.size.width;
            let y_of = |v: f64| b.origin.y + plot_h * (1.0 - m.y_frac(v) as f32);
            let x_of = |v: f64| b.origin.x + plot_w * m.x_frac(v) as f32;
            for v in m.grid_values() {
                let y = y_of(v);
                window.paint_quad(quad_fill(
                    Bounds::new(point(b.origin.x, y), size(plot_w, px(1.))),
                    grid_color,
                ));
            }
            if let Some(base) = m.baseline {
                let y = y_of(base);
                let mut path = PathBuilder::stroke(px(1.)).dash_array(&[px(3.), px(3.)]);
                path.move_to(point(b.origin.x, y));
                path.line_to(point(b.origin.x + plot_w, y));
                if let Ok(path) = path.build() {
                    window.paint_path(path, baseline_color);
                }
            }
            let series_count = m.series.len().max(1) as f32;
            match m.kind {
                ChartKind::Scatter => {
                    for (si, s) in m.series.iter().enumerate() {
                        let color = series_colors[si % series_colors.len()];
                        for p in &s.points {
                            let (x, y) = (x_of(p.0), y_of(p.1));
                            window.paint_quad(
                                quad_fill(
                                    Bounds::new(point(x - px(2.), y - px(2.)), size(px(4.), px(4.))),
                                    color,
                                )
                                .corner_radii(px(2.)),
                            );
                        }
                    }
                }
                ChartKind::Bar => {
                    // Bars sit AT their x value; sibling series share the
                    // slot, side by side (TS's layout).
                    let n = m
                        .series
                        .iter()
                        .map(|s| s.points.len())
                        .max()
                        .unwrap_or(1)
                        .max(1);
                    let bar_w = ((plot_w / ((n as f32) * series_count)) * 0.7).max(px(1.5));
                    let bottom = b.origin.y + plot_h;
                    for (si, s) in m.series.iter().enumerate() {
                        let color = series_colors[si % series_colors.len()];
                        for p in &s.points {
                            let cx = x_of(p.0) + (si as f32 - (series_count - 1.0) / 2.0) * bar_w;
                            let top = y_of(p.1);
                            let h = (bottom - top).max(px(1.));
                            window.paint_quad(
                                quad_fill(
                                    Bounds::new(point(cx - bar_w / 2.0, top), size(bar_w, h)),
                                    color.opacity(0.85),
                                )
                                .corner_radii(px(1.)),
                            );
                        }
                    }
                }
                ChartKind::Line => {
                    for (si, s) in m.series.iter().enumerate() {
                        let color = series_colors[si % series_colors.len()];
                        if s.points.len() == 1 {
                            let (x, y) = (x_of(s.points[0].0), y_of(s.points[0].1));
                            window.paint_quad(
                                quad_fill(
                                    Bounds::new(point(x - px(3.), y - px(3.)), size(px(6.), px(6.))),
                                    color,
                                )
                                .corner_radii(px(3.)),
                            );
                            continue;
                        }
                        let mut path = PathBuilder::stroke(px(1.5));
                        let mut first = true;
                        for p in &s.points {
                            let at = point(x_of(p.0), y_of(p.1));
                            if first {
                                path.move_to(at);
                                first = false;
                            } else {
                                path.line_to(at);
                            }
                        }
                        if let Ok(path) = path.build() {
                            window.paint_path(path, color);
                        }
                    }
                }
            }
        },
    )
    .w_full()
    .h(px(180.))
    .rounded_lg()
    .bg(theme.secondary);

    let mut outer = div()
        .id("wf-chart-view")
        .test_support()
        .flex()
        .flex_col()
        .px_3()
        .pt_2();
    // The y gridline labels ride beside the plot (top = max).
    let grid = model.grid_values();
    outer = outer.child(
        div()
            .flex()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .justify_between()
                    .items_end()
                    .h(px(180.))
                    .w(px(40.))
                    .text_size(px(9.))
                    .text_color(theme.muted_foreground)
                    .child(chart_fmt(grid[2]))
                    .child(chart_fmt(grid[1]))
                    .child(chart_fmt(grid[0])),
            )
            .child(div().flex_1().min_w_0().child(plot)),
    );
    // Category labels (≤12, TS's cap), first-seen order.
    if !model.categories.is_empty() && model.categories.len() <= 12 {
        outer = outer.child(
            div()
                .flex()
                .justify_between()
                .pl(px(46.))
                .pt(px(2.))
                .text_size(px(9.))
                .text_color(theme.muted_foreground)
                .children(
                    model
                        .categories
                        .iter()
                        .map(|c| {
                            let label = if c.chars().count() > 6 {
                                format!("{}…", c.chars().take(5).collect::<String>())
                            } else {
                                c.clone()
                            };
                            div().min_w_0().truncate().child(label).into_any_element()
                        })
                        .collect::<Vec<_>>(),
                ),
        );
    }
    // The legend: the latest value per series under the chart.
    outer = outer.child(
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .pt_2()
            .children(
                model
                    .series
                    .iter()
                    .enumerate()
                    .map(|(si, s)| {
                        let color = series_colors[si % series_colors.len()];
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .min_w_0()
                            .child(div().flex_none().size(px(6.)).rounded_full().bg(color))
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .text_size(px(12.))
                                    .text_color(theme.muted_foreground)
                                    .truncate()
                                    .child(s.label.clone()),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .font_family(theme.mono_font_family.clone())
                                    .text_size(px(12.))
                                    .text_color(theme.foreground)
                                    .child(format!(
                                        "{}{}",
                                        s.last.map(chart_fmt).unwrap_or_else(|| "—".to_owned()),
                                        s.unit
                                            .as_deref()
                                            .map(|u| format!(" {u}"))
                                            .unwrap_or_default()
                                    )),
                            )
                            .into_any_element()
                    })
                    .collect::<Vec<_>>(),
            ),
    );
    if model.use_log {
        outer = outer.child(
            div()
                .pt_1()
                .text_size(px(10.))
                .text_color(theme.muted_foreground)
                .child("log scale"),
        );
    }
    outer.into_any_element()
}

/// The unknown-kind metadata card (TS `ArtifactMetaCard`).
fn render_artifact_meta_card(
    wb: &Workbench,
    artifact: &WorkflowArtifact,
    theme: &component::Theme,
    cx: &Context<Workbench>,
) -> AnyElement {
    let meta = artifact_meta(artifact);
    let mut col = div()
        .id("wf-artifact-meta")
        .test_support()
        .flex()
        .flex_col()
        .px_3()
        .pt_3()
        .min_w_0()
        .child(
            div()
                .flex_none()
                .self_start()
                .px(px(6.))
                .py(px(1.))
                .rounded_md()
                .bg(theme.secondary)
                .font_family(theme.mono_font_family.clone())
                .text_size(px(10.))
                .text_color(theme.muted_foreground)
                .child(meta.kind.clone()),
        )
        .when_some(meta.description.clone(), |this, description| {
            this.child(
                div()
                    .pt_2()
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .min_w_0()
                    .child(description),
            )
        })
        .child(
            div()
                .mt_2()
                .px_3()
                .py_2()
                .rounded_xl()
                .bg(theme.title_bar)
                .border_1()
                .border_color(theme.border)
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(kv_row("id", &meta.id, theme))
                .when_some(meta.content_type.clone(), |this, ty| {
                    this.child(kv_row("type", &ty, theme))
                })
                .when_some(meta.source_path.clone(), |this, path| {
                    this.child(kv_row(&t!("zconfig.workflowSourcePath").into_owned(), &path, theme))
                })
                .when_some(meta.item_count, |this, count| {
                    this.child(kv_row("items", &count.to_string(), theme))
                })
                .children(
                    meta.extras
                        .iter()
                        .map(|(k, v)| kv_row(k, v, theme))
                        .collect::<Vec<_>>(),
                ),
        );
    col = col.child(render_raw_body(
        wb,
        "artifact",
        &serde_json::to_value(artifact).unwrap_or(serde_json::Value::Null),
        cx,
    ));
    col.into_any_element()
}

// ---------------------------------------------------------------------------
// Small text helpers
// ---------------------------------------------------------------------------

/// A monospace text block (the TS `<pre>`): wraps instead of scrolling
/// horizontally, capped in height like `max-h-*`; mega-lines chop (the
/// file viewer's display rule) and the tail over 1000 rendered lines is
/// elided (a layout guard the DOM <pre> gets for free).
fn pre_block(id: &str, text: &str, color: Hsla, theme: &component::Theme) -> AnyElement {
    const MAX_RENDER_LINES: usize = 1000;
    const MAX_LINE_CHARS: usize = 2000;
    let mut lines: Vec<String> = text
        .split('\n')
        .take(MAX_RENDER_LINES + 1)
        .map(|line| {
            if line.chars().count() > MAX_LINE_CHARS {
                let mut head: String = line.chars().take(MAX_LINE_CHARS).collect();
                head.push_str(" …");
                head
            } else if line.is_empty() {
                " ".to_owned()
            } else {
                line.to_owned()
            }
        })
        .collect();
    let total = text.split('\n').count();
    if total > MAX_RENDER_LINES {
        lines.truncate(MAX_RENDER_LINES);
        lines.push(format!("… {} more lines", total - MAX_RENDER_LINES));
    }
    div()
        .id(SharedString::from(id.to_owned()))
        .test_support()
        .flex()
        .flex_col()
        .min_w_0()
        .max_h(px(256.))
        .overflow_y_scroll()
        .children(lines.into_iter().map(|line| {
            div()
                .min_w_0()
                .font_family(theme.mono_font_family.clone())
                .text_size(px(12.))
                .text_color(color)
                .child(line)
        }))
        .into_any_element()
}

/// `n.toLocaleString()` (en-US grouping) for the usage line's token counts.
fn fmt_grouped(n: f64) -> String {
    let negative = n < 0.0;
    let int = n.abs().trunc() as u64;
    let digits = int.to_string();
    let grouped = digits
        .as_bytes()
        .rchunks(3)
        .rev()
        .map(std::str::from_utf8)
        .map(Result::unwrap)
        .collect::<Vec<_>>()
        .join(",");
    if negative {
        format!("-{grouped}")
    } else {
        grouped
    }
}

/// A JS number's display form (ordinals, exit codes): integers without a
/// fraction print bare.
fn ordinal_text(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
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

/// The meta edit form's state (TS `MetaSheet`): exactly the two prose
/// fields, prefilled from the loaded detail.
pub(crate) struct WorkflowMetaForm {
    pub(crate) scope: WorkflowScope,
    pub(crate) name: String,
    pub(crate) instance_id: String,
    pub(crate) description: Entity<TextareaState>,
    pub(crate) when_to_use: Entity<TextareaState>,
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

    /// Opens the meta edit sheet (TS `setEditingDesc(true)`): the two prose
    /// fields prefilled from the open detail page's loaded meta.
    pub fn open_workflow_meta_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(detail) = self
            .snap
            .workflows_page
            .as_ref()
            .and_then(|p| p.detail.as_ref())
        else {
            return;
        };
        let (scope, name, instance_id) =
            (detail.scope, detail.name.clone(), detail.instance_id.clone());
        let (description_text, when_to_use_text) = match &detail.detail {
            Some(d) => (
                meta_string(d, "description").unwrap_or_default(),
                meta_string(d, "whenToUse").unwrap_or_default(),
            ),
            None => (String::new(), String::new()),
        };
        let description = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx);
            state.set_value(description_text, window, cx);
            state
        });
        let when_to_use = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx);
            state.set_value(when_to_use_text, window, cx);
            state
        });
        let mut subs = Vec::new();
        for entity in [&description, &when_to_use] {
            subs.push(cx.subscribe(entity, |_, _, _: &InputEvent, cx| cx.notify()));
        }
        self.workflow_meta_form = Some(WorkflowMetaForm {
            scope,
            name,
            instance_id,
            description,
            when_to_use,
        });
        self.workflow_form_subs = subs;
        cx.notify();
    }

    /// TS `canSubmit` for the meta sheet: description is required non-empty
    /// (upstream refuses blank) — whenToUse is optional by construction.
    fn workflow_meta_can_submit(&self, cx: &Context<Self>) -> bool {
        let Some(form) = &self.workflow_meta_form else {
            return false;
        };
        !form.description.read(cx).value().trim().is_empty()
    }

    /// The meta sheet's submit (TS `saveMeta`): build the whole next meta
    /// (the write REPLACES, so the untouched rest merges back in; an empty
    /// whenToUse drops the key), dispatch, and close — the page's notice
    /// banner carries the outcome, the reload the success.
    fn submit_workflow_meta_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &self.workflow_meta_form else {
            return;
        };
        let existing = self
            .snap
            .workflows_page
            .as_ref()
            .and_then(|p| p.detail.as_ref())
            .and_then(|d| d.detail.as_ref())
            .and_then(|d| d.meta.as_ref());
        let meta = merge_workflow_meta(
            existing,
            &form.description.read(cx).value(),
            &form.when_to_use.read(cx).value(),
        );
        // A blank description cannot happen through the UI (the submit
        // stays disabled); if it slips through anyway, the merge returns
        // None and nothing dispatches — a blank write must never leave.
        if let Some(meta) = meta {
            let (scope, name, instance_id) =
                (form.scope, form.name.clone(), form.instance_id.clone());
            self.runtime.send(UiCommand::UpdateWorkflowMeta {
                scope,
                name,
                instance_id,
                meta,
            });
        }
        self.workflow_meta_form = None;
        self.workflow_form_subs.clear();
        cx.notify();
    }

    /// The meta edit sheet (TS `MetaSheet`).
    pub fn render_workflow_meta_form(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let Some(form) = &self.workflow_meta_form else {
            return div().into_any_element();
        };
        let can_submit = self.workflow_meta_can_submit(cx);
        let mut column = div()
            .id("workflow-meta-form")
            .test_support()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_x_hidden();
        column = column.child(form_header(
            &t!("zconfig.workflowEditDescription").into_owned(),
            "workflow-form-back",
            theme,
            cx,
        ));
        let wb = cx.entity();
        let body = div()
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .max_w(rems(46.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.foreground)
                            .child(t!("zconfig.workflowDescription").into_owned()),
                    )
                    .child(Textarea::new(&form.description).small()),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.foreground)
                            .child(t!("zconfig.workflowWhenToUse").into_owned()),
                    )
                    .child(Textarea::new(&form.when_to_use).small()),
            )
            .child(
                Button::new("workflow-meta-submit")
                    .primary()
                    .w_full()
                    .label(t!("common.save").into_owned())
                    .disabled(!can_submit)
                    .on_click(move |_, _, cx| {
                        wb.update(cx, |this, cx| this.submit_workflow_meta_form(cx));
                    }),
            );
        column
            .child(
                div()
                    .id("workflow-meta-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body),
            )
            .into_any_element()
    }

    /// The script fold (TS `setScriptOpen(!scriptOpen)`): view-local, reset
    /// when the open workflow changes.
    pub(crate) fn toggle_workflow_script(&mut self, cx: &mut Context<Self>) {
        self.workflow_script_open = !self.workflow_script_open;
        cx.notify();
    }

    /// The delete confirm's arm (TS `setConfirmDelete(true)`).
    pub(crate) fn arm_workflow_delete(&mut self, cx: &mut Context<Self>) {
        self.workflow_delete_confirm = true;
        cx.notify();
    }

    /// The delete confirm's cancel (TS `setConfirmDelete(false)`).
    pub(crate) fn cancel_workflow_delete(&mut self, cx: &mut Context<Self>) {
        self.workflow_delete_confirm = false;
        cx.notify();
    }

    /// Closes whichever workflow form is open (a header back).
    pub(crate) fn close_workflow_form(&mut self, cx: &mut Context<Self>) {
        self.workflow_start_form = None;
        self.workflow_amend_form = None;
        self.workflow_meta_form = None;
        self.workflow_form_subs.clear();
        cx.notify();
    }

    /// Flips one raw-payload toggle on the run page (TS `EventRow` /
    /// `RawData` `setOpen(!open)`): `ev-{sequence}` for a journal row,
    /// `node-{key}` for an expanded node, `artifact` / `art-items` for the
    /// viewer. View-local — the payload already sits in the snapshot.
    pub(crate) fn toggle_workflow_raw(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.workflow_raw_open.remove(key) {
            self.workflow_raw_open.insert(key.to_owned());
        }
        cx.notify();
    }

    /// Sets one phase card's fold (TS `phaseOpen[i] = !(phaseOpen[i] ?? i ===
    /// headSlice)`): the render side passes the flipped effective state, so
    /// an explicit entry always wins over the head default.
    pub(crate) fn set_workflow_phase_open(
        &mut self,
        slice_index: usize,
        open: bool,
        cx: &mut Context<Self>,
    ) {
        self.workflow_phase_open.insert(slice_index, open);
        cx.notify();
    }

    /// Sets one step row's expansion (TS `StepRow`'s `setOpen`).
    pub(crate) fn set_workflow_step_open(
        &mut self,
        key: &str,
        open: bool,
        cx: &mut Context<Self>,
    ) {
        if open {
            self.workflow_step_open.insert(key.to_owned());
        } else {
            self.workflow_step_open.remove(key);
        }
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
                        .text_size(px(12.))
                        .text_color(theme.danger_foreground)
                        .child(error),
                );
            }
        } else if let Some(area) = &form.free_json {
            let parsed = parse_args_input(&area.read(cx).value().to_string());
            body = body.child(
                div().flex().flex_col().gap_1().child(
                    div()
                        .text_size(px(12.))
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
                            .text_size(px(12.))
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
                                .text_size(px(12.))
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
                            .text_size(px(12.))
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
                                .text_size(px(12.))
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
                                    .text_size(px(13.))
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
            .text_size(px(12.))
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
        Phase, ProtocolRuntime, UiCommand, WorkflowArtifactViewerView, WorkflowDetailView,
        WorkflowNodeResultView, WorkflowRunTab, WorkflowRunView, WorkflowsPageView,
        WorkbenchSnapshot,
    };
    use crate::workbench::Workbench;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{
        px, AppContext as _, Bounds, Entity, Point, Size, TestAppContext, WindowBounds,
        WindowOptions,
    };
    use protocol::settings::{GateOverride, WorkflowGateBlock, WorkflowGateModeSetting};
    use protocol::workflow::{
        ConversationRunSummary, WorkflowArtifact, WorkflowDetailResponse, WorkflowEntry,
        WorkflowHubGroup, WorkflowOverviewResponse, WorkflowRunEvent, WorkflowRunRow,
        WorkflowScope, WorkflowWorkspaceNode,
    };
    use protocol::workflow_run_detail::ArtifactDataItem;
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
            launches: std::collections::HashMap::new(),
            session_runs: std::collections::HashMap::new(),
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
            tab: WorkflowRunTab::Events,
            artifacts: None,
            artifacts_busy: false,
            nodes: None,
            nodes_busy: false,
            node_result: None,
            node_busy: None,
            artifact: None,
        }
    }

    fn artifact(
        id: &str,
        kind: &str,
        title: Option<&str>,
        extra: serde_json::Value,
    ) -> WorkflowArtifact {
        let mut full = serde_json::json!({ "id": id, "kind": kind });
        if let Some(title) = title {
            full["title"] = serde_json::json!(title);
        }
        if let (Some(full_obj), Some(extra_obj)) = (full.as_object_mut(), extra.as_object()) {
            for (k, v) in extra_obj {
                full_obj.insert(k.clone(), v.clone());
            }
        }
        serde_json::from_value(full).expect("artifact parses")
    }

    fn viewer(
        art: WorkflowArtifact,
        text: Option<&str>,
        items: Option<Vec<ArtifactDataItem>>,
    ) -> WorkflowArtifactViewerView {
        WorkflowArtifactViewerView {
            artifact: art,
            busy: false,
            text: text.map(|t| std::sync::Arc::from(t)),
            binary: false,
            total_bytes: text.map(|t| t.len() as u64),
            next_offset: None,
            items,
            items_has_more: false,
        }
    }

    fn workspace_node(op: &str, args: serde_json::Value) -> WorkflowWorkspaceNode {
        serde_json::from_value(serde_json::json!({
            "siteId": "port", "ordinal": 2, "op": op, "args": args,
            "status": "completed",
            "summary": { "resultBytes": 1844, "exitCode": 0, "stdoutBytes": 100 },
            "createdAt": 1760526010000u64, "updatedAt": 1760526120000u64,
        }))
        .expect("node parses")
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

    /// Click `id` until the runtime records SOMETHING (bounded): at high
    /// suite parallelism the kit's pointer dispatch occasionally drops the
    /// very first click on a freshly opened window — the target resolves
    /// against a frame a concurrent suite-mate's scheduling displaced. A
    /// click that landed needs no retry, and a re-fired duplicate still
    /// satisfies the `any`-style assertions the callers make. Harness
    /// sensitivity only — no production behavior rides on this.
    fn click_recorded(
        window: &mut gpui_kit::Window,
        id: gpui_kit::SharedString,
        runtime: &ProtocolRuntime,
        cx: &mut gpui_kit::App,
    ) {
        let before = runtime.recorded_commands().len();
        for _ in 0..3 {
            window.click(id.clone(), cx);
            if runtime.recorded_commands().len() > before {
                return;
            }
        }
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

    /// The run view renders the journal through the protocol renderer —
    /// plain rows as event rows, node lifecycles folded into node lines
    /// inside their phase section, the phase timeline above — and the
    /// cursor's "Load more" dispatches the append read.
    #[gpui_kit::test]
    fn workflow_run_events_render_and_paginate(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let runtime = ProtocolRuntime::inert();
        let events = vec![
            event(1, "run-started", serde_json::json!({})),
            event(2, "phase-entered", serde_json::json!({ "name": "map", "ordinal": 1 })),
            event(
                3,
                "node-dispatched",
                serde_json::json!({
                    "actorName": "impl",
                    "instance": { "siteId": "site", "ordinal": 1 }
                }),
            ),
            event(4, "log", serde_json::json!({ "message": "hello" })),
            event(5, "phase-entered", serde_json::json!({ "name": "map", "ordinal": 2 })),
        ];
        let mut snap = WorkbenchSnapshot::initial();
        snap.phase = Phase::Connected;
        let mut view = page_view(None);
        view.run = Some(run_view(events, true));
        snap.workflows_page = Some(view);
        let (handle, _wb) = open_workbench(cx, runtime.clone(), snap);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            // The tab strip; the phase cards read newest-first — the "map"
            // slice (the newest one that paints; the empty ×2 re-entry
            // renders nothing) open on top, the prelude folded beneath it;
            // the folded step row and the log row inside "map", the
            // live-run header actions, and the cursor's next-page control
            // above the cards.
            assert!(window.find("workflow-run-tab-events").visible());
            assert!(
                window.try_find("workflow-run-phases").is_none(),
                "no standalone timeline — the card headers are it"
            );
            assert!(window.find("wf-phase-head-1").visible());
            assert!(window.find("wf-phase-section-1-map").visible());
            assert!(window.find("wf-node-line-site_1").visible());
            assert!(window.find("wf-event-4").visible());
            assert!(
                window.try_find("wf-event-1").is_none(),
                "the prelude folds shut by default"
            );
            assert!(
                window.try_find("wf-event-3").is_none(),
                "a node-* event folds into its step row, never a bare row"
            );
            assert!(window.find("workflow-run-stop").visible());
            assert!(window.find("workflow-run-amend-open").visible());
            assert!(
                window.try_find("workflow-run-resume").is_none(),
                "a flying run is not resumable"
            );
            assert!(window.find("workflow-run-load-newer").visible());
            window.click("workflow-run-load-newer", cx);
            let sent = runtime.recorded_commands();
            assert!(
                sent.iter().any(|c| matches!(c, UiCommand::WorkflowRunLoadMore)),
                "load newer must dispatch the append read, got {sent:?}"
            );
            // The prelude unfolds on its header click.
            window.click("wf-phase-head-0", cx);
            window.render_frame(cx);
            assert!(window.find("wf-event-1").visible());
            // The step unfolds into its own detail (site ref omitted).
            window.click("wf-step-head-site_1", cx);
            window.render_frame(cx);
            assert!(window.find("wf-step-body-site_1").visible());
            assert!(window.find("wf-event-3").visible());
        })
        .unwrap();
    }

    /// L3: a journal row's raw payload sits behind an explicit toggle —
    /// clicking the row opens the pretty JSON, clicking again closes it.
    #[gpui_kit::test]
    fn workflow_run_event_row_toggles_raw_payload(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let runtime = ProtocolRuntime::inert();
        let events = vec![event(7, "log", serde_json::json!({ "message": "hello" }))];
        let mut snap = WorkbenchSnapshot::initial();
        snap.phase = Phase::Connected;
        let mut view = page_view(None);
        view.run = Some(run_view(events, false));
        snap.workflows_page = Some(view);
        let (handle, _wb) = open_workbench(cx, runtime.clone(), snap);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("wf-raw-ev-7-body").is_none(), "summary by default");
            window.click("wf-event-7", cx);
            window.render_frame(cx);
            assert!(window.find("wf-raw-ev-7-body").visible(), "the raw JSON opens");
            window.click("wf-event-7", cx);
            window.render_frame(cx);
            assert!(window.try_find("wf-raw-ev-7-body").is_none(), "and closes again");
        })
        .unwrap();
    }

    /// L5: a settled, resumable run shows Resume and neither Stop nor
    /// Configure (both are live-run controls).
    #[gpui_kit::test]
    fn workflow_run_header_buttons_follow_state(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let runtime = ProtocolRuntime::inert();
        let mut snap = WorkbenchSnapshot::initial();
        snap.phase = Phase::Connected;
        let mut run = run_view(Vec::new(), false);
        run.active = false;
        if let Some(summary) = run.summary.as_mut() {
            summary.status = "cancelled".to_string();
            summary.resumable = true;
        }
        let mut view = page_view(None);
        view.run = Some(run);
        snap.workflows_page = Some(view);
        let (handle, _wb) = open_workbench(cx, runtime.clone(), snap);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("workflow-run-resume").visible());
            assert!(window.try_find("workflow-run-stop").is_none());
            assert!(window.try_find("workflow-run-amend-open").is_none());
            window.click("workflow-run-resume", cx);
            let sent = runtime.recorded_commands();
            assert!(
                sent.iter().any(|c| matches!(
                    c,
                    UiCommand::WorkflowResumeRun { run_id, session_id, instance_id, .. }
                        if run_id == "run-1" && session_id == "s1" && instance_id == "inst-a"
                )),
                "resume must dispatch on the run's address, got {sent:?}"
            );
        })
        .unwrap();
    }

    /// L1: picking the Artifacts tab dispatches the tab select (the lazy
    /// read rides the driver); a loaded list opens its viewer on click.
    #[gpui_kit::test]
    fn workflow_run_tabs_switch_and_artifacts_open(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let runtime = ProtocolRuntime::inert();
        let mut snap = WorkbenchSnapshot::initial();
        snap.phase = Phase::Connected;
        let mut run = run_view(Vec::new(), false);
        run.tab = WorkflowRunTab::Artifacts;
        run.artifacts = Some(vec![
            artifact(
                "art-0001",
                "markdown",
                Some("Checklist report"),
                serde_json::json!({ "versions": [{ "version": 1 }, { "version": 2 }] }),
            ),
            artifact("art-0003", "board", Some("Port board"), serde_json::json!({ "itemCount": 12 })),
        ]);
        let mut view = page_view(None);
        view.run = Some(run);
        snap.workflows_page = Some(view);
        let (handle, _wb) = open_workbench(cx, runtime.clone(), snap);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("wf-artifact-row-art-0001").visible());
            assert!(window.find("wf-artifact-row-art-0003").visible());
            window.click("workflow-run-tab-workspace", cx);
            window.click("wf-artifact-row-art-0001", cx);
            let sent = runtime.recorded_commands();
            assert!(
                sent.iter().any(|c| matches!(
                    c,
                    UiCommand::WorkflowRunSelectTab { tab } if *tab == WorkflowRunTab::Workspace
                )),
                "the tab pill must dispatch the select, got {sent:?}"
            );
            assert!(
                sent.iter().any(|c| matches!(
                    c,
                    UiCommand::WorkflowRunOpenArtifact { artifact_id } if artifact_id == "art-0001"
                )),
                "an artifact row must open its viewer, got {sent:?}"
            );
        })
        .unwrap();
    }

    /// L1: the artifact viewer draws the spec-driven table (declared
    /// columns, key dedupe) with the raw items one toggle away, and the page
    /// header's back closes the VIEWER first (the TS back ladder).
    #[gpui_kit::test]
    fn workflow_run_artifact_viewer_table_and_back(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let runtime = ProtocolRuntime::inert();
        let mut snap = WorkbenchSnapshot::initial();
        snap.phase = Phase::Connected;
        let mut run = run_view(Vec::new(), false);
        run.tab = WorkflowRunTab::Artifacts;
        let table = artifact(
            "art-0002",
            "table",
            Some("Port table"),
            serde_json::json!({
                "spec": {
                    "key": "title",
                    "columns": [{ "field": "title", "label": "File" }, { "field": "state" }]
                }
            }),
        );
        run.artifacts = Some(vec![table.clone()]);
        run.artifact = Some(viewer(
            table,
            None,
            Some(vec![
                ArtifactDataItem {
                    sequence: 1,
                    item: serde_json::json!({ "title": "hub.rs", "state": "todo" }),
                },
                ArtifactDataItem {
                    sequence: 2,
                    item: serde_json::json!({ "title": "hub.rs", "state": "done" }),
                },
            ]),
        ));
        let mut view = page_view(None);
        view.run = Some(run);
        snap.workflows_page = Some(view);
        let (handle, _wb) = open_workbench(cx, runtime.clone(), snap);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("workflow-artifact-viewer").visible());
            assert!(window.find("wf-table-view").visible());
            assert!(window.find("wf-table-row-0").visible());
            assert!(
                window.try_find("wf-table-row-1").is_none(),
                "the keyed replace folds two items into one row"
            );
            // The raw items toggle: closed, then open.
            assert!(window.try_find("wf-raw-art-items-body").is_none());
            window.click("wf-raw-art-items-toggle", cx);
            window.render_frame(cx);
            assert!(window.find("wf-raw-art-items-body").visible());
            // Back closes the viewer, not the run page.
            window.click("workflows-back", cx);
            let sent = runtime.recorded_commands();
            assert!(
                sent.iter().any(|c| matches!(c, UiCommand::WorkflowRunCloseArtifact)),
                "back must close the viewer first, got {sent:?}"
            );
            assert!(
                !sent.iter().any(|c| matches!(c, UiCommand::CloseWorkflowRun)),
                "the run page must stay open under the viewer"
            );
        })
        .unwrap();
    }

    /// L1: the workspace tab renders the op-tile rows; a click expands the
    /// node's result (one GET), and a landed result paints structured
    /// before raw — exit code + stdout, raw JSON behind the toggle.
    #[gpui_kit::test]
    fn workflow_run_workspace_rows_expand(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let runtime = ProtocolRuntime::inert();
        let mut snap = WorkbenchSnapshot::initial();
        snap.phase = Phase::Connected;
        let mut run = run_view(Vec::new(), false);
        run.tab = WorkflowRunTab::Workspace;
        run.nodes = Some(vec![workspace_node(
            "world.run",
            serde_json::json!({ "command": "cargo test -p protocol" }),
        )]);
        run.node_result = Some(WorkflowNodeResultView {
            key: "port:2".to_string(),
            status: Some("completed".to_string()),
            body: serde_json::json!({ "exitCode": 0, "stdout": "ok\n38 passed", "stderr": "" }),
            truncated: false,
        });
        let mut view = page_view(None);
        view.run = Some(run);
        snap.workflows_page = Some(view);
        let (handle, _wb) = open_workbench(cx, runtime.clone(), snap);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("wf-workspace-row-port_2").visible());
            assert!(window.find("wf-badge-workspace-port_2").visible());
            assert!(window.find("wf-node-result-port_2").visible());
            assert!(window.find("wf-node-result-port_2-stdout").visible());
            assert!(window.try_find("wf-raw-node-port:2-body").is_none());
            window.click("wf-raw-node-port:2-toggle", cx);
            window.render_frame(cx);
            assert!(window.find("wf-raw-node-port:2-body").visible());
            window.click("wf-workspace-row-port_2", cx);
            let sent = runtime.recorded_commands();
            assert!(
                sent.iter().any(|c| matches!(
                    c,
                    UiCommand::WorkflowRunToggleNode { key, site_id, ordinal }
                        if key == "port:2" && site_id == "port" && *ordinal == 2
                )),
                "the row must toggle its node result, got {sent:?}"
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

    /// Batch M (M1): the meta edit sheet — the whole-meta REPLACE merges the
    /// untouched rest back in (the args declaration survives), an emptied
    /// whenToUse DROPS the key, and a blank description keeps the submit
    /// disabled.
    #[gpui_kit::test]
    fn workflow_meta_form_merges_rest_and_drops_empty_when_to_use(cx: &mut TestAppContext) {
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
                serde_json::json!({ "target": { "type": "string", "required": true } }),
            ),
            (
                "description".to_string(),
                serde_json::json!("Deploy the app"),
            ),
            ("whenToUse".to_string(), serde_json::json!("When deploying")),
        ]);
        let mut view = page_view(None);
        view.detail = Some(detail_view(meta));
        snap.workflows_page = Some(view);
        let (handle, wb) = open_workbench(cx, runtime.clone(), snap);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            // The identity card's edit entry opens the sheet, prefilled.
            window.click("workflow-meta-edit-open", cx);
            window.render_frame(cx);
            assert!(window.find("workflow-meta-form").visible());
            // A cleared description disables the submit — nothing dispatches.
            wb.update(cx, |wb, cx| {
                let form = wb.workflow_meta_form.as_ref().expect("form open");
                form.description
                    .update(cx, |s, cx| s.set_value("   ", window, cx));
                cx.notify();
            });
            window.render_frame(cx);
            window.click("workflow-meta-submit", cx);
            assert!(
                !runtime
                    .recorded_commands()
                    .iter()
                    .any(|c| matches!(c, UiCommand::UpdateWorkflowMeta { .. })),
                "a blank description must not write"
            );
            // A new description with an EMPTIED whenToUse: the args rest
            // merges back, whenToUse is absent (never "").
            wb.update(cx, |wb, cx| {
                let form = wb.workflow_meta_form.as_ref().expect("form open");
                form.description
                    .update(cx, |s, cx| s.set_value("Ship it", window, cx));
                form.when_to_use
                    .update(cx, |s, cx| s.set_value("", window, cx));
                cx.notify();
            });
            window.render_frame(cx);
            window.click("workflow-meta-submit", cx);
            let sent = runtime.recorded_commands();
            assert!(
                sent.iter().any(|c| matches!(
                    c,
                    UiCommand::UpdateWorkflowMeta { scope, name, instance_id, meta }
                        if *scope == WorkflowScope::Global
                            && name == "deploy"
                            && instance_id == "inst-a"
                            && *meta == serde_json::json!({
                                "args": { "target": { "type": "string", "required": true } },
                                "description": "Ship it",
                            })
                )),
                "the submit must carry the merged whole meta, got {sent:?}"
            );
        })
        .unwrap();
    }

    /// Batch M (M2 + M3): a GLOBAL workflow offers the REST move to project
    /// (the project scope keeps the chat-prefill promote), and delete is a
    /// two-step confirm — the arm alone dispatches nothing.
    #[gpui_kit::test]
    fn workflow_detail_move_and_two_step_delete(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let runtime = ProtocolRuntime::inert();
        let mut snap = WorkbenchSnapshot::initial();
        snap.phase = Phase::Connected;
        let mut view = page_view(None);
        view.detail = Some(detail_view(BTreeMap::new()));
        snap.workflows_page = Some(view);
        let (handle, _wb) = open_workbench(cx, runtime.clone(), snap);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            // Global scope: move-to-project present, promote absent.
            assert!(window.find("workflow-move-project").visible());
            assert!(
                window.try_find("workflow-promote-global").is_none(),
                "promote is the PROJECT scope's direction"
            );
            window.click("workflow-move-project", cx);
            let sent = runtime.recorded_commands();
            assert!(
                sent.iter().any(|c| matches!(
                    c,
                    UiCommand::MoveWorkflowToProject { scope, name, instance_id }
                        if *scope == WorkflowScope::Global
                            && name == "deploy"
                            && instance_id == "inst-a"
                )),
                "move must dispatch the REST route, got {sent:?}"
            );
            // Delete arms, cancels, arms again, then fires.
            assert!(window.try_find("workflow-delete-confirm").is_none());
            window.click("workflow-delete-arm", cx);
            window.render_frame(cx);
            assert!(window.find("workflow-delete-cancel").visible());
            window.click("workflow-delete-cancel", cx);
            window.render_frame(cx);
            assert!(window.try_find("workflow-delete-confirm").is_none());
            window.click("workflow-delete-arm", cx);
            window.render_frame(cx);
            window.click("workflow-delete-confirm", cx);
            let sent = runtime.recorded_commands();
            assert!(
                sent.iter().any(|c| matches!(
                    c,
                    UiCommand::DeleteWorkflow { scope, name, instance_id }
                        if *scope == WorkflowScope::Global
                            && name == "deploy"
                            && instance_id == "inst-a"
                )),
                "the confirmed delete must dispatch, got {sent:?}"
            );
        })
        .unwrap();
    }

    /// Batch M (M4): the script renders read-only behind an explicit
    /// Show/Hide fold — closed by default, one click opens, another closes.
    #[gpui_kit::test]
    fn workflow_detail_script_folds(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let runtime = ProtocolRuntime::inert();
        let mut snap = WorkbenchSnapshot::initial();
        snap.phase = Phase::Connected;
        let mut view = page_view(None);
        view.detail = Some(detail_view(BTreeMap::new()));
        snap.workflows_page = Some(view);
        let (handle, _wb) = open_workbench(cx, runtime.clone(), snap);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("workflow-script-toggle").visible());
            assert!(
                window.try_find("workflow-script-body").is_none(),
                "the script is folded by default"
            );
            window.click("workflow-script-toggle", cx);
            window.render_frame(cx);
            assert!(window.find("workflow-script-body").visible());
            window.click("workflow-script-toggle", cx);
            window.render_frame(cx);
            assert!(window.try_find("workflow-script-body").is_none());
        })
        .unwrap();
    }

    /// Batch M (M5 + M7): zone rows without a bridge-joined session resolve
    /// through the launch memory into actionable rows — Stop addresses the
    /// launch's own instance/session, the recent row carries its chips line
    /// and dismisses without a session at all.
    #[gpui_kit::test]
    fn launch_memory_makes_zone_rows_actionable(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let runtime = ProtocolRuntime::inert();
        let mut overview = overview_fixture();
        let mut active = run_row("run-7", "deploy", "running", None);
        active.owner_instance_id = None;
        let mut recent = run_row("run-8", "deploy", "completed", None);
        recent.owner_instance_id = None;
        overview.active_runs = vec![active];
        overview.recent_runs = vec![recent];
        let mut view = page_view(Some(overview));
        view.launches = [
            (
                "run-7",
                crate::runtime::WorkflowLaunchRef {
                    instance_id: "inst-launch".to_string(),
                    acp_session_id: "s-launch".to_string(),
                },
            ),
            (
                "run-8",
                crate::runtime::WorkflowLaunchRef {
                    instance_id: "inst-launch".to_string(),
                    acp_session_id: "s-recent".to_string(),
                },
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let mut snap = WorkbenchSnapshot::initial();
        snap.phase = Phase::Connected;
        snap.workflows_page = Some(view);
        let (handle, _wb) = open_workbench(cx, runtime.clone(), snap);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            // The active row: Detail + Stop on the launch's own address.
            assert!(window.find("wf-active-0-detail").visible());
            assert!(window.find("wf-active-0-stop-run-7").visible());
            click_recorded(
                window,
                gpui_kit::SharedString::from("wf-active-0-stop-run-7"),
                &runtime,
                cx,
            );
            let sent = runtime.recorded_commands();
            assert!(
                sent.iter().any(|c| matches!(
                    c,
                    UiCommand::WorkflowStopRun { run_id, session_id, instance_id }
                        if run_id == "run-7"
                            && session_id == "s-launch"
                            && instance_id.as_deref() == Some("inst-launch")
                )),
                "stop must ride the launch memory's address, got {sent:?}"
            );
            // Repaint between the two interactions (the kit click rides
            // the last completed frame; a stale one can drop the second
            // click under load).
            window.render_frame(cx);
            // The recent row: chips line + Detail + dismiss (no session
            // needed for the dismiss).
            assert!(window.find("wf-recent-0-meta").visible());
            assert!(window.find("wf-recent-0-detail").visible());
            assert!(window.find("wf-recent-0-dismiss-run-8").visible());
            click_recorded(
                window,
                gpui_kit::SharedString::from("wf-recent-0-detail"),
                &runtime,
                cx,
            );
            let sent = runtime.recorded_commands();
            assert!(
                sent.iter().any(|c| matches!(
                    c,
                    UiCommand::OpenWorkflowRun { run_id, session_id, instance_id, .. }
                        if run_id == "run-8"
                            && session_id == "s-recent"
                            && instance_id == "inst-launch"
                )),
                "detail must ride the launch memory's address, got {sent:?}"
            );
        })
        .unwrap();
    }

    /// The third session ladder (TS ActiveRunsBlock's `sessionRuns`): an
    /// active run neither launch memory nor the bridge join could place
    /// still gets Detail/Stop when the CURRENT session's conversationRuns
    /// poll reported it — and the join is the ACTIVE zone's alone: a
    /// sessionless recent row stays read-only (TS RecentRunsBlock).
    #[gpui_kit::test]
    fn session_runs_join_makes_active_rows_actionable(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let runtime = ProtocolRuntime::inert();
        let mut overview = overview_fixture();
        let mut active = run_row("run-9x", "deploy", "running", None);
        active.owner_instance_id = None;
        let mut recent = run_row("run-8x", "deploy", "completed", None);
        recent.owner_instance_id = None;
        overview.active_runs = vec![active];
        overview.recent_runs = vec![recent];
        let mut view = page_view(Some(overview));
        view.session_runs = [("run-9x".to_string(), "s-cur".to_string())]
            .into_iter()
            .collect();
        let mut snap = WorkbenchSnapshot::initial();
        snap.phase = Phase::Connected;
        snap.workflows_page = Some(view);
        let (handle, _wb) = open_workbench(cx, runtime.clone(), snap);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            // The active row resolves through the join; the instance falls
            // back to the connected tab's (the join's session lives there).
            assert!(window.find("wf-active-0-detail").visible());
            assert!(window.find("wf-active-0-stop-run-9x").visible());
            click_recorded(
                window,
                gpui_kit::SharedString::from("wf-active-0-stop-run-9x"),
                &runtime,
                cx,
            );
            let sent = runtime.recorded_commands();
            assert!(
                sent.iter().any(|c| matches!(
                    c,
                    UiCommand::WorkflowStopRun { run_id, session_id, instance_id }
                        if run_id == "run-9x"
                            && session_id == "s-cur"
                            && instance_id.as_deref() == Some("inst-a")
                )),
                "stop must ride the sessionRuns join's address, got {sent:?}"
            );
            window.render_frame(cx);
            // The recent row has no join and no session of its own: the
            // detail affordance stays hidden (read-only visibility row).
            assert!(
                window.try_find("wf-recent-0-detail").is_none(),
                "the third ladder must not leak into the recent zone"
            );
            assert!(window.find("wf-recent-0-dismiss-run-8x").visible());
        })
        .unwrap();
    }

    /// Clear finished (TS `clearFinished`): the control shows only while a
    /// settled row exists, and one click dismiss-batches exactly the
    /// settled ids — still-flying rows never ride along.
    #[gpui_kit::test]
    fn clear_finished_dismisses_only_settled_rows(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let runtime = ProtocolRuntime::inert();
        let mut detail = detail_view(BTreeMap::new());
        if let Some(runs) = detail.runs.as_mut() {
            runs.push(run_row("run-4", "deploy", "running", Some("s4")));
        }
        let mut view = page_view(None);
        view.detail = Some(detail);
        let mut snap = WorkbenchSnapshot::initial();
        snap.phase = Phase::Connected;
        snap.workflows_page = Some(view);
        let (handle, _wb) = open_workbench(cx, runtime.clone(), snap);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("wf-clear-finished").visible());
            click_recorded(window, "wf-clear-finished".into(), &runtime, cx);
            let sent = runtime.recorded_commands();
            assert!(
                sent.iter().any(|c| matches!(
                    c,
                    UiCommand::DismissWorkflowRuns { instance_id, run_ids }
                        if instance_id == "inst-a"
                            && run_ids.as_slice() == ["run-3".to_string()]
                )),
                "clear finished must batch exactly the settled ids, got {sent:?}"
            );
        })
        .unwrap();

        // All-active history: nothing to clear, the control hides.
        let runtime2 = ProtocolRuntime::inert();
        let mut detail = detail_view(BTreeMap::new());
        if let Some(runs) = detail.runs.as_mut() {
            runs.clear();
            runs.push(run_row("run-5", "deploy", "running", Some("s5")));
        }
        let mut view = page_view(None);
        view.detail = Some(detail);
        let mut snap = WorkbenchSnapshot::initial();
        snap.phase = Phase::Connected;
        snap.workflows_page = Some(view);
        let (handle, _wb) = open_workbench(cx, runtime2, snap);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window.try_find("wf-clear-finished").is_none(),
                "an all-active history has nothing to clear"
            );
        })
        .unwrap();
    }

    /// The merge behind the meta write (plain logic, no window): rest
    /// preserved, old whenToUse stripped, empty new whenToUse dropped,
    /// blank description refused.
    #[test]
    fn merge_workflow_meta_shapes() {
        let existing = BTreeMap::from([
            (
                "args".to_string(),
                serde_json::json!({ "target": { "type": "string" } }),
            ),
            ("description".to_string(), serde_json::json!("old")),
            ("whenToUse".to_string(), serde_json::json!("stale")),
        ]);
        let merged = super::merge_workflow_meta(
            Some(&existing),
            "  Ship it  ",
            "  when downtime allows  ",
        )
        .expect("non-empty description merges");
        assert_eq!(
            merged,
            serde_json::json!({
                "args": { "target": { "type": "string" } },
                "description": "Ship it",
                "whenToUse": "when downtime allows",
            })
        );
        // An emptied whenToUse drops the key (never ""), and the old value
        // must not ride the spread back in.
        let merged = super::merge_workflow_meta(Some(&existing), "Ship it", "  ").unwrap();
        assert_eq!(
            merged,
            serde_json::json!({
                "args": { "target": { "type": "string" } },
                "description": "Ship it",
            })
        );
        // A blank description is refused outright (required upstream).
        assert!(super::merge_workflow_meta(Some(&existing), "   ", "x").is_none());
        assert!(super::merge_workflow_meta(None, "", "x").is_none());
        // No loaded detail yet still produces a valid minimal object.
        assert_eq!(
            super::merge_workflow_meta(None, "D", "").unwrap(),
            serde_json::json!({ "description": "D" })
        );
    }
}
