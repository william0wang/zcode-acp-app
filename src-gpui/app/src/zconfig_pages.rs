//! The five ZCode-config section pages (batch I, requirements §7): Skills,
//! MCP, Hooks, Agents, Backups — the desktop ports of
//! `src/screens/config/{Skills,Mcp,Hooks,Agents,Backups}Page.tsx`, one
//! shared panel shape:
//!
//! - the payload is a raw `Value` decoded at paint time through
//!   `protocol::config_decode` — the module the TS rule says the screens
//!   must call, never fork;
//! - every write goes through the applyConfigWrite path (label, write,
//!   reload): the driver surfaces the Effect Class — `immediate` is done,
//!   `needs-restart` arms the shared Backend Restart affordance below the
//!   list (CONTEXT.md), never an automatic restart;
//! - the Skills and Agents enable switches are immediate; everything on the
//!   MCP / Hooks screens needs a restart (the bridge re-reads its config
//!   only when its backend respawns);
//! - Hooks edits ONE existing entry (command + timeout + enabled) — the
//!   route offers no add/remove, so neither does the screen;
//! - the Agents form edits a personal agent's frontmatter only
//!   (description / color / model / thought level, the modelTouched gate);
//!   a built-in (`readOnly`) shows its badge and no controls — the API
//!   would refuse every write on it;
//! - Backups restore is a two-gesture confirm (Restore → "restore this
//!   backup?" → the write), scoped per file.

use std::collections::BTreeMap;

use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, Context, Entity, Rgba, Window, div, px, rgb};
use gpui_kit::component;
use gpui_kit::component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _,
};
use gpui_kit::{assets::IconName as KitIconName, *};
use protocol::config_decode::{
    agent_rows, backup_lists, decode_subagent_model, encode_subagent_model, find_model_option,
    flatten_hooks, hook_edit_patch, hook_timeout_ok, hooks_enabled, is_agent_read_only,
    is_user_tree_scope, mcp_servers, mcp_upsert_body, model_options, skill_rows, timeout_ms_of,
    AgentRow, HookRow, McpFormFields, McpServerRow, ModelOption, SkillRow,
};
use protocol::settings::{AgentUpsert, HookEntryPatch};
use rust_i18n::t;
use serde_json::Value;

use crate::i18n::{fill, t_count};
use crate::runtime::{ConfigPageKind, ConfigSectionPageView, UiCommand};
use crate::ui_prefs::ui_text;
use crate::workbench::Workbench;

/// The eight colors the server's own agent form offers (agents-config.ts);
/// the swatch fills are the tailwind values the TS picker paints.
pub(crate) const AGENT_COLORS: [&str; 8] = [
    "red", "blue", "green", "yellow", "purple", "orange", "pink", "cyan",
];

fn swatch_fill(name: &str) -> Rgba {
    match name {
        "red" => rgb(0xef4444),
        "blue" => rgb(0x3b82f6),
        "green" => rgb(0x22c55e),
        "yellow" => rgb(0xfacc15),
        "purple" => rgb(0xa855f7),
        "orange" => rgb(0xf97316),
        "pink" => rgb(0xec4899),
        _ => rgb(0x06b6d4),
    }
}

/// The MCP form's transports (TS `TYPES`); an entry carrying a type this
/// build does not know keeps it verbatim and selectable so a save never
/// silently downgrades it.
const MCP_TYPES: [&str; 3] = ["stdio", "http", "sse"];

/// Which of the three config forms is open. One at a time — the open form
/// replaces the page (TS ConfigFormSheet: a full screen of its own, never a
/// half-open form behind the section list).
pub(crate) enum ZConfigForm {
    Mcp(McpFormState),
    Hook(HookFormState),
    Agent(AgentFormState),
}

/// The MCP add/edit form's state (the TS `McpForm` local state), owned by
/// the workbench so it survives re-renders.
pub(crate) struct McpFormState {
    /// The server being edited; `None` = adding (TS `target`).
    target: Option<McpServerRow>,
    /// Add mode's name input; edit mode shows the name read-only (it is the
    /// entry's identity — it travels in the PUT path).
    name: Entity<InputState>,
    /// stdio | http | sse, or an unknown spelling kept verbatim.
    server_type: String,
    command: Entity<InputState>,
    args: Entity<TextareaState>,
    env: Entity<TextareaState>,
    url: Entity<InputState>,
    headers: Entity<TextareaState>,
    enabled: bool,
}

/// The hook-entry edit form's state (the TS `HookForm`).
pub(crate) struct HookFormState {
    target: HookRow,
    command: Entity<InputState>,
    /// Seconds, blank = keep the entry's current timeout.
    timeout: Entity<InputState>,
}

/// The personal-agent form's state (the TS `PersonalAgentForm`).
pub(crate) struct AgentFormState {
    /// The agent being edited; `None` = creating one (the TS `NewAgentForm`
    /// branch). A new agent has no name yet, so the identity field is editable
    /// only in this mode — on an existing agent the name IS the file path.
    target: Option<AgentRow>,
    name: Entity<InputState>,
    description: Entity<TextareaState>,
    /// The system-prompt body (the frontmatter's markdown text below the
    /// fence). Multi-line: this is the one field a user rewrites most.
    system_prompt: Entity<TextareaState>,
    /// One of [`AGENT_COLORS`], or `None` = leave unset.
    color: Option<String>,
    /// The picker's encoded value; `None` = inherit. Seeded from the file
    /// when its model maps onto a picker option.
    model: Option<String>,
    /// Only a user PICK makes the model writeable: seeding must not count,
    /// or an untouched save would rewrite the file's own spelling of the
    /// same model.
    model_touched: bool,
    thought_level: Option<String>,
    /// The file's raw model spelling (the unmappable read-only display).
    stored_raw: Option<String>,
    /// The file's own thought level — kept when an unrelated edit saves.
    stored_level: Option<String>,
    /// Tool restriction: `true` = every tool (the key is absent),
    /// `false` = only the checked ones in `tools`.
    all_tools: bool,
    tools: Vec<String>,
    /// The file's own tool list, so "all tools" can be restored faithfully
    /// and an untouched checkbox row does not rewrite the file.
    stored_tools: Option<Vec<String>>,
    permission_mode: Option<String>,
    max_turns: Entity<InputState>,
    inject_agents_md: bool,
    background: bool,
}

impl ConfigPageKind {
    /// Stable element-id fragment for this page's controls.
    pub(crate) fn key(self) -> &'static str {
        match self {
            ConfigPageKind::Skills => "skills",
            ConfigPageKind::Mcp => "mcp",
            ConfigPageKind::Hooks => "hooks",
            ConfigPageKind::Agents => "agents",
            ConfigPageKind::Backups => "backups",
            ConfigPageKind::AppUpdate => "app-update",
        }
    }
}

/// Element-id-safe slug (ids cannot carry '/', ':', …).
fn slug(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect()
}

/// `KEY=VALUE\n…` from a record (TS `kvToLines` — the textarea prefill).
fn kv_to_lines(map: &BTreeMap<String, String>) -> String {
    map.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The picker option the agent form's file-stored model maps onto, against
/// the CURRENT options list. Free function so the paint branch and
/// `agent_level_choices` share ONE live derivation — never frozen into the
/// form state, so a form opened before the models read settles upgrades
/// itself when the payload lands.
fn agent_stored_match(options: &[ModelOption], form: &AgentFormState) -> Option<ModelOption> {
    decode_subagent_model(form.stored_raw.as_deref())
        .and_then(|s| find_model_option(options, &s.provider_id, &s.model_id).cloned())
}

/// The agent's current permission mode, read from its frontmatter.
fn stored_permission_mode(target: &Option<AgentRow>) -> Option<String> {
    target
        .as_ref()
        .and_then(|t| t.frontmatter.as_ref())
        .and_then(|fm| fm.permission_mode.clone())
        .filter(|v| !v.is_empty())
}

/// The agent's current `maxTurns`, read from its frontmatter.
fn stored_max_turns(target: &Option<AgentRow>) -> Option<u32> {
    target
        .as_ref()
        .and_then(|t| t.frontmatter.as_ref())
        .and_then(|fm| fm.max_turns)
}

/// The agent's current `injectAgentsMd`. Absent means the runtime's default
/// (true), which is what the form shows.
fn stored_inject_agents_md(target: &Option<AgentRow>) -> Option<bool> {
    target
        .as_ref()
        .and_then(|t| t.frontmatter.as_ref())
        .and_then(|fm| fm.inject_agents_md)
}

/// The agent's current `background`. `None` = the key was never set, and the
/// form does not invent one.
fn stored_background(target: &Option<AgentRow>) -> Option<bool> {
    target
        .as_ref()
        .and_then(|t| t.frontmatter.as_ref())
        .and_then(|fm| fm.background)
}

/// The tools a custom agent may be restricted to — the upstream app's list
/// (`SubagentsSection.tsx` `TOOL_OPTIONS`). A tool outside this list stays
/// visible as a checkbox when the file already names it, so a hand-written
/// entry is never dropped by an unrelated save (see `agent_tool_options`).
const AGENT_TOOL_OPTIONS: [&str; 9] = [
    "Read",
    "Grep",
    "Glob",
    "Bash",
    "Edit",
    "Write",
    "WebFetch",
    "WebSearch",
    "TodoWrite",
];

/// The permission modes the runtime accepts (server `AGENT_PERMISSION_MODES`,
/// itself mirroring `subagent-profile.ts`). Kept in step with the bridge: a
/// value outside this list is refused server-side.
const AGENT_PERMISSION_MODES: [&str; 6] = [
    "acceptEdits",
    "auto",
    "bypassPermissions",
    "default",
    "dontAsk",
    "plan",
];

// ---------------------------------------------------------------------------
// The shared page shell
// ---------------------------------------------------------------------------

/// The config-screen notice (TS `notice`): a `notice.*` i18n key resolves
/// through the shared catalog, anything else is plain failure text (t!
/// falls back to its input when the key misses).
fn notice_text(notice: &str) -> String {
    t!(notice).into_owned()
}

/// The unsupported / error card (TS ConfigPageFrame's two states).
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
                .text_size(ui_text(13.))
                .text_color(theme.foreground)
                .child(title.to_string()),
        )
        .child(
            div()
                .pt_2()
                .text_size(ui_text(12.))
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
                .text_size(ui_text(13.))
                .text_color(theme.muted_foreground)
                .child(text.to_string()),
        )
        .into_any_element()
}

/// A labelled form field (TS `ConfigField`): label, control, hint.
fn field_label(label: &str, hint: Option<&str>, theme: &component::Theme) -> gpui::Div {
    let mut field = div().flex().flex_col().gap_1().pt_2();
    field = field.child(
        div()
            .text_size(ui_text(12.))
            .text_color(theme.foreground)
            .child(label.to_string()),
    );
    if let Some(hint) = hint {
        field = field.child(
            div()
                .text_size(ui_text(10.))
                .text_color(theme.muted_foreground)
                .child(hint.to_string()),
        );
    }
    field
}

/// The Backend Restart affordance (ZCodeConfigScreen `RestartNote` — the
/// same shape the Models page paints): shown ONLY when a needs-restart
/// write is armed. Restarting cancels in-flight turns, so it is never
/// automatic; with no session open there is no bridge to name and the
/// affordance says so.
fn restart_card(
    theme: &component::Theme,
    has_instance: bool,
    runtime: &crate::runtime::ProtocolRuntime,
    key: &str,
) -> AnyElement {
    let runtime = runtime.clone();
    div()
        .m_3()
        .p_3()
        .rounded_xl()
        .bg(theme.title_bar)
        .border_1()
        .border_color(theme.border)
        .child(
            div()
                .text_size(ui_text(12.))
                .text_color(theme.foreground)
                .child(t!("zconfig.restartPending").into_owned()),
        )
        .child(
            div()
                .pt_1()
                .text_size(ui_text(12.))
                .text_color(theme.muted_foreground)
                .child(t!("zconfig.restartWarning").into_owned()),
        )
        .child(if has_instance {
            Button::new(SharedString::from(format!("{key}-restart")))
                .secondary()
                .small()
                .w_full()
                .icon(IconName::RefreshCw)
                .label(t!("zconfig.restart").into_owned())
                .on_click(move |_, _, _| runtime.send(UiCommand::RestartBackend))
                .into_any_element()
        } else {
            div()
                .pt_2()
                .text_size(ui_text(12.))
                .text_color(theme.warning_foreground)
                .child(t!("zconfig.restartNoInstance").into_owned())
                .into_any_element()
        })
        .into_any_element()
}

