//! The inline Approval Card (CONTEXT.md): the pending permission /
//! elicitation ask of the ACTIVE session rendered as a card in the chat
//! stream — the desktop shape of the mobile `PermissionDialog` /
//! `ElicitationDialog` pair (requirements §2: "approval cards render as
//! inline cards in the chat stream (not dialogs), fitting the workbench
//! density").
//!
//! Three shapes, classified exactly like PermissionDialog.tsx:
//! - Plan Approval (ExitPlanMode) — plan markdown + fixed Approve/Reject.
//! - Tool Permission — tool name, INPUT_KEYS highlights (raw-JSON
//!   fallback), option list styled by option kind.
//! - Question (AskUserQuestion) — question text + options. Over the
//!   elicitation.form channel (which this client declares at initialize)
//!   the bridge sends the richer `elicitation/create` form: one field per
//!   question with enum options and a free-text companion; the
//!   plan-approval FORM is the single approve/reject field whose
//!   description carries the complete plan.
//!
//! Card strings come from the shared rust-i18n catalogs (`permission.*`,
//! `common.*`; requirements §2). Fallback labels for what the server did not
//! send (Approve/Reject wording, the other-value placeholders) are
//! desktop-specific and stay English — the catalogs carry no keys for them
//! and forking them in is what §2 forbids.

use std::collections::HashMap;
use std::time::Duration;


use gpui::prelude::FluentBuilder as _;
use gpui_kit::component;
use gpui_kit::*;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::text::{TextView, TextViewMotion, TextViewState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _};
use protocol::approval::{
    build_elicitation_answers, permission_shape, plan_field, raw_input_highlights, ApprovalShape,
    ElicitValue, PendingAsk,
};
use rust_i18n::t;

use crate::runtime::{ProtocolRuntime, UiCommand};
use crate::workbench::Workbench;

/// Stream-reveal timing matching the chat rows (workbench's constants).
const STREAM_FADE_MS: u64 = 280;
const STREAM_FADE_STAGGER_MS: u64 = 10;

/// Interactive state of one approval card, owned as an entity so the chat
/// scroller's row renderer can read it and the option buttons can update it.
/// Rebuilt whenever the pending ask (request id + channel) changes.
pub struct ApprovalCard {
    ask: PendingAsk,
    /// Single-select picks (field key -> option value) — elicitation forms.
    picked: HashMap<String, String>,
    /// Multi-select toggles (option value -> on).
    checked: HashMap<String, bool>,
    /// Free-text companions (`q_<i>_other`), keyed by the other field key.
    others: HashMap<String, Entity<TextareaState>>,
    /// Markdown body: the plan (Plan shapes) or question text.
    body: Option<Entity<TextViewState>>,
    /// Per-field description markdown (question forms).
    descriptions: Vec<Entity<TextViewState>>,
    /// The tool shape's raw-input pane fold (TS PermissionDialog's
    /// `<details>` — closed by default, the summary is the tool name).
    raw_open: bool,
}

/// One markdown pane entity for the card (plan / question / descriptions).
fn mk_markdown(cx: &mut Context<ApprovalCard>, text: &str) -> Entity<TextViewState> {
    cx.new(|cx| {
        let mut state = TextViewState::markdown(text, cx);
        state.set_motion(stream_motion());
        state
    })
}

impl ApprovalCard {
    /// Identity for rebuild decisions: the request id plus its channel.
    pub fn key(&self) -> (u64, bool) {
        (
            self.ask.request_id(),
            matches!(self.ask, PendingAsk::Elicitation(_)),
        )
    }