/// One section page's frame (TS `ConfigPageFrame`): back header, refresh,
/// notice banner, the unsupported / error / first-load states, the page
/// body, and the shared restart affordance. The open form replaces the
/// page wholesale (the caller decides).
pub(crate) fn render_config_section_page(wb: &Workbench, cx: &Context<Workbench>) -> AnyElement {
    let theme = cx.theme();
    let page = wb
        .snap
        .config_section_page
        .clone()
        .expect("render_config_section_page with no open section page");
    let key = page.kind.key();
    let runtime = wb.runtime.clone();
    let mut column = div()
        .id(SharedString::from(format!("config-section-{key}")))
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .overflow_x_hidden();

    // Header: back, title, refresh.
    let close_kind = page.kind;
    let refresh_kind = page.kind;
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
                Button::new(SharedString::from(format!("{key}-back")))
                    .ghost()
                    .xsmall()
                    .icon(IconName::ArrowLeft)
                    .tooltip("Back to the workbench")
                    .on_click(move |_, _, _| {
                        runtime.send(UiCommand::CloseConfigPage { page: close_kind })
                    }),
            )
            .child(
                div()
                    .text_size(ui_text(13.))
                    .text_color(theme.foreground)
                    .child(t!(page.kind.title()).into_owned()),
            )
            .child(div().flex_1())
            .child(
                Button::new(SharedString::from(format!("{key}-refresh")))
                    .ghost()
                    .xsmall()
                    .icon(IconName::RefreshCw)
                    .tooltip("Reload the section")
                    .disabled(page.loading)
                    .on_click(move |_, _, _| {
                        runtime2.send(UiCommand::RefreshConfigPage { page: refresh_kind })
                    }),
            ),
    );

    // The notice banner (write outcomes and effect classes land here).
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
                        .text_size(ui_text(12.))
                        .text_color(theme.foreground)
                        .child(notice_text(notice)),
                )
                .child(
                    Button::new(SharedString::from(format!("{key}-notice-dismiss")))
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

    // The three frame states, in the TS order: unsupported → error →
    // first-load spinner; anything else paints the page body.
    let body = if page.supported == Some(false) {
        state_card(
            &t!("zconfig.unsupported"),
            &t!("zconfig.unsupportedHint"),
            theme,
        )
    } else if let Some(error) = &page.error {
        state_card(&t!("zconfig.loadFailed"), error, theme)
    } else if page.loading && page.payload.is_none() {
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .items_center()
            .justify_center()
            .child(Spinner::new())
            .into_any_element()
    } else {
        match page.kind {
            ConfigPageKind::Skills => render_skills_body(wb, &page, cx),
            ConfigPageKind::Mcp => render_mcp_body(wb, &page, cx),
            ConfigPageKind::Hooks => render_hooks_body(wb, &page, cx),
            ConfigPageKind::Agents => render_agents_body(wb, &page, cx),
            ConfigPageKind::Backups => render_backups_body(wb, &page, cx),
            ConfigPageKind::AppUpdate => render_app_update_body(wb, &page, cx),
        }
    };
    column = column.child(body);

    // The shared Backend Restart affordance (a needs-restart write armed it).
    if wb.snap.pending_restart {
        column = column.child(restart_card(
            theme,
            wb.snap.active_tab.is_some(),
            &wb.runtime,
            key,
        ));
    }
    column.into_any_element()
}

/// The scrollable list container every section body uses (Stateful for
/// `.overflow_y_scroll()`).
fn section_list(id: &str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(id))
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .pb_4()
}

/// The bordered card a section's rows sit in (TS `ConfigBlock`).
fn rows_card(theme: &component::Theme) -> gpui::Div {
    div()
        .m_3()
        .rounded_xl()
        .bg(theme.title_bar)
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
}

// ---------------------------------------------------------------------------
// I1 — Skills
// ---------------------------------------------------------------------------

fn render_skills_body(wb: &Workbench, page: &ConfigSectionPageView, cx: &Context<Workbench>) -> AnyElement {
    let theme = cx.theme();
    let rows = skill_rows(page.payload.as_ref());
    let mut list = section_list("skills-list");

    if rows.is_empty() {
        list = list.child(empty_card(&t!("zconfig.skillsEmpty"), theme));
        return list.into_any_element();
    }

    let mut block = div().flex().flex_col();
    for (index, skill) in rows.iter().enumerate() {
        // The armed flag is resolved HERE, from the `&Workbench` the body
        // already holds: the delete control must never lease the entity being
        // rendered (`cx.entity().read_with(cx, ..)` double-leases and panics).
        let armed = wb.skill_delete_confirm.as_deref() == Some(skill.id());
        block = block.child(render_skill_row(
            index,
            skill,
            page.busy,
            armed,
            &wb.runtime,
            cx,
        ));
    }
    list.child(rows_card(theme).child(block))
        .into_any_element()
}

fn render_skill_row(
    index: usize,
    skill: &SkillRow,
    busy: bool,
    armed: bool,
    runtime: &crate::runtime::ProtocolRuntime,
    cx: &Context<Workbench>,
) -> AnyElement {
    let theme = cx.theme();
    let id = skill.id().to_string();
    // Copy only makes sense for a skill NOT already in the user tree — and
    // for a project skill it is the one way to get an editable copy.
    let copyable = !is_user_tree_scope(skill.scope.as_deref());
    let name = skill.name.clone().unwrap_or_else(|| id.clone());
    let scope_note = skill
        .scope
        .as_deref()
        .map(|s| {
            let key = format!("zconfig.scope{s}");
            format!("{} · ", t!(key.as_str()))
        })
        .unwrap_or_default();

    let runtime1 = runtime.clone();
    let switch_path = id.clone();
    let enable = !skill.is_enabled();
    let mut row = div()
        .flex()
        .items_start()
        .gap_3()
        .px_4()
        .py_3()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .text_size(ui_text(13.))
                        .text_color(theme.foreground)
                        .child(name),
                )
                .when(skill.description.is_some(), |this| {
                    this.child(
                        div()
                            .text_size(ui_text(10.))
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(skill.description.clone().unwrap_or_default()),
                    )
                })
                .child(
                    div()
                        .font_family(theme.mono_font_family.clone())
                        .text_size(ui_text(10.))
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(format!("{scope_note}{id}")),
                ),
        )
        // Immediate effect, so a switch is the right control — no confirm,
        // no restart.
        .child(
            Switch::new(SharedString::from(format!("skill-switch-{index}")))
                .checked(skill.is_enabled())
                .disabled(busy)
                .on_click(move |_, _, _| {
                    runtime1.send(UiCommand::SetSkillEnabled {
                        path: switch_path.clone(),
                        enable,
                    });
                }),
        );

    if copyable {
        let runtime2 = runtime.clone();
        let copy_path = id.clone();
        row = row.child(
            Button::new(SharedString::from(format!("skill-copy-{index}")))
                .secondary()
                .outline()
                .xsmall()
                .flex_none()
                .icon(IconName::Copy)
                .label(t!("zconfig.skillCopy").into_owned())
                .disabled(busy)
                .on_click(move |_, _, _| {
                    runtime2.send(UiCommand::CopySkillToUser {
                        path: copy_path.clone(),
                    });
                }),
        );
    }
    // Only the server's deletable rows (user/agents trees) get the control —
    // the delete route refuses plugin and project skills anyway.
    if skill.is_deletable() {
        row = row.child(render_skill_delete(index, &id, busy, armed, runtime, cx));
    }
    row.into_any_element()
}

/// A deletable skill's delete control, two gestures like an agent delete: the
/// first press arms it (the row swaps to Cancel / Delete), the second removes
/// the skill directory — there is no undo from the app.
fn render_skill_delete(
    index: usize,
    path: &str,
    busy: bool,
    armed: bool,
    runtime: &crate::runtime::ProtocolRuntime,
    cx: &Context<Workbench>,
) -> AnyElement {
    let theme = cx.theme();
    let wb = cx.entity();

    if armed {
        let wb_cancel = wb.clone();
        let wb_confirm = wb.clone();
        let runtime_fire = runtime.clone();
        let fire_path = path.to_string();
        return div()
            .flex()
            .items_center()
            .gap_1()
            .flex_none()
            .child(
                Button::new(SharedString::from(format!("skill-delete-cancel-{index}")))
                    .ghost()
                    .xsmall()
                    .label(t!("common.cancel").into_owned())
                    .on_click(move |_, _, cx| {
                        wb_cancel.update(cx, |this, cx| {
                            this.skill_delete_confirm = None;
                            cx.notify();
                        });
                    }),
            )
            .child(
                Button::new(SharedString::from(format!("skill-delete-confirm-{index}")))
                    .primary()
                    .xsmall()
                    .label(t!("zconfig.skillDelete").into_owned())
                    .disabled(busy)
                    .on_click(move |_, _, cx| {
                        wb_confirm.update(cx, |this, cx| {
                            this.skill_delete_confirm = None;
                            cx.notify();
                        });
                        runtime_fire.send(UiCommand::DeleteSkill {
                            path: fire_path.clone(),
                        });
                    }),
            )
            .into_any_element();
    }

    let wb_arm = wb;
    let arm_path = path.to_string();
    Button::new(SharedString::from(format!("skill-delete-{index}")))
        .ghost()
        .xsmall()
        .icon(KitIconName::Trash)
        .text_color(theme.muted_foreground)
        .disabled(busy)
        .on_click(move |_, _, cx| {
            let path = arm_path.clone();
            wb_arm.update(cx, |this, cx| {
                this.skill_delete_confirm = Some(path);
                cx.notify();
            });
        })
        .into_any_element()
}

// ---------------------------------------------------------------------------
// I2 — MCP
// ---------------------------------------------------------------------------

fn render_mcp_body(wb: &Workbench, page: &ConfigSectionPageView, cx: &Context<Workbench>) -> AnyElement {
    let theme = cx.theme();
    let rows = mcp_servers(page.payload.as_ref());
    let mut list = section_list("mcp-list");

    if rows.is_empty() {
        list = list.child(empty_card(&t!("zconfig.mcpEmpty"), theme));
    } else {
        let mut block = div().flex().flex_col();
        for (index, server) in rows.iter().enumerate() {
            // Resolved from `wb` (already borrowed) — never via
            // `cx.entity().read_with(cx, ..)`, which double-leases the entity
            // mid-render and panics (the 14:51 SIGABRT, 2026-10-21).
            let armed = wb.mcp_delete_confirm.as_deref() == server.name.as_deref();
            block = block.child(render_mcp_row(
                index,
                server,
                page.busy,
                armed,
                wb,
                cx,
            ));
        }
        list = list.child(rows_card(theme).child(block));
    }

    // The add control (TS mcpAdd).
    let wb_handle = cx.entity();
    list = list.child(
        div().px_3().child(
            Button::new("mcp-add")
                .secondary()
                .small()
                .w_full()
                .icon(IconName::Plus)
                .label(t!("zconfig.mcpAdd").into_owned())
                .on_click(move |_, window, cx| {
                    wb_handle.update(cx, |this, cx| this.open_mcp_form(None, window, cx));
                }),
        ),
    );
    list.into_any_element()
}

fn render_mcp_row(
    index: usize,
    server: &McpServerRow,
    busy: bool,
    armed: bool,
    wb_ref: &Workbench,
    cx: &Context<Workbench>,
) -> AnyElement {
    let theme = cx.theme();
    let name = server.name.clone().unwrap_or_default();
    let subtitle = if let Some(command) = server.command.as_deref() {
        let mut parts = vec![command.to_string()];
        parts.extend(server.args.iter().flatten().cloned());
        parts.join(" ")
    } else {
        server
            .url
            .clone()
            .or_else(|| server.server_type.clone())
            .unwrap_or_default()
    };
    let disabled = server.enabled == Some(false);
    let wb = cx.entity();
    let target = server.clone();
    let runtime = wb_ref.runtime.clone();
    let runtime_switch = runtime.clone();
    let switch_name = name.clone();
    let enable = !disabled;
    let delete = render_mcp_delete(index, &name, busy, armed, &runtime, cx);
    div()
        .id(SharedString::from(format!("mcp-row-{index}-{name}")))
        .test_support()
        .flex()
        .items_center()
        .gap_3()
        .px_4()
        .py_3()
        .cursor_pointer()
        .hover(|this| this.bg(theme.secondary.opacity(0.6)))
        .on_click(move |_, window, cx| {
            wb.update(cx, |this, cx| this.open_mcp_form(Some(target.clone()), window, cx));
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(ui_text(13.))
                        .text_color(theme.foreground)
                        .child(name),
                )
                .child(
                    div()
                        .font_family(theme.mono_font_family.clone())
                        .text_size(ui_text(10.))
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(subtitle),
                )
                .when(disabled, |this| {
                    this.child(
                        div()
                            .text_size(ui_text(10.))
                            .text_color(theme.muted_foreground)
                            .child(t!("zconfig.disabled").into_owned()),
                    )
                }),
        )
        // The row switch (POST /settings/mcp/enable) — immediate effect, so
        // no confirm; stop_propagation keeps it from opening the form.
        .child(
            Switch::new(SharedString::from(format!("mcp-switch-{index}")))
                .checked(!disabled)
                .disabled(busy)
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    runtime_switch.send(UiCommand::SetMcpEnabled {
                        name: switch_name.clone(),
                        enable,
                    });
                }),
        )
        .child(delete)
        .child(
            div()
                .size_3()
                .flex_none()
                .text_color(theme.muted_foreground)
                .child(Icon::new(IconName::ChevronRight)),
        )
        .into_any_element()
}

/// An MCP server's delete control, two gestures like an agent delete: the
/// first press arms it (Cancel / Delete swap in), the second removes the
/// config entry — no undo from the app.
fn render_mcp_delete(
    index: usize,
    name: &str,
    busy: bool,
    armed: bool,
    runtime: &crate::runtime::ProtocolRuntime,
    cx: &Context<Workbench>,
) -> AnyElement {
    let theme = cx.theme();
    let wb = cx.entity();

    if armed {
        let wb_cancel = wb.clone();
        let wb_confirm = wb.clone();
        let runtime_fire = runtime.clone();
        let fire_name = name.to_string();
        return div()
            .flex()
            .items_center()
            .gap_1()
            .flex_none()
            .child(
                Button::new(SharedString::from(format!("mcp-delete-cancel-{index}")))
                    .ghost()
                    .xsmall()
                    .label(t!("common.cancel").into_owned())
                    .on_click(move |_, _, cx| {
                        wb_cancel.update(cx, |this, cx| {
                            this.mcp_delete_confirm = None;
                            cx.notify();
                        });
                    }),
            )
            .child(
                Button::new(SharedString::from(format!("mcp-delete-confirm-{index}")))
                    .primary()
                    .xsmall()
                    .label(t!("zconfig.mcpDelete").into_owned())
                    .disabled(busy)
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        wb_confirm.update(cx, |this, cx| {
                            this.mcp_delete_confirm = None;
                            cx.notify();
                        });
                        runtime_fire.send(UiCommand::DeleteMcpServer {
                            name: fire_name.clone(),
                        });
                    }),
            )
            .into_any_element();
    }

    let wb_arm = wb;
    let arm_name = name.to_string();
    Button::new(SharedString::from(format!("mcp-delete-{index}")))
        .ghost()
        .xsmall()
        .icon(KitIconName::Trash)
        .text_color(theme.muted_foreground)
        .disabled(busy)
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            let name = arm_name.clone();
            wb_arm.update(cx, |this, cx| {
                this.mcp_delete_confirm = Some(name);
                cx.notify();
            });
        })
        .into_any_element()
}

// ---------------------------------------------------------------------------
// I3 — Hooks
// ---------------------------------------------------------------------------

fn render_hooks_body(wb: &Workbench, page: &ConfigSectionPageView, cx: &Context<Workbench>) -> AnyElement {
    let theme = cx.theme();
    let events = page
        .payload
        .as_ref()
        .and_then(|p| p.get("hooks"))
        .and_then(|h| h.get("events"))
        .and_then(|e| e.as_object())
        .cloned()
        .unwrap_or_default();
    let rows = flatten_hooks(&events);
    let mut list = section_list("hooks-list");

    if rows.is_empty() {
        list = list.child(empty_card(&t!("zconfig.hooksEmpty"), theme));
        return list.into_any_element();
    }

    // The whole tree's switch. One write, needs-restart — the bridge
    // re-reads hooks only when its backend respawns.
    let enabled = hooks_enabled(page.payload.as_ref());
    let runtime = wb.runtime.clone();
    list = list.child(
        div()
            .flex()
            .items_center()
            .gap_3()
            .mx_3()
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
                    .text_size(ui_text(13.))
                    .text_color(theme.foreground)
                    .child(t!("zconfig.hooksAll").into_owned()),
            )
            .child(
                Switch::new("hooks-all-switch")
                    .checked(enabled)
                    .disabled(page.busy)
                    .on_click(move |_, _, _| {
                        runtime.send(UiCommand::SetHooksEnabled { enabled: !enabled });
                    }),
            ),
    );

    // Group by event for the collapsible list, preserving the server's
    // order (the payload map is insertion-ordered on the wire).
    let mut by_event: Vec<(String, Vec<&HookRow>)> = Vec::new();
    for row in &rows {
        match by_event.iter_mut().find(|(e, _)| e == &row.event) {
            Some((_, group)) => group.push(row),
            None => by_event.push((row.event.clone(), vec![row])),
        }
    }

    for (event, entries) in by_event {
        let is_open = wb.hooks_open_event.as_deref() == Some(event.as_str());
        let event_slug = slug(&event);
        let wb_handle = cx.entity();
        let event_name = event.clone();
        let mut block = rows_card(theme).child(
            div()
                .id(SharedString::from(format!("hooks-event-{event_slug}")))
                .test_support()
                .flex()
                .items_center()
                .gap_2()
                .px_4()
                .py_3()
                .cursor_pointer()
                .hover(|this| this.bg(theme.secondary.opacity(0.6)))
                .on_click(move |_, _, cx| {
                    wb_handle.update(cx, |this, cx| {
                        this.hooks_open_event =
                            if is_open { None } else { Some(event_name.clone()) };
                        cx.notify();
                    });
                })
                .child(
                    div()
                        .size_3()
                        .flex_none()
                        .text_color(theme.muted_foreground)
                        .child(Icon::new(if is_open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .font_family(theme.mono_font_family.clone())
                                .text_size(ui_text(13.))
                                .text_color(theme.foreground)
                                .truncate()
                                .child(event.clone()),
                        )
                        .child(
                            div()
                                .text_size(ui_text(10.))
                                .text_color(theme.muted_foreground)
                                .child(fill(
                                    &t_count("zconfig.hooksEntryCount", entries.len() as u64),
                                    &[("count", &entries.len().to_string())],
                                )),
                        ),
                ),
        );

        if is_open {
            let mut items = div().flex().flex_col().pb_2();
            for h in entries {
                items = items.child(render_hook_entry(h, page.busy, &wb.runtime, cx));
            }
            block = block.child(items);
        }
        list = list.child(block);
    }

    // The read-only footnote (the route offers no add/remove).
    list = list.child(
        div()
            .px_4()
            .pt_1()
            .text_size(ui_text(10.))
            .text_color(theme.muted_foreground)
            .child(fill(
                &t!("zconfig.hooksReadOnly"),
                &[("count", &rows.len().to_string())],
            )),
    );
    list.into_any_element()
}

fn render_hook_entry(
    h: &HookRow,
    busy: bool,
    runtime: &crate::runtime::ProtocolRuntime,
    cx: &Context<Workbench>,
) -> AnyElement {
    let theme = cx.theme();
    let ms = timeout_ms_of(h);
    let key = slug(&h.key);
    let wb = cx.entity();
    let target = h.clone();
    let runtime1 = runtime.clone();
    let row = h.clone();
    let matcher = if h.matcher.is_empty() {
        t!("zconfig.hooksAnyMatcher").into_owned()
    } else {
        h.matcher.clone()
    };
    div()
        .id(SharedString::from(format!("hook-row-{key}")))
        .flex()
        .items_start()
        .gap_2()
        .mx_4()
        .mb(px(6.))
        .p_2()
        .rounded_md()
        .bg(theme.secondary.opacity(0.4))
        .child(
            div()
                .id(SharedString::from(format!("hook-edit-{key}")))
                .test_support()
                .flex_1()
                .min_w_0()
                .cursor_pointer()
                .on_click(move |_, window, cx| {
                    wb.update(cx, |this, cx| this.open_hook_form(target.clone(), window, cx));
                })
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .justify_between()
                        .gap_2()
                        .child(
                            div()
                                .font_family(theme.mono_font_family.clone())
                                .text_size(ui_text(12.))
                                .text_color(theme.foreground)
                                .truncate()
                                .child(matcher),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(ui_text(10.))
                                .text_color(if h.enabled {
                                    theme.muted_foreground
                                } else {
                                    theme.warning_foreground
                                })
                                .child(if h.enabled {
                                    ms.map(|ms| format!("{ms}ms")).unwrap_or_default()
                                } else {
                                    t!("zconfig.disabled").into_owned()
                                }),
                        ),
                )
                .child(
                    div()
                        .pt(px(2.))
                        .font_family(theme.mono_font_family.clone())
                        .text_size(ui_text(10.))
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(h.command.clone()),
                ),
        )
        .child(
            Switch::new(SharedString::from(format!("hook-switch-{key}")))
                .checked(h.enabled)
                .disabled(busy)
                .on_click(move |_, _, _| {
                    runtime1.send(UiCommand::UpdateHookEntry {
                        event: row.event.clone(),
                        matcher_index: row.matcher_index,
                        hook_index: row.hook_index,
                        patch: HookEntryPatch {
                            enabled: Some(!row.enabled),
                            ..Default::default()
                        },
                    });
                }),
        )
        .into_any_element()
}

// ---------------------------------------------------------------------------
// I4 — Agents
// ---------------------------------------------------------------------------

fn render_agents_body(wb: &Workbench, page: &ConfigSectionPageView, cx: &Context<Workbench>) -> AnyElement {
    let theme = cx.theme();
    let rows = agent_rows(page.payload.as_ref());
    // For the rows' model labels only; the models payload may not have
    // landed yet, in which case the label degrades to the bare model id.
    let options = model_options(page.models.as_ref());
    let mut list = section_list("agents-list");

    if rows.is_empty() {
        list = list.child(empty_card(&t!("zconfig.agentsEmpty"), theme));
    } else {
        let mut block = div().flex().flex_col();
        for (index, agent) in rows.iter().enumerate() {
            // The delete key is the row's DISPLAY name (render_agent_row's
            // `name`), so derive it the same way here. Resolved from `wb`
            // (already borrowed) — never via `cx.entity().read_with(cx, ..)`,
            // which double-leases the entity mid-render and panics (the
            // 14:51 SIGABRT, 2026-10-21).
            let row_name = agent.name.clone().unwrap_or_default();
            let armed = wb.agent_delete_confirm.as_deref() == Some(row_name.as_str());
            block = block.child(render_agent_row(
                index,
                agent,
                &options,
                page.busy,
                armed,
                &wb.runtime,
                cx,
            ));
        }
        list = list.child(rows_card(theme).child(block));
    }

    // The create control — the one action the page was missing entirely: the
    // route has always accepted a PUT on a new name, but nothing in the UI
    // could reach it.
    let wb_handle = cx.entity();
    list = list.child(
        div().px_3().child(
            Button::new("agents-add")
                .secondary()
                .small()
                .w_full()
                .icon(IconName::Plus)
                .label(t!("zconfig.agentsAdd").into_owned())
                .on_click(move |_, window, cx| {
                    wb_handle.update(cx, |this, cx| this.open_agent_form(None, window, cx));
                }),
        ),
    );
    list.into_any_element()
}