    pub fn new(window: &mut Window, ask: PendingAsk, cx: &mut Context<Self>) -> Self {
        let markdown = |text: &str, cx: &mut Context<Self>| mk_markdown(cx, text);
        let mut body = None;
        let mut descriptions = Vec::new();
        let mut others = HashMap::new();
        match &ask {
            PendingAsk::Permission(p) => match permission_shape(p) {
                ApprovalShape::Plan => {
                    // PermissionDialog.tsx bodyText: ctx.plan ?? ctx.detail.
                    let text = p
                        .context
                        .as_ref()
                        .and_then(|c| c.plan.clone().or_else(|| c.detail.clone()))
                        .filter(|t| !t.is_empty());
                    if let Some(text) = text {
                        body = Some(markdown(&text, cx));
                    }
                }
                ApprovalShape::Question => {
                    let text = p
                        .context
                        .as_ref()
                        .and_then(|c| c.detail.clone().or_else(|| c.title.clone()))
                        .filter(|t| !t.is_empty());
                    if let Some(text) = text {
                        body = Some(markdown(&text, cx));
                    }
                }
                ApprovalShape::Tool => {}
            },
            PendingAsk::Elicitation(form) => {
                if let Some(field) = plan_field(form) {
                    // Plan-approval FORM: the description carries the plan.
                    if let Some(text) = field.description.as_ref().filter(|t| !t.is_empty()) {
                        body = Some(markdown(text, cx));
                    }
                } else {
                    for field in &form.fields {
                        if let Some(text) = field.description.as_ref().filter(|t| !t.is_empty()) {
                            descriptions.push(markdown(text, cx));
                        }
                        if let Some(key) = &field.other_key {
                            let state = cx.new(|cx| {
                                TextareaState::new(window, cx)
                                    .placeholder(if field.multi {
                                        "or a custom value"
                                    } else {
                                        "or type a custom value"
                                    })
                                    .submit_on_enter(false)
                            });
                            others.insert(key.clone(), state);
                        }
                    }
                }
            }
        }
        Self {
            ask,
            picked: HashMap::new(),
            checked: HashMap::new(),
            others,
            body,
            descriptions,
            raw_open: false,
        }
    }
}

fn stream_motion() -> TextViewMotion {
    TextViewMotion::default()
        .with_stream_fade(Duration::from_millis(STREAM_FADE_MS))
        .with_stream_fade_stagger(Duration::from_millis(STREAM_FADE_STAGGER_MS))
}