fn render_agent_row(
    index: usize,
    agent: &AgentRow,
    options: &[protocol::config_decode::ModelOption],
    busy: bool,
    armed: bool,
    runtime: &crate::runtime::ProtocolRuntime,
    cx: &Context<Workbench>,
) -> AnyElement {
    let theme = cx.theme();
    let name = agent.name.clone().unwrap_or_default();
    // `readOnly` is the server's verdict (a built-in has no file to edit);
    // trusting it keeps the controls off agents the API would refuse.
    let read_only = is_agent_read_only(agent.read_only);
    let fm = agent.frontmatter.clone().unwrap_or_default();
    let description = fm.description.clone();
    // `modelSelection` is the built-ins' override; a personal agent's model
    // lives in its own frontmatter. Both render as the picker label —
    // never the raw `custom:…` spelling the file carries.
    let stored = decode_subagent_model(fm.model.as_deref());
    let model = agent
        .model_selection
        .as_ref()
        .and_then(|s| s.model_id.clone())
        .or_else(|| {
            stored.as_ref().and_then(|s| {
                find_model_option(options, &s.provider_id, &s.model_id).map(|o| o.label.clone())
            })
        })
        .or_else(|| fm.model.clone());
    let thought_level = agent
        .model_selection
        .as_ref()
        .and_then(|s| s.thought_level.clone())
        .or_else(|| fm.thought_level.clone());

    let mut row = div()
        .id(SharedString::from(format!("agent-row-{index}-{name}")))
        .test_support()
        .flex()
        .items_start()
        .gap_3()
        .px_4()
        .py_3();

    // A personal agent opens its form; a built-in has nothing to edit.
    if !read_only {
        let wb = cx.entity();
        let target = agent.clone();
        row = row
            .cursor_pointer()
            .hover(|this| this.bg(theme.secondary.opacity(0.6)))
            .on_click(move |_, window, cx| {
                wb.update(cx, |this, cx| {
                    this.open_agent_form(Some(target.clone()), window, cx)
                });
            });
    }

    row = row.child(
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(ui_text(13.))
                            .text_color(theme.foreground)
                            .truncate()
                            .child(name.clone()),
                    )
                    .when(read_only, |this| {
                        // The built-in badge — the server's own verdict.
                        this.child(
                            div()
                                .id(SharedString::from(format!("agent-builtin-{index}")))
                                .test_support()
                                .flex_none()
                                .px(px(6.))
                                .py(px(1.))
                                .rounded_md()
                                .bg(theme.secondary)
                                .text_size(ui_text(9.))
                                .text_color(theme.muted_foreground)
                                .child(t!("zconfig.agentBuiltIn").into_owned()),
                        )
                    }),
            )
            .when(description.is_some(), |this| {
                this.child(
                    div()
                        .text_size(ui_text(10.))
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(description.unwrap_or_default()),
                )
            })
            .when(model.is_some(), |this| {
                let note = match (&model, &thought_level) {
                    (Some(m), Some(l)) => format!("{m} · {l}"),
                    (Some(m), None) => m.clone(),
                    _ => String::new(),
                };
                this.child(
                    div()
                        .text_size(ui_text(10.))
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(note),
                )
            }),
    );

    // The switch only exists where the API accepts it — a built-in cannot
    // be disabled.
    if !read_only {
        let runtime1 = runtime.clone();
        let enable = !agent.is_enabled();
        let switch_name = name.clone();
        row = row.child(
            Switch::new(SharedString::from(format!("agent-switch-{index}")))
                .checked(agent.is_enabled())
                .disabled(busy)
                .on_click(move |_, _, cx| {
                    // The switch sits INSIDE the row's click target — swallow
                    // the bubble or one tap both flips the switch and opens
                    // the form.
                    cx.stop_propagation();
                    runtime1.send(UiCommand::SetAgentEnabled {
                        name: switch_name.clone(),
                        enable,
                    });
                }),
        );
        row = row.child(render_agent_delete(
            index, &name, busy, armed, runtime, cx,
        ));
    } else {
        // A built-in has no file, but its MODEL is overridable — the server
        // keeps that in the agents state file. Without this control the only
        // way to change it was to hand-edit that file.
        row = row.child(render_builtin_model_control(
            index, agent, &name, options, busy, runtime, cx,
        ));
    }
    row.into_any_element()
}

/// A built-in agent's model override picker (server `setBuiltInAgentModel`).
/// "Inherit" is the clear path — it sends provider+model as null, restoring
/// the workspace default. The thought level is part of the same override, so
/// picking a model resets it to that model's first level.
fn render_builtin_model_control(
    index: usize,
    agent: &AgentRow,
    name: &str,
    options: &[protocol::config_decode::ModelOption],
    busy: bool,
    runtime: &crate::runtime::ProtocolRuntime,
    cx: &Context<Workbench>,
) -> AnyElement {
    // The models payload may not have landed; the button then shows the
    // stored override's id without offering a picker to change it.
    let override_pair = agent.model_selection.as_ref().and_then(|s| {
        let provider = s.provider_id.clone()?;
        let model = s.model_id.clone()?;
        Some((provider, model))
    });
    let label = override_pair
        .as_ref()
        .and_then(|(p, m)| find_model_option(options, p, m).map(|o| o.label.clone()))
        .or_else(|| override_pair.as_ref().map(|(_, m)| m.clone()))
        .unwrap_or_else(|| t!("zconfig.agentModelInherit").into_owned());

    if options.is_empty() {
        return div()
            .flex_none()
            .text_size(ui_text(10.))
            .text_color(cx.theme().muted_foreground)
            .child(label)
            .into_any_element();
    }

    let entries: Vec<(Option<(String, String)>, String)> = std::iter::once((
        None,
        t!("zconfig.agentModelInherit").into_owned(),
    ))
    .chain(options.iter().map(|o| {
        (
            Some((o.provider_id.clone(), o.model_id.clone())),
            o.label.clone(),
        )
    }))
    .collect();
    let runtime_pick = runtime.clone();
    let agent_name = name.to_string();
    let current = override_pair.clone();
    // The menu builder is 'static, so it gets owned copies rather than
    // borrows of the caller's slice.
    let level_options: Vec<(String, String, Vec<String>)> = options
        .iter()
        .map(|o| {
            (
                o.provider_id.clone(),
                o.model_id.clone(),
                o.reasoning_levels.clone(),
            )
        })
        .collect();

    Button::new(SharedString::from(format!("agent-builtin-model-{index}")))
        .secondary()
        .outline()
        .xsmall()
        .label(label)
        .disabled(busy)
        .dropdown_menu(move |menu, _, _| {
            let mut m = menu;
            for (value, entry_label) in entries.clone() {
                let active = value == current;
                let runtime_item = runtime_pick.clone();
                let agent_name = agent_name.clone();
                let levels = level_options.clone();
                m = m.item(
                    PopupMenuItem::new(entry_label)
                        .checked(active)
                        .on_click(move |_, _, _| {
                            runtime_item.send(UiCommand::SetBuiltInAgentModel {
                                name: agent_name.clone(),
                                selection: value.as_ref().map(|(provider_id, model_id)| {
                                    let first_level = levels
                                        .iter()
                                        .find(|(p, m, _)| p == provider_id && m == model_id)
                                        .and_then(|(_, _, ls)| ls.first().cloned());
                                    crate::runtime::BuiltInAgentSelection {
                                        provider_id: provider_id.clone(),
                                        model_id: model_id.clone(),
                                        reasoning_level: first_level,
                                    }
                                }),
                            });
                        }),
                );
            }
            m
        })
        .into_any_element()
}

/// A personal agent's delete control, two gestures like a backup restore: the
/// first press arms it (the row swaps to Cancel / Delete), the second fires.
/// Deleting removes the markdown file — the prompt body included — and the app
/// has no undo, so one stray click must not do it.
fn render_agent_delete(
    index: usize,
    name: &str,
    busy: bool,
    armed: bool,
    runtime: &crate::runtime::ProtocolRuntime,
    cx: &Context<Workbench>,
) -> AnyElement {
    let theme = cx.theme();
    let wb = cx.entity();

    if armed {
        let wb_cancel = wb.clone();
        let wb_confirm = wb.clone();
        let runtime_fire = runtime.clone();
        let fire_name = name.to_string();
        return div()
            .flex()
            .items_center()
            .gap_1()
            .flex_none()
            .child(
                Button::new(SharedString::from(format!("agent-delete-cancel-{index}")))
                    .ghost()
                    .xsmall()
                    .label(t!("common.cancel").into_owned())
                    .on_click(move |_, _, cx| {
                        wb_cancel.update(cx, |this, cx| {
                            this.agent_delete_confirm = None;
                            cx.notify();
                        });
                    }),
            )
            .child(
                Button::new(SharedString::from(format!("agent-delete-confirm-{index}")))
                    .primary()
                    .xsmall()
                    .label(t!("zconfig.agentDelete").into_owned())
                    .disabled(busy)
                    .on_click(move |_, _, cx| {
                        wb_confirm.update(cx, |this, cx| {
                            this.agent_delete_confirm = None;
                            cx.notify();
                        });
                        runtime_fire.send(UiCommand::DeleteAgent {
                            name: fire_name.clone(),
                        });
                    }),
            )
            .into_any_element();
    }

    let wb_arm = wb;
    let arm_name = name.to_string();
    Button::new(SharedString::from(format!("agent-delete-{index}")))
        .ghost()
        .xsmall()
        .icon(KitIconName::Trash)
        .text_color(theme.muted_foreground)
        .disabled(busy)
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            let name = arm_name.clone();
            wb_arm.update(cx, |this, cx| {
                this.agent_delete_confirm = Some(name);
                cx.notify();
            });
        })
        .into_any_element()
}

// ---------------------------------------------------------------------------
// I5 — Backups
// ---------------------------------------------------------------------------

fn render_backups_body(wb: &Workbench, page: &ConfigSectionPageView, cx: &Context<Workbench>) -> AnyElement {
    let theme = cx.theme();
    let lists = backup_lists(page.payload.as_ref());
    let groups: [(&str, &str, Vec<&protocol::config_decode::BackupRow>); 2] = [
        (
            "providerConfig",
            &t!("zconfig.backupProvider"),
            lists.provider_config.iter().collect(),
        ),
        (
            "cliConfig",
            &t!("zconfig.backupCli"),
            lists.cli_config.iter().collect(),
        ),
    ];
    let mut list = section_list("backups-list");
    let total: usize = groups.iter().map(|(_, _, g)| g.len()).sum();

    if total == 0 {
        list = list.child(empty_card(&t!("zconfig.backupsEmpty"), theme));
        return list.into_any_element();
    }

    for (file_key, label, items) in groups {
        let mut block = rows_card(theme).child(
            div()
                .px_4()
                .pt_3()
                .pb_1()
                .text_size(ui_text(12.))
                .text_color(theme.foreground)
                .child(label.to_string()),
        );
        if items.is_empty() {
            block = block.child(
                div()
                    .px_4()
                    .pb_3()
                    .text_size(ui_text(10.))
                    .text_color(theme.muted_foreground)
                    .child(t!("zconfig.backupsNoneForFile").into_owned()),
            );
        } else {
            for (index, backup) in items.iter().enumerate() {
                block = block.child(render_backup_row(
                    wb,
                    index,
                    file_key,
                    backup,
                    page.busy,
                    &wb.runtime,
                    cx,
                ));
            }
        }
        list = list.child(block);
    }

    list = list.child(
        div()
            .px_4()
            .pt_1()
            .text_size(ui_text(10.))
            .text_color(theme.muted_foreground)
            .child(t!("zconfig.backupsScope").into_owned()),
    );
    list.into_any_element()
}

fn render_backup_row(
    wb: &Workbench,
    index: usize,
    file_key: &str,
    backup: &protocol::config_decode::BackupRow,
    busy: bool,
    runtime: &crate::runtime::ProtocolRuntime,
    cx: &Context<Workbench>,
) -> AnyElement {
    let theme = cx.theme();
    // The confirm is keyed by the backup's path — one armed at a time.
    let confirming = wb
        .backup_confirm
        .as_ref()
        .is_some_and(|(_, p)| p == &backup.path);
    let mut card = div().flex().flex_col().px_4().py_2();

    // First line: the stamp plus the gesture controls.
    let mut line = div().flex().items_center().gap_3().child(
        div()
            .text_size(ui_text(12.))
            .text_color(theme.foreground)
            .min_w_0()
            .truncate()
            // The server's ISO-8601 stamp, verbatim — a parse failure must
            // not hide the row (TS falls back to the raw string too).
            .child(backup.created_at.clone()),
    );

    if confirming {
        let wb = cx.entity();
        line = line
            .child(div().flex_1())
            .child(
                Button::new(SharedString::from(format!(
                    "backup-cancel-{file_key}-{index}"
                )))
                .ghost()
                .xsmall()
                .label(t!("common.cancel").into_owned())
                .on_click(move |_, _, cx| {
                    wb.update(cx, |this, cx| {
                        this.backup_confirm = None;
                        cx.notify();
                    });
                }),
            )
            .child({
                let wb2 = cx.entity();
                let file = file_key.to_string();
                let path = backup.path.clone();
                let runtime1 = runtime.clone();
                Button::new(SharedString::from(format!(
                    "backup-confirm-{file_key}-{index}"
                )))
                .primary()
                .xsmall()
                .label(t!("zconfig.confirmRestore").into_owned())
                .disabled(busy)
                .on_click(move |_, _, cx| {
                    // The second gesture: clear the confirm and fire the
                    // write (the driver owns the outcome notice).
                    wb2.update(cx, |this, cx| {
                        this.backup_confirm = None;
                        cx.notify();
                    });
                    runtime1.send(UiCommand::RestoreBackup {
                        file: file.clone(),
                        path: path.clone(),
                    });
                })
            });
    } else {
        let wb = cx.entity();
        let file = file_key.to_string();
        let path = backup.path.clone();
        line = line.child(div().flex_1()).child(
            Button::new(SharedString::from(format!(
                "backup-restore-{file_key}-{index}"
            )))
            .secondary()
            .outline()
            .xsmall()
            .flex_none()
            .icon(IconName::Undo)
            .label(t!("zconfig.restore").into_owned())
            .disabled(busy)
            .on_click(move |_, _, cx| {
                // First gesture: arm the confirm — the restore only fires
                // from the confirm control.
                wb.update(cx, |this, cx| {
                    this.backup_confirm = Some((file.clone(), path.clone()));
                    cx.notify();
                });
            }),
        );
    }
    card = card.child(line);
    card.child(
        div()
            .pt(px(2.))
            .w_full()
            .font_family(theme.mono_font_family.clone())
            .text_size(ui_text(10.))
            .text_color(theme.muted_foreground)
            .truncate()
            .child(backup.path.clone()),
    )
    .into_any_element()
}