/// Renders the card row (the scroller's last row while the ask is pending).
/// `workbench` is the weak self-handle for re-render notifications after a
/// local pick/toggle.
pub fn render_approval_card(
    card: &Entity<ApprovalCard>,
    runtime: &ProtocolRuntime,
    workbench: &WeakEntity<Workbench>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let state = card.read(cx);

    let mut column = div().flex().flex_col().gap_2();
    match &state.ask {
        PendingAsk::Permission(p) => match permission_shape(p) {
            ApprovalShape::Plan => {
                column = column
                    .child(card_title(&t!("permission.planTitle"), theme))
                    .when_some(state.body.clone(), |this, body| {
                        this.child(
                            div()
                                .max_w(rems(46.))
                                .child(TextView::new(&body)),
                        )
                    });
                // The two options are fixed and well-known — make them the
                // primary actions (ActionButtons plan branch; the approve
                // match is TS `kind?.startsWith("allow")`, any allow_* kind).
                let approve = p.options.iter().find(|o| {
                    o.option_id == "approve"
                        || o.kind.as_deref().is_some_and(|k| k.starts_with("allow"))
                });
                let reject = p.options.iter().find(|o| Some(*o) != approve);
                let request_id = p.request_id;
                if let Some(approve) = approve {
                    let option_id = approve.option_id.clone();
                    let label = approve
                        .name
                        .clone()
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| "Approve — exit plan mode".to_string());
                    let runtime = runtime.clone();
                    column = column.child(
                        Button::new(SharedString::from(format!("approve-{request_id}")))
                            .success()
                            .label(label)
                            .on_click(move |_, _, _| {
                                runtime.send(UiCommand::AnswerPermission {
                                    request_id,
                                    option_id: option_id.clone(),
                                });
                            }),
                    );
                }
                if let Some(reject) = reject {
                    let option_id = reject.option_id.clone();
                    let label = reject
                        .name
                        .clone()
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| "Reject — keep planning".to_string());
                    let runtime = runtime.clone();
                    column = column.child(
                        Button::new(SharedString::from(format!("reject-{request_id}")))
                            .danger()
                            .outline()
                            .label(label)
                            .on_click(move |_, _, _| {
                                runtime.send(UiCommand::AnswerPermission {
                                    request_id,
                                    option_id: option_id.clone(),
                                });
                            }),
                    );
                }
            }
            ApprovalShape::Question => {
                column = column.child(card_title(&t!("permission.questionTitle"), theme));
                if let Some(body) = state.body.clone() {
                    column = column.child(TextView::new(&body));
                }
                column = column.child(permission_option_buttons(p, runtime, theme));
            }
            ApprovalShape::Tool => {
                column = column.child(card_title(&t!("permission.title"), theme));
                let ctx = p.context.as_ref();
                if let Some(name) =
                    ctx.and_then(|c| c.tool_name.clone().or_else(|| c.title.clone()))
                {
                    column = column.child(
                        div()
                            .font_family(theme.mono_font_family.clone())
                            .text_size(px(12.))
                            .text_color(theme.warning)
                            .child(name),
                    );
                }
                // Highlights from the raw tool input; unknown input falls
                // back to the raw JSON pane.
                let highlights = ctx.map(raw_input_highlights).unwrap_or_default();
                for h in &highlights {
                    column = column.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(theme.muted_foreground)
                                    .child(h.label.to_uppercase()),
                            )
                            .child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .bg(theme.background)
                                    .font_family(theme.mono_font_family.clone())
                                    .text_size(px(12.))
                                    .text_color(theme.muted_foreground)
                                    .child(h.value.clone()),
                            ),
                    );
                }
                if highlights.is_empty() {
                    if let Some(raw) =
                        ctx.and_then(|c| c.raw_input_text.clone()).filter(|t| !t.is_empty())
                    {
                        // TS PermissionDialog:176-189 — the raw input folds
                        // behind a <details> (closed by default) whose body
                        // is capped at max-h-40 and scrolls.
                        let open = state.raw_open;
                        let card = card.clone();
                        let workbench = workbench.clone();
                        // The summary is `ctx.toolName ?? "input"`, verbatim TS.
                        let summary = ctx
                            .and_then(|c| c.tool_name.clone())
                            .unwrap_or_else(|| "input".to_string());
                        column = column.child(
                            div()
                                .id("approval-raw-toggle")
                                .flex()
                                .items_center()
                                .gap_1()
                                .text_size(px(10.))
                                .text_color(theme.muted_foreground)
                                .cursor_pointer()
                                .child(if open { "▾" } else { "▸" })
                                .child(summary)
                                .on_click(move |_, _, cx| {
                                    card.update(cx, |state, _| state.raw_open = !state.raw_open);
                                    if let Some(wb) = workbench.upgrade() {
                                        wb.update(cx, |_, cx| cx.notify());
                                    }
                                }),
                        );
                        if open {
                            column = column.child(
                                div()
                                    .id("approval-raw-body")
                                    .max_h(px(160.))
                                    .overflow_y_scroll()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .bg(theme.background)
                                    .font_family(theme.mono_font_family.clone())
                                    .text_size(px(12.))
                                    .text_color(theme.muted_foreground)
                                    .child(raw),
                            );
                        }
                    } else if let Some(first) = p.options.first() {
                        // No context at all: degrade gracefully to the first
                        // option's name (PermissionDialog's fallback).
                        column = column.child(
                            div()
                                .text_size(px(13.))
                            .text_color(theme.muted_foreground)
                            .child(first.name.clone().unwrap_or_else(|| t!("permission.title").into_owned())),
                        );
                    }
                }
                column = column.child(permission_option_buttons(p, runtime, theme));
            }
        },
        PendingAsk::Elicitation(form) => {
            if let Some(field) = plan_field(form) {
                // Plan-approval form: direct-answer buttons (the
                // ElicitationDialog planField branch).
                let request_id = form.request_id;
                let key = field.key.clone();
                let title = if form.message.is_empty() {
                    t!("permission.planTitle").into_owned()
                } else {
                    form.message.clone()
                };
                column = column.child(card_title(&title, theme));
                if let Some(body) = state.body.clone() {
                    column = column.child(
                        div().max_w(rems(46.)).child(TextView::new(&body)),
                    );
                }
                let approve_label = field
                    .options
                    .iter()
                    .find(|o| o.value == "approve")
                    .map(|o| o.label.clone())
                    .unwrap_or_else(|| "Approve".to_string());
                let reject_label = field
                    .options
                    .iter()
                    .find(|o| o.value == "reject")
                    .map(|o| o.label.clone())
                    .unwrap_or_else(|| "Reject".to_string());
                let mk = |value: &'static str| {
                    let runtime = runtime.clone();
                    let key = key.clone();
                    move |_: &ClickEvent, _: &mut Window, _: &mut App| {
                        runtime.send(UiCommand::AnswerElicitation {
                            request_id,
                            answers: Some(vec![(key.clone(), ElicitValue::One(value.to_string()))]),
                        });
                    }
                };
                column = column
                    .child(
                        Button::new(SharedString::from(format!("plan-approve-{request_id}")))
                            .success()
                            .label(approve_label)
                            .on_click(mk("approve")),
                    )
                    .child(
                        Button::new(SharedString::from(format!("plan-reject-{request_id}")))
                            .danger()
                            .outline()
                            .label(reject_label)
                            .on_click(mk("reject")),
                    );
                let runtime = runtime.clone();
                column = column.child(
                    Button::new(SharedString::from(format!("plan-cancel-{request_id}")))
                        .secondary()
                        .outline()
                        .label(t!("common.cancel").into_owned())
                        .on_click(move |_, _, _| {
                            runtime.send(UiCommand::AnswerElicitation {
                                request_id,
                                answers: None,
                            });
                        }),
                );
            } else {
                // Question form: one block per field — question, description
                // markdown, options (single = pick; multi = toggles), and the
                // free-text companion.
                column = column.child(card_title(
                    &if form.message.is_empty() {
                        t!("permission.questionTitle").into_owned()
                    } else {
                        form.message.clone()
                    },
                    theme,
                ));
                let mut description_index = 0;
                for field in &form.fields {
                    let mut block = div().flex().flex_col().gap_1();
                    block = block.child(
                        div()
                            .text_size(px(13.))
                            .text_color(theme.foreground)
                            .child(field.question.clone()),
                    );
                    if let Some(description) = state.descriptions.get(description_index) {
                        description_index += 1;
                        block = block.child(TextView::new(description));
                    }
                    for option in &field.options {
                        let on = if field.multi {
                            state.checked.get(&option.value).copied().unwrap_or(false)
                        } else {
                            state.picked.get(&field.key).map(String::as_str)
                                == Some(option.value.as_str())
                        };
                        let mut button = if on {
                            Button::new(SharedString::from(format!(
                                "opt-{}-{}",
                                form.request_id,
                                slug(&option.value)
                            )))
                            .primary()
                        } else {
                            Button::new(SharedString::from(format!(
                                "opt-{}-{}",
                                form.request_id,
                                slug(&option.value)
                            )))
                            .secondary()
                            .outline()
                        };
                        button = button.label(option.label.clone());
                        if field.multi {
                            let prefix = if on { "✓ " } else { "· " };
                            button = button.label(format!("{prefix}{}", option.label));
                        }
                        let card = card.clone();
                        let workbench = workbench.clone();
                        let key = field.key.clone();
                        let value = option.value.clone();
                        let multi = field.multi;
                        button = button.on_click(move |_, _, cx| {
                            card.update(cx, |state, _| {
                                if multi {
                                    let entry = state.checked.entry(value.clone()).or_insert(false);
                                    *entry = !*entry;
                                } else {
                                    state.picked.insert(key.clone(), value.clone());
                                }
                            });
                            if let Some(wb) = workbench.upgrade() {
                                wb.update(cx, |_, cx| cx.notify());
                            }
                        });
                        block = block.child(button);
                    }
                    if let Some(other_key) = &field.other_key {
                        if let Some(input) = state.others.get(other_key) {
                            block = block.child(
                                div()
                                    .pt_1()
                                    .child(div().max_w(rems(46.)).child(Textarea::new(input).h(px(36.)))),
                            );
                        }
                    }
                    column = column.child(block);
                }
                // Submit: fold the local picks into the form content
                // (ElicitationDialog's submit); Cancel declines the form.
                let request_id = form.request_id;
                let card = card.clone();
                let submit_runtime = runtime.clone();
                let fields = form.fields.clone();
                column = column.child(
                    Button::new(SharedString::from(format!("submit-{request_id}")))
                        .primary()
                        .label(t!("permission.submit").into_owned())
                        .on_click(move |_, _, cx| {
                            let state = card.read(cx);
                            let mut others: HashMap<String, String> = HashMap::new();
                            for (key, input) in &state.others {
                                others.insert(key.clone(), input.read(cx).value().to_string());
                            }
                            let answers = build_elicitation_answers(
                                &fields,
                                &state.picked,
                                &state.checked,
                                &others,
                            );
                            submit_runtime.send(UiCommand::AnswerElicitation {
                                request_id,
                                answers: Some(answers),
                            });
                        }),
                );
                let runtime = runtime.clone();
                column = column.child(
                    Button::new(SharedString::from(format!("cancel-{request_id}")))
                        .secondary()
                        .outline()
                        .label(t!("common.cancel").into_owned())
                        .on_click(move |_, _, _| {
                            runtime.send(UiCommand::AnswerElicitation {
                                request_id,
                                answers: None,
                            });
                        }),
                );
            }
        }
    }

    // The Permission Race hint (permission.hint).
    column = column.child(
        div()
            .text_size(px(10.))
            .text_color(theme.muted_foreground)
            .child(t!("permission.hint").into_owned()),
    );

    div()
        .flex()
        .gap_3()
        .child(
            div()
                .w_12()
                .flex_none()
                .text_size(px(12.))
                .text_color(theme.warning)
                .child("Ask"),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_2()
                .px_3()
                .py_3()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.title_bar)
                .child(column),
        )
        .into_any_element()
}