// ---------------------------------------------------------------------------
// The forms (impl Workbench — the TS ConfigFormSheet surfaces)
// ---------------------------------------------------------------------------

/// Sets one input's value during an open (window-gated, like every
/// `set_value` in this codebase).
pub(crate) fn prefill(
    state: &Entity<InputState>,
    text: String,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) {
    state.update(cx, |s, cx| s.set_value(text, window, cx));
}

/// The textarea twin of [`prefill`].
fn prefill_area(
    state: &Entity<TextareaState>,
    text: String,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) {
    state.update(cx, |s, cx| s.set_value(text, window, cx));
}

impl Workbench {
    // ---- open / close ----

    /// Opens the MCP add (`target == None`) or edit form, prefilled from
    /// the entry (TS `useState(target?.…)`). Every write on this screen is
    /// needs-restart; the driver's write outcome arms the shared affordance.
    pub fn open_mcp_form(
        &mut self,
        target: Option<McpServerRow>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("context7"));
        let command = cx.new(|cx| InputState::new(window, cx).placeholder("npx"));
        let args = cx.new(|cx| {
            TextareaState::new(window, cx).placeholder("-y\n@upstash/context7-mcp")
        });
        let env = cx.new(|cx| TextareaState::new(window, cx).placeholder("API_KEY=…"));
        let url =
            cx.new(|cx| InputState::new(window, cx).placeholder("https://example.com/mcp"));
        let headers = cx.new(|cx| TextareaState::new(window, cx).placeholder("Authorization=…"));

        if let Some(t) = &target {
            if let Some(text) = t.name.clone() {
                prefill(&name, text, window, cx);
            }
            if let Some(text) = t.command.clone() {
                prefill(&command, text, window, cx);
            }
            if let Some(list) = &t.args {
                prefill_area(&args, list.join("\n"), window, cx);
            }
            if let Some(map) = &t.env {
                prefill_area(&env, kv_to_lines(map), window, cx);
            }
            if let Some(text) = t.url.clone() {
                prefill(&url, text, window, cx);
            }
            if let Some(map) = &t.headers {
                prefill_area(&headers, kv_to_lines(map), window, cx);
            }
        }

        // Re-render per keystroke: canSubmit depends on the typed values.
        let mut subs = Vec::new();
        for entity in [name.clone(), command.clone(), url.clone()] {
            subs.push(cx.subscribe(&entity, |_, _, _: &InputEvent, cx| cx.notify()));
        }
        for entity in [args.clone(), env.clone(), headers.clone()] {
            subs.push(cx.subscribe(&entity, |_, _, _: &InputEvent, cx| cx.notify()));
        }

        self.zconfig_form = Some(ZConfigForm::Mcp(McpFormState {
            server_type: target
                .as_ref()
                .and_then(|t| t.server_type.clone())
                .unwrap_or_else(|| "stdio".to_string()),
            enabled: target.as_ref().map(|t| t.enabled != Some(false)).unwrap_or(true),
            target,
            name,
            command,
            args,
            env,
            url,
            headers,
        }));
        self.zconfig_form_subs = subs;
        cx.notify();
    }

    /// Opens the hook-entry edit form (command + timeout, TS `HookForm`).
    pub fn open_hook_form(&mut self, target: HookRow, window: &mut Window, cx: &mut Context<Self>) {
        let command = cx.new(|cx| InputState::new(window, cx).placeholder("echo done"));
        prefill(&command, target.command.clone(), window, cx);
        // Seconds (the desktop form's unit), blank = keep.
        let timeout = cx.new(|cx| InputState::new(window, cx).placeholder("60"));
        if let Some(ms) = timeout_ms_of(&target) {
            // TS `String(ms / 1000)`: keep the fraction (3500ms is "3.5"),
            // whole seconds print bare.
            let secs = ms as f64 / 1000.0;
            let text = if secs.fract() == 0.0 {
                (secs as u64).to_string()
            } else {
                secs.to_string()
            };
            prefill(&timeout, text, window, cx);
        }
        let sub_c = cx.subscribe(&command, |_, _, _: &InputEvent, cx| cx.notify());
        let sub_t = cx.subscribe(&timeout, |_, _, _: &InputEvent, cx| cx.notify());
        self.zconfig_form = Some(ZConfigForm::Hook(HookFormState {
            target,
            command,
            timeout,
        }));
        self.zconfig_form_subs = vec![sub_c, sub_t];
        cx.notify();
    }

    /// Opens the personal-agent form (description / prompt / color / model /
    /// thought level / tools / permission mode). The picker's data is the
    /// section page's models payload; the file's model seeds the picker only
    /// when it maps onto an option — anything else stays visible read-only and
    /// is never written.
    ///
    /// `target = None` is the create form: the name box is editable and empty,
    /// and every field starts at the runtime's default.
    pub fn open_agent_form(
        &mut self,
        target: Option<AgentRow>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let models = self
            .snap
            .config_section_page
            .as_ref()
            .and_then(|page| page.models.clone());
        let options = model_options(models.as_ref());
        let fm = target
            .as_ref()
            .and_then(|t| t.frontmatter.clone())
            .unwrap_or_default();
        let stored = decode_subagent_model(fm.model.as_deref());
        let matched = stored
            .as_ref()
            .and_then(|s| find_model_option(&options, &s.provider_id, &s.model_id));
        // Seed the picker only when the payload has landed AND the file's
        // model maps onto an option — otherwise leave it unseeded; the
        // paint-time derivation (agent_stored_match) upgrades the form once
        // the models read settles instead of freezing a read-only verdict.
        let stored_encoded = stored
            .zip(matched)
            .map(|(s, _)| encode_subagent_model(&s.provider_id, &s.model_id));

        let name = cx.new(|cx| InputState::new(window, cx));
        prefill(
            &name,
            target
                .as_ref()
                .and_then(|t| t.name.clone())
                .unwrap_or_default(),
            window,
            cx,
        );
        let description = cx.new(|cx| TextareaState::new(window, cx));
        prefill_area(
            &description,
            fm.description.clone().unwrap_or_default(),
            window,
            cx,
        );
        let system_prompt = cx.new(|cx| TextareaState::new(window, cx));
        prefill_area(
            &system_prompt,
            target
                .as_ref()
                .and_then(|t| t.system_prompt.clone())
                .unwrap_or_default(),
            window,
            cx,
        );
        let max_turns = cx.new(|cx| InputState::new(window, cx));
        prefill(
            &max_turns,
            fm.max_turns.map(|n| n.to_string()).unwrap_or_default(),
            window,
            cx,
        );
        let color = fm
            .color
            .as_deref()
            .filter(|c| AGENT_COLORS.contains(c))
            .map(str::to_string);
        // A file that names specific tools opens in custom mode with exactly
        // those checked; an absent key means "every tool".
        let stored_tools = fm.tools.clone().filter(|t| !t.is_empty());
        let all_tools = stored_tools.is_none();
        let tools = stored_tools.clone().unwrap_or_default();
        let sub = cx.subscribe(&description, |_, _, _: &InputEvent, cx| cx.notify());
        let sub_prompt = cx.subscribe(&system_prompt, |_, _, _: &InputEvent, cx| cx.notify());
        let sub_name = cx.subscribe(&name, |_, _, _: &InputEvent, cx| cx.notify());
        self.zconfig_form = Some(ZConfigForm::Agent(AgentFormState {
            target,
            name,
            description,
            system_prompt,
            color,
            // Seeded (not "picked") — an untouched save must not rewrite the
            // file's own spelling of the same model.
            model: stored_encoded.clone(),
            model_touched: false,
            thought_level: fm.thought_level.clone().filter(|l| !l.is_empty()),
            stored_raw: fm.model.clone(),
            stored_level: fm.thought_level.clone(),
            all_tools,
            tools,
            stored_tools,
            permission_mode: fm.permission_mode.clone().filter(|v| !v.is_empty()),
            max_turns,
            inject_agents_md: fm.inject_agents_md.unwrap_or(true),
            background: fm.background.unwrap_or(false),
        }));
        self.zconfig_form_subs = vec![sub, sub_prompt, sub_name];
        cx.notify();
    }

    /// Closes the open form (the header back / a successful save).
    pub(crate) fn close_zconfig_form(&mut self, cx: &mut Context<Self>) {
        self.zconfig_form = None;
        self.zconfig_form_subs.clear();
        cx.notify();
    }

    // ---- validation ----

    /// TS `canSubmit` for the MCP form: a name the route accepts (non-empty,
    /// no path separators) and the transport's required field.
    fn mcp_form_can_submit(&self, cx: &Context<Self>) -> bool {
        let Some(ZConfigForm::Mcp(form)) = &self.zconfig_form else {
            return false;
        };
        let name = form.name.read(cx).value().trim().to_string();
        let name_ok = !name.is_empty() && !name.contains('/') && !name.contains('\\');
        let remote = form.server_type != "stdio";
        let type_ok = if remote {
            !form.url.read(cx).value().trim().is_empty()
        } else {
            !form.command.read(cx).value().trim().is_empty()
        };
        name_ok && type_ok
    }

    /// TS `canSubmit` for the hook form: a non-empty command and a blank or
    /// positive-numeric timeout.
    fn hook_form_can_submit(&self, cx: &Context<Self>) -> bool {
        let Some(ZConfigForm::Hook(form)) = &self.zconfig_form else {
            return false;
        };
        !form.command.read(cx).value().trim().is_empty()
            && hook_timeout_ok(&form.timeout.read(cx).value())
    }

    /// TS `canSubmit` for the agent form: a description.
    fn agent_form_can_submit(&self, cx: &Context<Self>) -> bool {
        let Some(ZConfigForm::Agent(form)) = &self.zconfig_form else {
            return false;
        };
        if form.description.read(cx).value().trim().is_empty() {
            return false;
        }
        // Creating needs a name the route accepts (`^[a-zA-Z0-9-]{3,50}$`).
        // Checking here rather than letting the server refuse keeps the save
        // button off for a value the user can still fix, which is a better
        // answer than a toast after a round trip.
        if let Some(_) = &form.target {
            return true;
        }
        let name = form.name.read(cx).value();
        let name = name.trim();
        (3..=50).contains(&name.chars().count())
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
    }

    // ---- submits ----

    /// The MCP form's Save: the body carries only its transport's fields
    /// (config_decode's `mcp_upsert_body`); the write outcome — not the
    /// click — closes the form.
    fn submit_mcp_form(&mut self, cx: &mut Context<Self>) {
        let Some(ZConfigForm::Mcp(form)) = &self.zconfig_form else {
            return;
        };
        let adding = form.target.is_none();
        let name = match &form.target {
            Some(t) => t.name.clone().unwrap_or_default(),
            None => form.name.read(cx).value().trim().to_string(),
        };
        let body = mcp_upsert_body(&McpFormFields {
            server_type: &form.server_type,
            enabled: form.enabled,
            remote: form.server_type != "stdio",
            url: &form.url.read(cx).value(),
            headers_text: &form.headers.read(cx).value(),
            command: &form.command.read(cx).value(),
            args_text: &form.args.read(cx).value(),
            env_text: &form.env.read(cx).value(),
        });
        self.runtime
            .send(UiCommand::UpsertMcpServer { adding, name, body });
        cx.notify();
    }

    /// The hook form's Save: the patch carries the trimmed command and the
    /// timeout only when the field is non-blank (config_decode's
    /// `hook_edit_patch` writes back in the entry's own unit).
    fn submit_hook_form(&mut self, cx: &mut Context<Self>) {
        let Some(ZConfigForm::Hook(form)) = &self.zconfig_form else {
            return;
        };
        let patch = hook_edit_patch(
            &form.target,
            &form.command.read(cx).value(),
            &form.timeout.read(cx).value(),
        );
        self.runtime.send(UiCommand::UpdateHookEntry {
            event: form.target.event.clone(),
            matcher_index: form.target.matcher_index,
            hook_index: form.target.hook_index,
            patch,
        });
        cx.notify();
    }

    /// The agent form's Save — the modelTouched gate is the whole point: an
    /// untouched save sends NEITHER the model nor the thought level, so the
    /// file keeps its own spelling of both; picking "inherit" sends the
    /// null that clears the key; an unmappable stored model is never
    /// written (it was shown read-only).
    fn submit_agent_form(&mut self, cx: &mut Context<Self>) {
        let Some(ZConfigForm::Agent(form)) = &self.zconfig_form else {
            return;
        };
        // Creating reads the name from its input; editing uses the row's own
        // name (it is the file path, and the route addresses it in the URL).
        let name = match &form.target {
            Some(target) => target.name.clone().unwrap_or_default(),
            None => form.name.read(cx).value().trim().to_string(),
        };
        let description = form.description.read(cx).value().trim().to_string();
        let prompt = form.system_prompt.read(cx).value().trim().to_string();
        let mut body = AgentUpsert {
            description: Some(description),
            ..Default::default()
        };
        if let Some(color) = &form.color {
            body.color = Some(color.clone());
        }
        // The prompt body: an edit sends what the box holds (a cleared box
        // means an empty prompt, which is a legitimate state); a create sends
        // it only when non-empty so the server's template seeds it.
        if form.target.is_some() {
            body.system_prompt = Some(Some(prompt.clone()));
        } else if !prompt.is_empty() {
            body.system_prompt = Some(Some(prompt));
        }
        if form.model_touched {
            match &form.model {
                Some(encoded) => body.model = Some(Some(encoded.clone())),
                // Inherit picked on a file that had a model → clear it.
                None => {
                    if form.stored_raw.is_some() {
                        body.model = Some(None);
                    }
                }
            }
            let level_choices = self.agent_level_choices(cx);
            match &form.thought_level {
                Some(level) if level_choices.contains(level) => {
                    body.thought_level = Some(Some(level.clone()))
                }
                // Inherit picked on a file that had a level → clear it.
                _ => {
                    if form.stored_level.is_some() {
                        body.thought_level = Some(None);
                    }
                }
            }
        }
        // The tool list: "all tools" clears the key (the runtime's default),
        // a custom list sends exactly what is checked. Only sent when the form
        // actually diverges from the file — an untouched row must not rewrite
        // a hand-written list.
        let stored = form.stored_tools.clone();
        let current = if form.all_tools { None } else { Some(form.tools.clone()) };
        if stored != current {
            body.tools = Some(current);
        }
        if form.permission_mode != stored_permission_mode(&form.target) {
            body.permission_mode = Some(form.permission_mode.clone());
        }
        let max_turns = form.max_turns.read(cx).value().trim().to_string();
        let stored_turns = stored_max_turns(&form.target);
        if max_turns.is_empty() {
            if stored_turns.is_some() {
                body.max_turns = Some(None);
            }
        } else if let Ok(turns) = max_turns.parse::<u32>() {
            if Some(turns) != stored_turns {
                body.max_turns = Some(Some(turns));
            }
        }
        let stored_inject = stored_inject_agents_md(&form.target);
        if Some(form.inject_agents_md) != stored_inject {
            body.inject_agents_md = Some(Some(form.inject_agents_md));
        }
        let stored_background = stored_background(&form.target);
        if stored_background.is_some() && Some(form.background) != stored_background {
            body.background = Some(Some(form.background));
        }
        self.runtime.send(UiCommand::UpsertAgent { name, body });
        cx.notify();
    }

    /// The thought-level vocabulary follows the model actually selected —
    /// not the one the file had. A stored level the vocabulary omits stays
    /// selectable while the stored model is selected, so the file's value
    /// survives an unrelated edit (TS `levelChoices`).
    fn agent_level_choices(&self, _cx: &Context<Self>) -> Vec<String> {
        let Some(ZConfigForm::Agent(form)) = &self.zconfig_form else {
            return Vec::new();
        };
        let models = self
            .snap
            .config_section_page
            .as_ref()
            .and_then(|page| page.models.clone());
        let options = model_options(models.as_ref());
        let stored_match = agent_stored_match(&options, form);
        let active = form
            .model
            .as_ref()
            .and_then(|encoded| {
                options
                    .iter()
                    .find(|o| &encode_subagent_model(&o.provider_id, &o.model_id) == encoded)
            })
            .or(stored_match.as_ref());
        let mut choices: Vec<String> = active
            .map(|o| o.reasoning_levels.clone())
            .unwrap_or_default();
        // The file's own level rides on the STORED model's vocabulary —
        // selected either by an explicit pick or by nothing being picked at
        // all (the unseeded case while the payload lands).
        if let Some(stored) = stored_match.as_ref() {
            let active_is_stored = active
                .map(|o| o.provider_id == stored.provider_id && o.model_id == stored.model_id)
                .unwrap_or(true);
            if active_is_stored {
                if let Some(level) = form.stored_level.clone() {
                    if !level.is_empty() && !choices.contains(&level) {
                        choices.push(level);
                    }
                }
            }
        }
        choices
    }

    // ---- the form surfaces ----

    /// The open form — replaces the section page while open (TS
    /// ConfigFormSheet: a full screen of its own).
    pub fn render_zconfig_form(&self, cx: &Context<Workbench>) -> AnyElement {
        let theme = cx.theme();
        let busy = self
            .snap
            .config_section_page
            .as_ref()
            .map(|p| p.busy)
            .unwrap_or(false);
        let (title, can_submit, submit): (String, bool, fn(&mut Self, &mut Context<Self>)) =
            match &self.zconfig_form {
                Some(ZConfigForm::Mcp(_)) => (
                    t!("zconfig.mcpEdit").into_owned(),
                    self.mcp_form_can_submit(cx),
                    Self::submit_mcp_form,
                ),
                Some(ZConfigForm::Hook(_)) => (
                    t!("zconfig.hookEdit").into_owned(),
                    self.hook_form_can_submit(cx),
                    Self::submit_hook_form,
                ),
                Some(ZConfigForm::Agent(form)) => {
                    let title = match &form.target {
                        Some(target) => format!(
                            "{} · {}",
                            t!("zconfig.agentEdit"),
                            target.name.clone().unwrap_or_default()
                        ),
                        None => t!("zconfig.agentsAdd").into_owned(),
                    };
                    (title, self.agent_form_can_submit(cx), Self::submit_agent_form)
                }
                None => return div().into_any_element(),
            };

        let mut column = div().flex().flex_col().flex_1().min_h_0().min_w_0();
        let wb = cx.entity();
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
                    Button::new("zconfig-form-back")
                        .ghost()
                        .xsmall()
                        .icon(IconName::ArrowLeft)
                        .tooltip("Back to the section list")
                        .on_click(move |_, _, cx| {
                            wb.update(cx, |this, cx| this.close_zconfig_form(cx));
                        }),
                )
                .child(
                    div()
                        .text_size(ui_text(13.))
                        .text_color(theme.foreground)
                        .truncate()
                        .min_w_0()
                        .child(title),
                ),
        );

        let body = match &self.zconfig_form {
            Some(ZConfigForm::Mcp(form)) => self.render_mcp_form(form, theme, cx),
            Some(ZConfigForm::Hook(form)) => self.render_hook_form(form, theme, cx),
            Some(ZConfigForm::Agent(form)) => self.render_agent_form(form, theme, cx),
            None => div().into_any_element(),
        };

        let wb2 = cx.entity();
        column
            .child(
                div()
                    .id("zconfig-form-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body)
                    .child(
                        div().p_4().max_w(rems(46.)).child(
                            Button::new("zconfig-form-save")
                                .primary()
                                .w_full()
                                .label(if busy {
                                    t!("zconfig.saving").into_owned()
                                } else {
                                    t!("common.save").into_owned()
                                })
                                .disabled(busy || !can_submit)
                                .on_click(move |_, _, cx| {
                                    wb2.update(cx, |this, cx| submit(this, cx));
                                }),
                        ),
                    ),
            )
            .into_any_element()
    }

    /// The MCP form's body: name (add mode only), the transport segment,
    /// the transport's own fields (url+headers remote / command+args+env
    /// stdio — the branches mirror `mcp_upsert_body`: the fields the user
    /// SEES are the fields the write sends), and enabled.
    fn render_mcp_form(
        &self,
        form: &McpFormState,
        theme: &component::Theme,
        cx: &Context<Workbench>,
    ) -> AnyElement {
        let mut body = div().flex().flex_col().gap_2().p_4();

        if let Some(target) = &form.target {
            // Edit mode: the name is the entry's identity (it travels in
            // the PUT path) — shown, never edited.
            body = body.child(
                div()
                    .font_family(theme.mono_font_family.clone())
                    .text_size(ui_text(12.))
                    .text_color(theme.muted_foreground)
                    .child(target.name.clone().unwrap_or_default()),
            );
        } else {
            body = body
                .child(field_label(
                    &t!("zconfig.mcpName"),
                    Some(&t!("zconfig.mcpNameHint")),
                    theme,
                ))
                .child(div().max_w(rems(46.)).child(Input::new(&form.name)));
        }

        // The transport segment. An unknown type stays its own option so a
        // save never silently downgrades a server this build has not heard
        // of (only http/sse fold into the remote branch).
        let mut choices: Vec<String> = MCP_TYPES.iter().map(|s| s.to_string()).collect();
        if !choices.contains(&form.server_type) {
            choices.insert(0, form.server_type.clone());
        }
        body = body
            .child(field_label(&t!("zconfig.mcpType"), None, theme))
            .child(
                ButtonGroup::new("mcp-type").small().flex_wrap().children(
                    choices.iter().map(|ty| {
                        let wb = cx.entity();
                        let picked = &form.server_type == ty;
                        Button::new(SharedString::from(format!("mcp-type-{}", slug(ty))))
                            .label(ty.clone())
                            .selected(picked)
                            .on_click({
                                let ty = ty.clone();
                                move |_, _, cx| {
                                    wb.update(cx, |this, cx| {
                                        if let Some(ZConfigForm::Mcp(form)) =
                                            &mut this.zconfig_form
                                        {
                                            form.server_type = ty.clone();
                                        }
                                        cx.notify();
                                    });
                                }
                            })
                    }),
                ),
            );

        // Remote transports (http/sse) edit url/headers; stdio edits
        // command/args/env.
        if form.server_type != "stdio" {
            body = body
                .child(field_label(&t!("zconfig.mcpUrl"), None, theme))
                .child(div().max_w(rems(46.)).child(Input::new(&form.url)))
                .child(field_label(
                    &t!("zconfig.mcpHeaders"),
                    Some(&t!("zconfig.kvPerLine")),
                    theme,
                ))
                .child(
                    div()
                        .max_w(rems(46.))
                        .child(Textarea::new(&form.headers).h(px(72.))),
                );
        } else {
            body = body
                .child(field_label(&t!("zconfig.mcpCommand"), None, theme))
                .child(div().max_w(rems(46.)).child(Input::new(&form.command)))
                .child(field_label(
                    &t!("zconfig.mcpArgs"),
                    Some(&t!("zconfig.perLine")),
                    theme,
                ))
                .child(
                    div()
                        .max_w(rems(46.))
                        .child(Textarea::new(&form.args).h(px(72.))),
                )
                .child(field_label(
                    &t!("zconfig.mcpEnv"),
                    Some(&t!("zconfig.kvPerLine")),
                    theme,
                ))
                .child(
                    div()
                        .max_w(rems(46.))
                        .child(Textarea::new(&form.env).h(px(72.))),
                );
        }

        let wb3 = cx.entity();
        body.child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .pt_2()
                .child(
                    div()
                        .text_size(ui_text(12.))
                        .text_color(theme.foreground)
                        .child(t!("zconfig.enabled").into_owned()),
                )
                .child(
                    Switch::new("mcp-enabled").checked(form.enabled).on_click(
                        move |checked: &bool, _, cx| {
                            wb3.update(cx, |this, cx| {
                                if let Some(ZConfigForm::Mcp(form)) = &mut this.zconfig_form {
                                    form.enabled = *checked;
                                }
                                cx.notify();
                            });
                        },
                    ),
                ),
        )
        .into_any_element()
    }

    /// The hook form's body: the entry's coordinates, the command, and the
    /// timeout (seconds — the desktop form's unit; blank keeps the entry's).
    fn render_hook_form(
        &self,
        form: &HookFormState,
        theme: &component::Theme,
        _cx: &Context<Workbench>,
    ) -> AnyElement {
        let matcher = if form.target.matcher.is_empty() {
            t!("zconfig.hooksAnyMatcher").into_owned()
        } else {
            form.target.matcher.clone()
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .child(
                div()
                    .font_family(theme.mono_font_family.clone())
                    .text_size(ui_text(12.))
                    .text_color(theme.muted_foreground)
                    .child(format!("{} · {}", form.target.event, matcher)),
            )
            .child(field_label(&t!("zconfig.hookCommand"), None, theme))
            .child(div().max_w(rems(46.)).child(Input::new(&form.command)))
            .child(field_label(
                &t!("zconfig.hookTimeout"),
                Some(&t!("zconfig.hookTimeoutHint")),
                theme,
            ))
            .child(div().max_w(rems(46.)).child(Input::new(&form.timeout)))
            .into_any_element()
    }

    /// The personal-agent form's body: description, the eight-color swatch
    /// row, and the model + thought-level pickers (the levels follow the
    /// model actually selected).
    fn render_agent_form(
        &self,
        form: &AgentFormState,
        theme: &component::Theme,
        cx: &Context<Workbench>,
    ) -> AnyElement {
        let mut body = div().flex().flex_col().gap_2().p_4();

        // The name is the agent's identity AND its file name, so it is only
        // editable while creating one. On an existing agent the row above the
        // form already shows it.
        if form.target.is_none() {
            body = body
                .child(field_label(
                    &t!("zconfig.agentName"),
                    Some(&t!("zconfig.agentNameHint")),
                    theme,
                ))
                .child(div().max_w(rems(46.)).child(Input::new(&form.name)));
        }

        body = body
            .child(field_label(&t!("zconfig.agentDescription"), None, theme))
            .child(
                div()
                    .max_w(rems(46.))
                    .child(Textarea::new(&form.description).h(px(72.))),
            )
            .child(field_label(
                &t!("zconfig.agentSystemPrompt"),
                Some(&t!("zconfig.agentSystemPromptHint")),
                theme,
            ))
            .child(
                div()
                    .max_w(rems(46.))
                    .child(Textarea::new(&form.system_prompt).h(px(140.))),
            );

        // The color swatches (the server's own eight) plus the unset chip.
        body = body.child(field_label(&t!("zconfig.agentColor"), None, theme));
        let mut swatches = div().flex().flex_wrap().items_center().gap_2();
        for color in AGENT_COLORS {
            let wb = cx.entity();
            let picked = form.color.as_deref() == Some(color);
            swatches = swatches.child(
                div()
                    .id(SharedString::from(format!("agent-color-{color}")))
                    .size_6()
                    .flex_none()
                    .rounded_full()
                    .bg(swatch_fill(color))
                    .when(picked, |this| {
                        this.border_2().border_color(theme.foreground)
                    })
                    .cursor_pointer()
                    .on_click(move |_, _, cx| {
                        wb.update(cx, |this, cx| {
                            if let Some(ZConfigForm::Agent(form)) = &mut this.zconfig_form {
                                form.color = Some(color.to_string());
                            }
                            cx.notify();
                        });
                    }),
            );
        }
        let wb_none = cx.entity();
        swatches = swatches.child(
            Button::new("agent-color-none")
                .secondary()
                .outline()
                .xsmall()
                .label(t!("zconfig.agentColorNone").into_owned())
                .selected(form.color.is_none())
                .on_click(move |_, _, cx| {
                    wb_none.update(cx, |this, cx| {
                        if let Some(ZConfigForm::Agent(form)) = &mut this.zconfig_form {
                            form.color = None;
                        }
                        cx.notify();
                    });
                }),
        );
        body = body.child(swatches);

        // The model picker. A model id this build cannot map onto a picker
        // option (hand-written in the file) is shown read-only, and the
        // submit leaves it alone — substituting a different model on the
        // user's behalf is worse than keeping the value they wrote.
        // Known-ness is derived LIVE from the section page's models payload:
        // the form can open before that read settles (a remote hub), and a
        // frozen open-time verdict would keep the picker read-only after
        // the data landed.
        let page_models = self
            .snap
            .config_section_page
            .as_ref()
            .and_then(|page| page.models.clone());
        let models_present = page_models.is_some();
        let options = model_options(page_models.as_ref());
        let stored_match = agent_stored_match(&options, form);
        if form.stored_raw.is_some() && models_present && stored_match.is_none() {
            body = body
                .child(field_label(&t!("zconfig.agentModel"), None, theme))
                .child(
                    div()
                        .font_family(theme.mono_font_family.clone())
                        .text_size(ui_text(12.))
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(form.stored_raw.clone().unwrap_or_default()),
                )
                .child(
                    div()
                        .text_size(ui_text(10.))
                        .text_color(theme.muted_foreground)
                        .child(t!("zconfig.agentModelUnavailable").into_owned()),
                );
        } else if form.stored_raw.is_some() && !models_present {
            // Payload still loading: show the raw spelling read-only, but
            // no "unavailable" verdict — it may well map once the read
            // lands (the branch above re-derives every paint).
            body = body
                .child(field_label(&t!("zconfig.agentModel"), None, theme))
                .child(
                    div()
                        .font_family(theme.mono_font_family.clone())
                        .text_size(ui_text(12.))
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(form.stored_raw.clone().unwrap_or_default()),
                );
        } else {
            let current = form
                .model
                .as_ref()
                .and_then(|encoded| {
                    options
                        .iter()
                        .find(|o| &encode_subagent_model(&o.provider_id, &o.model_id) == encoded)
                })
                // An unseeded picker (opened before the payload landed)
                // still shows the file's model when it maps — the display
                // is live even though the pick is not.
                .or(stored_match.as_ref())
                .map(|o| o.label.clone())
                .unwrap_or_else(|| t!("zconfig.agentModelInherit").into_owned());
            // The builder runs on every open, so hand each run fresh copies.
            let entries: Vec<(Option<String>, String)> = std::iter::once((
                None,
                t!("zconfig.agentModelInherit").into_owned(),
            ))
            .chain(options.iter().map(|o| {
                (
                    Some(encode_subagent_model(&o.provider_id, &o.model_id)),
                    o.label.clone(),
                )
            }))
            .collect();
            let wb = cx.entity();
            // The menu builder must be 'static — it gets an owned copy of
            // the current pick rather than a borrow of the form.
            let form_model = form.model.clone();
            body = body
                .child(field_label(
                    &t!("zconfig.agentModel"),
                    Some(&t!("zconfig.agentInheritHint")),
                    theme,
                ))
                .child(
                    div().max_w(rems(46.)).child(
                        Button::new("agent-model")
                            .secondary()
                            .small()
                            .w_full()
                            .label(current)
                            .dropdown_menu(move |menu, _, _| {
                                let mut m = menu;
                                for (value, label) in entries.clone() {
                                    let wb = wb.clone();
                                    let active = form_model == value;
                                    m = m.item(
                                        PopupMenuItem::new(label).checked(active).on_click(
                                            move |_, _, cx| {
                                                wb.update(cx, |this, cx| {
                                                    if let Some(ZConfigForm::Agent(form)) =
                                                        &mut this.zconfig_form
                                                    {
                                                        // A PICK (even back to
                                                        // inherit) makes the model
                                                        // writeable; the level
                                                        // restarts from the new
                                                        // model's vocabulary.
                                                        form.model = value.clone();
                                                        form.model_touched = true;
                                                        form.thought_level = None;
                                                    }
                                                    cx.notify();
                                                });
                                            },
                                        ),
                                    );
                                }
                                m
                            }),
                    ),
                );

            // The thought-level chips follow the selected model; an empty
            // vocabulary shows the none-configured note.
            let level_choices = self.agent_level_choices(cx);
            body = body.child(field_label(
                &t!("zconfig.agentThoughtLevel"),
                Some(&t!("zconfig.agentInheritHint")),
                theme,
            ));
            if level_choices.is_empty() {
                body = body.child(
                    div()
                        .text_size(ui_text(10.))
                        .text_color(theme.muted_foreground)
                        .child(t!("zconfig.agentThoughtNone").into_owned()),
                );
            } else {
                let mut chips = div().flex().flex_wrap().gap_2();
                for level in level_choices {
                    let on = form.thought_level.as_deref() == Some(level.as_str());
                    let wb = cx.entity();
                    chips = chips.child(
                        Button::new(SharedString::from(format!(
                            "agent-level-{}",
                            slug(&level)
                        )))
                        .xsmall()
                        .label(level.clone())
                        .when(on, |b| b.primary())
                        .when(!on, |b| b.secondary().outline())
                        .on_click(move |_, _, cx| {
                            wb.update(cx, |this, cx| {
                                if let Some(ZConfigForm::Agent(form)) = &mut this.zconfig_form {
                                    // Tapping the active chip = inherit.
                                    form.thought_level =
                                        if on { None } else { Some(level.clone()) };
                                }
                                cx.notify();
                            });
                        }),
                    );
                }
                body = body.child(chips);
            }
        }

        // ---- tools ----
        body = body.child(field_label(
            &t!("zconfig.agentTools"),
            Some(&t!("zconfig.agentToolsHint")),
            theme,
        ));
        let wb_all = cx.entity();
        let wb_custom = cx.entity();
        // Two-way: "All tools" collapses the grid, "Custom" (only shown while
        // all is active) brings the checkboxes back with their last state —
        // a one-way button left custom selection unreachable without
        // reopening the form.
        body = body
            .child(
                div().flex().flex_wrap().gap_2().child(
                    Button::new("agent-tools-all")
                        .secondary()
                        .outline()
                        .xsmall()
                        .label(t!("zconfig.agentToolsAll").into_owned())
                        .selected(form.all_tools)
                        .on_click(move |_, _, cx| {
                            wb_all.update(cx, |this, cx| {
                                if let Some(ZConfigForm::Agent(form)) = &mut this.zconfig_form {
                                    form.all_tools = true;
                                }
                                cx.notify();
                            });
                        }),
                ),
            )
            .when(form.all_tools, |this| {
                this.child(
                    div().flex().flex_wrap().gap_2().child(
                        Button::new("agent-tools-custom")
                            .secondary()
                            .outline()
                            .xsmall()
                            .label(t!("zconfig.agentToolsCustom").into_owned())
                            .on_click(move |_, _, cx| {
                                wb_custom.update(cx, |this, cx| {
                                    if let Some(ZConfigForm::Agent(form)) = &mut this.zconfig_form {
                                        form.all_tools = false;
                                    }
                                    cx.notify();
                                });
                            }),
                    ),
                )
            });
        // The checkboxes only matter in custom mode; showing them greyed under
        // "all tools" would suggest they still exclude something.
        if !form.all_tools {
            // The file's own tools come first so an unknown spelling stays
            // visible and removable — dropping it silently would loosen the
            // agent's restriction without saying so.
            let mut options: Vec<String> = form
                .stored_tools
                .clone()
                .unwrap_or_default()
                .into_iter()
                .filter(|t| !AGENT_TOOL_OPTIONS.contains(&t.as_str()))
                .collect();
            options.extend(AGENT_TOOL_OPTIONS.iter().map(|t| t.to_string()));
            let mut grid = div().flex().flex_wrap().gap_2().max_w(rems(46.));
            for tool in options {
                let on = form.tools.contains(&tool);
                let wb = cx.entity();
                let label = tool.clone();
                grid = grid.child(
                    Button::new(SharedString::from(format!("agent-tool-{}", slug(&tool))))
                        .xsmall()
                        .label(label.clone())
                        .when(on, |b| b.primary())
                        .when(!on, |b| b.secondary().outline())
                        .on_click(move |_, _, cx| {
                            wb.update(cx, |this, cx| {
                                if let Some(ZConfigForm::Agent(form)) = &mut this.zconfig_form {
                                    if on {
                                        form.tools.retain(|t| t != &label);
                                    } else if !form.tools.contains(&label) {
                                        form.tools.push(label.clone());
                                    }
                                }
                                cx.notify();
                            });
                        }),
                );
            }
            body = body.child(grid);
        }

        // ---- permission mode ----
        body = body.child(field_label(&t!("zconfig.agentPermissionMode"), None, theme));
        let wb_perm = cx.entity();
        let perm_label = form
            .permission_mode
            .clone()
            .unwrap_or_else(|| t!("zconfig.agentPermissionInherit").into_owned());
        let perm_current = form.permission_mode.clone();
        body = body.child(
            div().max_w(rems(46.)).child(
                Button::new("agent-permission-mode")
                    .secondary()
                    .small()
                    .w_full()
                    .label(perm_label)
                    .dropdown_menu(move |menu, _, _| {
                        let mut m = menu.item(
                            PopupMenuItem::new(
                                t!("zconfig.agentPermissionInherit").into_owned(),
                            )
                            .checked(perm_current.is_none())
                            .on_click({
                                let wb = wb_perm.clone();
                                move |_, _, cx| {
                                    wb.update(cx, |this, cx| {
                                        if let Some(ZConfigForm::Agent(form)) =
                                            &mut this.zconfig_form
                                        {
                                            form.permission_mode = None;
                                        }
                                        cx.notify();
                                    });
                                }
                            }),
                        );
                        for mode in AGENT_PERMISSION_MODES {
                            let wb = wb_perm.clone();
                            let value = mode.to_string();
                            let active = perm_current.as_deref() == Some(mode);
                            m = m.item(
                                PopupMenuItem::new(value.clone())
                                    .checked(active)
                                    .on_click(move |_, _, cx| {
                                        let value = value.clone();
                                        wb.update(cx, |this, cx| {
                                            if let Some(ZConfigForm::Agent(form)) =
                                                &mut this.zconfig_form
                                            {
                                                form.permission_mode = Some(value.clone());
                                            }
                                            cx.notify();
                                        });
                                    }),
                            );
                        }
                        m
                    }),
            ),
        );

        // ---- max turns ----
        body = body
            .child(field_label(
                &t!("zconfig.agentMaxTurns"),
                Some(&t!("zconfig.agentMaxTurnsHint")),
                theme,
            ))
            .child(div().max_w(rems(46.)).child(Input::new(&form.max_turns)));

        // ---- the two switches ----
        let wb_inject = cx.entity();
        body = body.child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .max_w(rems(46.))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(1.))
                        .child(
                            div()
                                .text_size(ui_text(12.))
                                .text_color(theme.foreground)
                                .child(t!("zconfig.agentInjectAgentsMd").into_owned()),
                        )
                        .child(
                            div()
                                .text_size(ui_text(10.))
                                .text_color(theme.muted_foreground)
                                .child(t!("zconfig.agentInjectAgentsMdHint").into_owned()),
                        ),
                )
                .child(
                    Switch::new("agent-inject-agents-md")
                        .checked(form.inject_agents_md)
                        .on_click(move |checked: &bool, _, cx| {
                            let checked = *checked;
                            wb_inject.update(cx, |this, cx| {
                                if let Some(ZConfigForm::Agent(form)) = &mut this.zconfig_form {
                                    form.inject_agents_md = checked;
                                }
                                cx.notify();
                            });
                        }),
                ),
        );
        // `background` only appears once the file already declares it: it is
        // not a setting the app introduces, and a default `false` written on
        // every save would be a silent behavioural change.
        if stored_background(&form.target).is_some() {
            let wb_bg = cx.entity();
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .max_w(rems(46.))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(px(1.))
                            .child(
                                div()
                                    .text_size(ui_text(12.))
                                    .text_color(theme.foreground)
                                    .child(t!("zconfig.agentBackground").into_owned()),
                            )
                            .child(
                                div()
                                    .text_size(ui_text(10.))
                                    .text_color(theme.muted_foreground)
                                    .child(t!("zconfig.agentBackgroundHint").into_owned()),
                            ),
                    )
                    .child(
                        Switch::new("agent-background")
                            .checked(form.background)
                            .on_click(move |checked: &bool, _, cx| {
                                let checked = *checked;
                                wb_bg.update(cx, |this, cx| {
                                    if let Some(ZConfigForm::Agent(form)) =
                                        &mut this.zconfig_form
                                    {
                                        form.background = checked;
                                    }
                                    cx.notify();
                                });
                            }),
                    ),
            );
        }

        body.into_any_element()
    }
}