/// The option list styled by kind (PermissionDialog.tsx `optionClass`).
fn permission_option_buttons(
    p: &protocol::approval::PendingPermission,
    runtime: &ProtocolRuntime,
    theme: &component::Theme,
) -> gpui::Div {
    let _ = theme;
    let mut list = div().flex().flex_col().gap_2();
    for option in &p.options {
        let kind = option.kind.as_deref().unwrap_or("");
        let mut button = match kind {
            "allow_once" => Button::new(SharedString::from(format!("perm-{}-{}", p.request_id, slug(&option.option_id))))
                .primary(),
            "allow_always" | "allow_project" | "allow_session" => {
                Button::new(SharedString::from(format!("perm-{}-{}", p.request_id, slug(&option.option_id))))
                    .secondary()
                    .outline()
            }
            k if k.starts_with("reject") => {
                Button::new(SharedString::from(format!("perm-{}-{}", p.request_id, slug(&option.option_id))))
                    .danger()
                    .outline()
            }
            _ => Button::new(SharedString::from(format!("perm-{}-{}", p.request_id, slug(&option.option_id))))
                .secondary(),
        };
        let label = option
            .name
            .clone()
            .filter(|n| !n.is_empty())
            .or_else(|| option.kind.clone())
            .unwrap_or_else(|| option.option_id.clone());
        button = button.label(label);
        let runtime = runtime.clone();
        let request_id = p.request_id;
        let option_id = option.option_id.clone();
        button = button.on_click(move |_, _, _| {
            runtime.send(UiCommand::AnswerPermission {
                request_id,
                option_id: option_id.clone(),
            });
        });
        list = list.child(button);
    }
    list
}

fn card_title(text: &str, theme: &component::Theme) -> gpui::Div {
    div()
        .text_size(px(13.))
        .text_color(theme.foreground)
        .child(text.to_string())
}

/// Element-id-safe slug for option ids (they can contain ':', spaces, …).
fn slug(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect()
}

/// The ephemeral toast strip (notify-toast semantics live in the driver):
/// text plus a dismiss control, rendered above the composer.
pub fn render_toast_strip(
    runtime: &ProtocolRuntime,
    text: &str,
    theme: &component::Theme,
) -> AnyElement {
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .py_1()
        .border_t_1()
        .border_color(theme.border)
        .bg(theme.title_bar)
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .text_size(px(12.))
                .text_color(theme.foreground)
                .child(text.to_string()),
        )
        .child(
            Button::new("toast-dismiss")
                .ghost()
                .xsmall()
                .label("×")
                .tooltip("Dismiss")
                .on_click({
                    let runtime = runtime.clone();
                    move |_, _, _| runtime.send(UiCommand::DismissToast)
                }),
        )
        .into_any_element()
}