// ---------------------------------------------------------------------------
// App Update (ADR-0009) — src/screens/config/AppUpdatePage.tsx, desktop cut
// ---------------------------------------------------------------------------

/// One label/value line of the versions block (TS `ConfigRow`).
fn update_row(label: &str, value: &str, theme: &component::Theme) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .px_4()
        .py(px(8.))
        .child(
            div()
                .flex_1()
                .text_size(ui_text(13.))
                .text_color(theme.muted_foreground)
                .child(label.to_string()),
        )
        .child(
            div()
                .text_size(ui_text(13.))
                .text_color(theme.foreground)
                .child(value.to_string()),
        )
}

/// App Update's effective stage: the poll-adopted install block wins (TS
/// `appUpdateInstall`); a busy install POST with no verdict yet is already
/// downloading; anything else is idle.
fn app_update_stage(page: &ConfigSectionPageView) -> &str {
    if let Some(stage) = page
        .install
        .as_ref()
        .and_then(|i| i.get("stage"))
        .and_then(Value::as_str)
    {
        return stage;
    }
    if page.busy {
        return "downloading";
    }
    "idle"
}

fn render_app_update_body(
    wb: &Workbench,
    page: &ConfigSectionPageView,
    cx: &Context<Workbench>,
) -> AnyElement {
    let theme = cx.theme();
    let runtime = wb.runtime.clone();
    let mut list = section_list("app-update-list");
    let info = page.payload.as_ref().and_then(|p| p.get("appUpdate"));
    let str_of = |key: &str| {
        info.and_then(|i| i.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let current = str_of("currentVersion").unwrap_or_else(|| "—".to_string());
    let latest = str_of("latestVersion");
    let available = info
        .and_then(|i| i.get("updateAvailable"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // Versions + the channel segment: the check names the channel, and the
    // page's Refresh re-checks against the picked one (TS parity — the pick
    // alone does not auto re-check).
    let mut head = div().flex().flex_col();
    head = head.child(update_row(&t!("zconfig.updateInstalled"), &current, theme));
    head = head.child(update_row(
        &t!("zconfig.updateLatest"),
        latest.as_deref().unwrap_or("—"),
        theme,
    ));
    if let Some(platform) = str_of("platform") {
        head = head.child(update_row(&t!("zconfig.updatePlatform"), &platform, theme));
    }
    let stable = page.update_channel != "preview";
    let rt_channel = runtime.clone();
    head = head.child(
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_4()
            .py(px(8.))
            .child(
                div()
                    .flex_1()
                    .text_size(ui_text(13.))
                    .text_color(theme.muted_foreground)
                    .child(t!("zconfig.updateChannel").into_owned()),
            )
            .child(
                ButtonGroup::new("update-channel")
                    .small()
                    .child(
                        Button::new("update-channel-stable")
                            .label(t!("zconfig.updateChannelStable").into_owned())
                            .selected(stable),
                    )
                    .child(
                        Button::new("update-channel-preview")
                            .label(t!("zconfig.updateChannelPreview").into_owned())
                            .selected(!stable),
                    )
                    .on_click(move |clicks, _, _| {
                        if let Some(&ix) = clicks.first() {
                            rt_channel.send(UiCommand::SetUpdateChannel {
                                channel: if ix == 0 { "stable" } else { "preview" }.to_string(),
                            });
                        }
                    }),
            ),
    );
    list = list.child(rows_card(theme).child(head));

    // The verdict card's stage ladder (AppUpdatePage.tsx 134-227). The
    // install artifact is `files[0]` — the check answers at most one.
    let stage = app_update_stage(page);
    let busy = stage == "downloading" || stage == "installing";
    let artifact = info
        .and_then(|i| i.get("files"))
        .and_then(Value::as_array)
        .and_then(|files| files.first());
    let artifact_url = || {
        artifact
            .and_then(|f| f.get("url"))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let body: AnyElement = if !available {
        empty_card(&t!("zconfig.updateNone"), theme)
    } else if busy {
        let pct = page
            .install
            .as_ref()
            .and_then(|i| {
                let total = i.get("totalBytes").and_then(Value::as_f64);
                let got = i.get("receivedBytes").and_then(Value::as_f64)?;
                total.filter(|t| *t > 0.0).map(|t| (got / t * 100.0).min(100.0))
            })
            .map(|p| p as u32);
        rows_card(theme)
            .id("update-busy")
            .test_support()
            .px_4()
            .py_4()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_size(ui_text(13.))
                    .text_color(theme.foreground)
                    .child(if stage == "installing" {
                        t!("zconfig.updateInstalling").into_owned()
                    } else {
                        t!("zconfig.updateDownloading").into_owned()
                    }),
            )
            .when_some(pct, |card, pct| {
                card.child(
                    div()
                        .w_full()
                        .h(px(6.))
                        .overflow_hidden()
                        .rounded_full()
                        .bg(white().opacity(0.08))
                        .child(
                            div()
                                .id("update-progress")
                                .test_support()
                                .h_full()
                                .rounded_full()
                                .w(relative(pct as f32 / 100.0))
                                .bg(white().opacity(0.3)),
                        ),
                )
                .child(
                    div()
                        .text_size(ui_text(12.))
                        .text_color(theme.muted_foreground)
                        .child(format!("{pct}%")),
                )
            })
            .into_any_element()
    } else if stage == "done" {
        rows_card(theme)
            .id("update-done")
            .test_support()
            .px_4()
            .py_4()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_size(ui_text(13.))
                    .text_color(rgb(0x6ee7b7))
                    .child(fill(
                        &t!("zconfig.updateDone"),
                        &[("version", latest.as_deref().unwrap_or(""))],
                    )),
            )
            .child(
                div()
                    .text_size(ui_text(12.))
                    .text_color(theme.muted_foreground)
                    .child(t!("zconfig.updateRestartApp").into_owned()),
            )
            .into_any_element()
    } else if stage == "needs-user-install" {
        rows_card(theme)
            .id("update-needs-user")
            .test_support()
            .px_4()
            .py_4()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_size(ui_text(13.))
                    .text_color(rgb(0xfcd34d))
                    .child(t!("zconfig.updateNeedsUser").into_owned()),
            )
            .when_some(
                page.install
                    .as_ref()
                    .and_then(|i| i.get("artifactPath"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                |card, path| {
                    card.child(
                        div()
                            .w_full()
                            .font_family(theme.mono_font_family.clone())
                            .text_size(ui_text(12.))
                            .text_color(theme.muted_foreground)
                            .child(path),
                    )
                },
            )
            .child(
                div()
                    .text_size(ui_text(12.))
                    .text_color(theme.muted_foreground)
                    .child(t!("zconfig.updateNeedsUserHint").into_owned()),
            )
            .into_any_element()
    } else if stage == "failed" {
        rows_card(theme)
            .id("update-failed")
            .test_support()
            .px_4()
            .py_4()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_size(ui_text(13.))
                    .text_color(rgb(0xfca5a5))
                    .child(t!("zconfig.updateFailed").into_owned()),
            )
            .when_some(
                page.install
                    .as_ref()
                    .and_then(|i| i.get("error"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                |card, error| {
                    card.child(
                        div()
                            .w_full()
                            .font_family(theme.mono_font_family.clone())
                            .text_size(ui_text(12.))
                            .text_color(theme.muted_foreground)
                            .child(error),
                    )
                },
            )
            .into_any_element()
    } else if wb.update_confirm {
        let wb_cancel = cx.entity();
        let wb_fire = wb_cancel.clone();
        let rt_install = runtime.clone();
        let version = latest.clone().unwrap_or_default();
        let url = artifact_url().unwrap_or_default();
        let channel = page.update_channel.clone();
        rows_card(theme)
            .id("update-confirm")
            .test_support()
            .px_4()
            .py_4()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_size(ui_text(12.))
                    .text_color(theme.foreground)
                    .child(fill(
                        &t!("zconfig.updateConfirm"),
                        &[("version", latest.as_deref().unwrap_or(""))],
                    )),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("update-confirm-cancel")
                            .ghost()
                            .xsmall()
                            .flex_1()
                            .label(t!("common.cancel").into_owned())
                            .on_click(move |_, _, cx| {
                                wb_cancel.update(cx, |this, cx| {
                                    this.update_confirm = false;
                                    cx.notify();
                                });
                            }),
                    )
                    .child(
                        Button::new("update-confirm-install")
                            .secondary()
                            .xsmall()
                            .flex_1()
                            .label(t!("zconfig.updateInstall").into_owned())
                            .on_click(move |_, _, cx| {
                                wb_fire.update(cx, |this, cx| {
                                    this.update_confirm = false;
                                    cx.notify();
                                });
                                rt_install.send(UiCommand::InstallAppUpdate {
                                    version: version.clone(),
                                    url: url.clone(),
                                    channel: channel.clone(),
                                });
                            }),
                    ),
            )
            .into_any_element()
    } else {
        // Idle with an update on offer: the install gesture. Arming needs
        // both halves (the version and the artifact URL) — a check that
        // answered "available" without them cannot be installed, and a
        // dead button that explains nothing is worse than a disabled one.
        let wb_arm = cx.entity();
        let armed = latest.is_some() && artifact_url().is_some();
        rows_card(theme)
            .id("update-available")
            .test_support()
            .px_4()
            .py_4()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                Button::new("update-install")
                    .secondary()
                    .xsmall()
                    .w_full()
                    .icon(KitIconName::Download)
                    .label(t!("zconfig.updateInstall").into_owned())
                    .disabled(!armed)
                    .on_click(move |_, _, cx| {
                        wb_arm.update(cx, |this, cx| {
                            this.update_confirm = true;
                            cx.notify();
                        });
                    }),
            )
            .child(
                div()
                    .text_size(ui_text(12.))
                    .text_color(theme.muted_foreground)
                    .child(fill(
                        &t!("zconfig.updateSize"),
                        &[(
                            "size",
                            artifact
                                .and_then(|f| f.get("size"))
                                .and_then(Value::as_f64)
                                .map(|s| format!("{}MB", (s / 1_000_000.0).round() as u64))
                                .as_deref()
                                .unwrap_or("—"),
                        )],
                    )),
            )
            .into_any_element()
    };
    list = list.child(body);

    if let Some(notes) = str_of("releaseNotes").filter(|n| !n.trim().is_empty()) {
        list = list.child(
            rows_card(theme)
                .child(
                    div()
                        .px_4()
                        .pt_3()
                        .pb_1()
                        .text_size(ui_text(12.))
                        .text_color(theme.muted_foreground)
                        .child(t!("zconfig.updateNotes").into_owned()),
                )
                .child(
                    div()
                        .id("update-notes")
                        .test_support()
                        .px_4()
                        .pb_3()
                        .text_size(ui_text(12.))
                        .text_color(theme.muted_foreground)
                        .child(notes),
                ),
        );
    }
    list.into_any_element()
}
