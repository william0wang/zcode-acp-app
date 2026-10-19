//! The workbench window view (requirements §6): connection status bar
//! (bridge version + running-session count on the left, the hub's quota
//! dock line on the right), the left dock (ADR-0009's dock-style
//! window — a rail of panel toggles with the sessions / configuration panels
//! docked beside the chat area, resizable against it), one session's
//! streaming chat over `MessageScroller` + `TextView`, and the composer.
//! The tab strip is the multi-tab seam: it renders the single active session
//! for now; M2's one-connection-per-tab tabs slot into the same place.
//!
//! Basic desktop shortcuts (requirements §4): Enter sends (the composer's
//! `submit_on_enter`), ⌘K focuses the sessions panel's search field, ⌘1..9
//! select the tab strip's nth session tab.
//!
//! Interaction semantics follow `src/chat/ChatView.tsx`:
//! - identity cache — every markdown part owns ONE `TextViewState` entity,
//!   keyed by message id + part index; unchanged parts are never re-parsed,
//!   and the streaming tail updates through `push_str` (append-only deltas)
//!   so the incremental parse and stream fade behave like the TS convert
//!   cache (`convertCache`, ChatView.tsx:72).
//! - auto-scroll — the scroller follows its tail (`FollowMode::Tail`) and
//!   offers the jump-to-latest button: the ChatView stick-to-bottom +
//!   jump-button pair (ChatView.tsx:999,1097).
//! - composer — Enter sends, Shift+Enter breaks a line; a prompt typed while
//!   a turn is running lands in the pending queue (SessionCore semantics)
//!   and flushes itself when the turn settles.
//! - attachments — image picks (file dialog), pastes, and drops ride the
//!   ADR 0007 pipeline (`protocol::image`, the `src/lib/image.ts` port):
//!   downsample/re-encode off the main thread, capped at MAX_IMAGES, then
//!   out with the prompt as ACP image blocks (ChatView.tsx `addFiles`).

use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    relative, AnyElement, App, ClipboardEntry, ClipboardItem, Context, Entity, ExternalPaths,
    ImageFormat, PathPromptOptions, SharedString, Subscription, Window, div, img, px,
};
use gpui_kit::component;
use gpui_kit::component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui_kit::component::input::{
    Input, InputEvent, InputState, Textarea, TextareaState,
};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::text::{TextView, TextViewMotion, TextViewState};
use gpui_kit::component::{
    h_resizable, resizable_panel, ActiveTheme as _, Disableable as _, Icon, IconName,
    ResizableState, Selectable as _, Sizable as _, WindowExt as _,
};
use gpui_kit::TestSupportExt as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{assets::IconName as KitIconName, *};
use rust_i18n::t;
use protocol::chat::{AttachmentDraft, ChatMessage, ChatPart, ChatRole};
use protocol::image::{
    decode_attachment_bytes, prepare_image, sniff_mime, PrepareError, MAX_IMAGES,
};

use crate::approval_card::{render_approval_card, render_toast_strip, ApprovalCard};
use crate::diff_view::{blocks_of, DiffBlock, DiffLineKind};
use crate::file_browser::{
    absolute_dir, is_loopback_hub, render_file_viewer, render_files_panel, FilesPanelCtx,
};
use crate::i18n::{fill, fmt_relative, now_ms, parse_iso_ms, t_count};
use crate::models_page::ModelsForm;
use crate::runtime::{
    FsBrowseView, HistoryRow, HistorySelected, NewSessionView, Phase, ProtocolRuntime,
    RuntimeEvent, SessionRow, TabPhase, TabSummary, TabView, UiCommand, WorkbenchSnapshot,
};
use crate::quota_page::SpendStage;
use crate::server_form::ServerForm;
use crate::system_status::render_system_status_page;
use crate::usage_page::render_usage_page;
use crate::workflows_page::{
    WorkflowAmendForm, WorkflowMetaForm, WorkflowStartForm,
};
use crate::zconfig_pages::ZConfigForm;

/// Stream-reveal timing for streamed markdown chunks (the Claude-style fade
/// TextView ships; numbers from the gpui-component defaults).
const STREAM_FADE_MS: u64 = 280;
const STREAM_FADE_STAGGER_MS: u64 = 10;

// The `Quit` action (⌘Q / the app menu's Quit item) — handled by
// `Workbench::on_quit_action` for the quit guard. `FocusSessionSearch` is ⌘K;
// `SelectTab1..9` are ⌘1..⌘9 (requirements §4's basic desktop shortcuts).
// `StopTurn` is Esc — `session/cancel` on the running turn (ChatView's
// Cancel, at keyboard distance).
gpui_kit::actions!(
    workbench,
    [
        QuitApp,
        FocusSessionSearch,
        StopTurn,
        SelectTab1,
        SelectTab2,
        SelectTab3,
        SelectTab4,
        SelectTab5,
        SelectTab6,
        SelectTab7,
        SelectTab8,
        SelectTab9
    ]
);

/// Which panel the left dock shows (ADR-0009: sessions/settings as dockable
/// panels around the chat area). Files is NOT here — session-scoped surfaces
/// live in the right session panel; the browser opens as a full page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DockPanel {
    Sessions,
    Config,
}

impl DockPanel {
    /// Stable element-id fragment for the rail buttons.
    fn key(self) -> &'static str {
        match self {
            DockPanel::Sessions => "sessions",
            DockPanel::Config => "config",
        }
    }
}

pub struct Workbench {
    pub(crate) runtime: ProtocolRuntime,
    pub(crate) snap: WorkbenchSnapshot,
    /// The active tab's session view (the M1 chat surface; the sidebar, tab
    /// strip, and approval cards render from `snap`).
    tab: Option<TabView>,
    scroller: Entity<MessageScrollerState>,
    /// Projection of `snap.messages` — one row per message, owning the
    /// `TextViewState` entities (the identity cache).
    rows: Vec<RowModel>,
    /// The session id the rows were built for; a switch rebuilds everything.
    rows_session: Option<String>,
    composer: Entity<TextareaState>,
    /// The composer's clear-on-send, applied during render (set_value is
    /// window-gated and the Enter subscription has no window).
    clear_composer: bool,
    /// A queued draft returned for editing (RuntimeEvent::ComposerPrefill —
    /// ChatView `editPending`): applied during render like clear_composer,
    /// then dropped. Text re-enters the composer; drafts re-stage with their
    /// decoded rasters so the strip shows the same thumbnails a paste would.
    composer_prefill: Option<protocol::chat::PromptDraft>,
    /// Staged attachments waiting for the next send (ChatView `images`),
    /// capped at [`MAX_IMAGES`].
    staged: Vec<StagedImage>,
    /// Last attachment notice (limit / too large / unsupported) — the TS
    /// toast, at workbench density.
    notice: Option<String>,
    /// The inline approval card for the active session's pending ask
    /// (CONTEXT.md "Approval Card"), owning its interactive state; rebuilt
    /// when the request changes.
    card: Option<Entity<ApprovalCard>>,
    /// Whether the scroller's last row is the approval card.
    card_row: bool,
    /// Quit-guard dialog state: one confirm at a time, and the flag that
    /// lets the confirmed pass-through close without asking again.
    quit_dialog_open: bool,
    quit_confirmed: bool,
    /// The left dock (view-local layout, never wire state): the docked panel,
    /// its open/collapsed flag, and the splitter's persisted sizes.
    dock_panel: DockPanel,
    dock_open: bool,
    dock_sizes: Entity<ResizableState>,
    /// The right session panel (TS SessionPanel — the chat-side sheet):
    /// view-local like the left dock, with its own splitter.
    session_panel_open: bool,
    session_dock_sizes: Entity<ResizableState>,
    /// The chat font ladder (TS `fontSize`): scales the reading text — the
    /// user bubbles and the agent prose/thought bodies. Chrome stays fixed.
    font_scale: crate::ui_prefs::FontScale,
    /// The Session Files page (view-local layout): opens from the right
    /// session panel's row, replaces the chat surfaces, and closes on back
    /// or Esc. The listing state itself is the driver's (`snap.files`).
    files_page_open: bool,
    /// The session the files page was opened FOR — a session switch under
    /// an open page re-seeds the browser (or closes it with no session),
    /// never leaves it showing the previous session's stale browse.
    files_page_session: Option<String>,
    /// The files page's focus target: the row click that opens the page
    /// unmounts with the dock it lived in, so without an explicit refocus
    /// key events (Esc) would dispatch from a detached node and never
    /// reach the workbench's action handlers.
    files_page_focus: FocusHandle,
    /// The sessions panel's search field — the ⌘K target (TS SessionList's
    /// "Search sessions": title or workspace base name, substring).
    session_search: Entity<InputState>,
    /// The new-session chooser's known-projects filter (TS ProjectCreateDialog):
    /// matches the workspace path case-insensitively (the name is its last
    /// segment, so a name hit is a path hit).
    new_session_filter: Entity<InputState>,
    _ns_filter_sub: Subscription,
    /// The history panel's project-layer filter (TS ProjectHistoryDialog).
    history_filter: Entity<InputState>,
    _history_filter_sub: Subscription,
    _search_sub: Subscription,
    /// The Models config page's add/edit form (TS ModelForm local state):
    /// `None` = closed. Rendered INSTEAD of the page while open (the TS
    /// ConfigFormSheet replaces the section page outright).
    pub(crate) models_form: Option<ModelsForm>,
    /// Keeps the form inputs' change subscriptions alive.
    pub(crate) models_form_subs: Vec<Subscription>,
    /// The open ZCode-config section form (batch I: MCP / hook / agent);
    /// `None` = closed. Rendered instead of the section page while open,
    /// like `models_form`.
    pub(crate) zconfig_form: Option<ZConfigForm>,
    /// Keeps the config form inputs' change subscriptions alive.
    pub(crate) zconfig_form_subs: Vec<Subscription>,
    /// The Workflows page's start sheet (batch J); `None` = closed.
    /// Rendered instead of the page while open, like `zconfig_form`.
    pub(crate) workflow_start_form: Option<WorkflowStartForm>,
    /// The run-settings amendment sheet (batch J); `None` = closed.
    pub(crate) workflow_amend_form: Option<WorkflowAmendForm>,
    /// The meta edit sheet (batch M, TS `MetaSheet`); `None` = closed.
    pub(crate) workflow_meta_form: Option<WorkflowMetaForm>,
    /// Keeps whichever workflow form's input subscriptions alive.
    pub(crate) workflow_form_subs: Vec<Subscription>,
    /// The WorkflowGate sheet (batch J) — view-local overlay.
    pub(crate) workflow_gate_sheet: bool,
    /// The detail page's script fold (batch M, TS `scriptOpen`): view-local,
    /// reset when the open workflow changes.
    pub(crate) workflow_script_open: bool,
    /// The detail page's armed delete confirm (batch M, TS `confirmDelete`).
    pub(crate) workflow_delete_confirm: bool,
    /// The detail page the script fold + delete confirm belong to (scope +
    /// name + instance): a workflow switch or close resets both.
    workflow_detail_key: Option<String>,
    /// The run page's open raw-payload toggles (batch L, TS `EventRow`'s
    /// `open` + `RawData`): the expanded raw keys, scoped to the open run —
    /// a run switch clears them (the TS key={runId} remount).
    pub(crate) workflow_raw_open: std::collections::HashSet<String>,
    /// Phase-card folds on the run page, keyed by slice index (TS
    /// `phaseOpen[i] ?? i === headSlice`: absent = only the head open).
    pub(crate) workflow_phase_open: std::collections::HashMap<usize, bool>,
    /// Expanded step rows on the run page, keyed by node key.
    pub(crate) workflow_step_open: std::collections::HashSet<String>,
    /// The run id the toggles belong to.
    workflow_raw_run: Option<String>,
    /// The Hooks page's expanded event (view-local fold, TS `open`).
    pub(crate) hooks_open_event: Option<String>,
    /// The Backups page's armed restore confirm (TS `confirm`): the
    /// (file, path) the second gesture would restore.
    pub(crate) backup_confirm: Option<(String, String)>,
    /// The servers panel's armed remove (TS two-click delete): the profile
    /// id and when the 3s confirm window opened.
    pub(crate) server_remove_armed: Option<(String, std::time::Instant)>,
    /// The file viewer's markdown source toggle (TS FileViewer: preview by
    /// default, one toggle to source) — keyed by the viewed path so a new
    /// file always starts in preview.
    pub(crate) file_md_source: Option<(String, bool)>,
    /// The App Update page's armed install confirm (TS `confirm` useState):
    /// page-local, dropped when the page closes.
    pub(crate) update_confirm: bool,
    /// The quota page's spend gesture (TS `confirm` useState): which window,
    /// and whether the below-threshold warning has been answered. Lives only
    /// inside the page — a page closed under an open gesture drops it so a
    /// reopen starts clean.
    pub(crate) quota_confirm: Option<SpendStage>,
    /// The message row whose copy button just fired (the TS ActionBar's
    /// copied checkmark, E4): cleared by a short timer.
    copied_row: Option<String>,
    /// The plan/todo strip's manual fold (E3, ChatScreen PlanPanel's
    /// `<details>`): collapsed by default, reset on a session switch like
    /// the TS remount.
    plan_open: bool,
    /// The settled thought cards expanded past the preview cap (batch U),
    /// keyed by (row id, part index). The list's ScrollableMask owns the
    /// wheel while it can move, so a nested inner scroller is unreliable —
    /// a settled card clips at the cap instead and this flag lifts the cap.
    thought_full: std::collections::HashSet<(String, usize)>,
    /// The files panel's dotfile toggle (F1, TS FileBrowser `showHidden`):
    /// view-local like the TS component state — the entries always arrive
    /// whole from the driver, filtered here.
    files_show_hidden: bool,
    /// The rename sheet (G1, TS SessionList's action sheet → rename mode):
    /// the sidebar row it was opened for, `(instance_id, session_id)`.
    /// `None` = closed. View-local — the wire call travels a UiCommand.
    rename_target: Option<(String, String)>,
    /// The sheet's prefill (the TS `draftTitle = title ?? ""`), applied
    /// during render — `set_value` is window-gated and the menu's click
    /// closure has no window.
    rename_prefill: Option<String>,
    /// The rename sheet's single-line draft.
    rename_input: Entity<InputState>,
    _rename_sub: Subscription,
    /// The composer's "/" completion menu (batch K, ChatView.tsx `Composer`):
    /// view-local highlight + Escape memory. The token and matches derive
    /// from the LIVE composer text at paint time; these fields only carry
    /// what keystrokes cannot recompute.
    slash_highlight: usize,
    /// The token the highlight belongs to — a change resets it (the TS
    /// `useEffect([token])`).
    slash_token: Option<String>,
    /// The Escape'd token: the menu stays closed until the query changes.
    slash_dismissed: Option<String>,
    /// A completion committed without a window at hand (the PressEnter
    /// subscription); applied during render like `clear_composer`.
    pending_slash_insert: Option<PendingSlashInsert>,
    /// Keeps the "/" menu's keystroke interceptor alive.
    _slash_key_sub: Subscription,
    /// The open image lightbox (batch K): click a message image (or the file
    /// viewer's preview) to enlarge; Esc / backdrop closes. View-local — the
    /// raster is already in memory.
    lightbox: Option<Lightbox>,
    _composer_sub: Subscription,
}

/// One committed slash completion waiting for a render-pass `set_value`
/// (window-gated; the PressEnter subscription has no window).
struct PendingSlashInsert {
    text: String,
    /// lsp-style (line, character) the caret parks at — right after the
    /// inserted "/name " (the TS rAF caret park).
    position: (u32, u32),
}

/// The enlarged image overlay's payload.
struct Lightbox {
    src: Arc<gpui::Image>,
    /// Where the raster came from (a message echo or a file preview).
    caption: Option<String>,
}

/// The composer's "/" menu key events (ChatView onKeyDownCapture's set).
#[derive(Clone, Copy, PartialEq, Eq)]
enum SlashKey {
    Up,
    Down,
    Commit,
    Dismiss,
}

/// Everything the "/" menu paints or acts on, derived from the LIVE composer
/// text (value + caret) — recomputed on every keystroke and every render, so
/// no cached token can go stale.
struct SlashMenuState {
    /// The token under the caret, "/" included (e.g. "/td", "/$tdd").
    token: String,
    /// The token's byte range in the composer text.
    start: usize,
    end: usize,
    /// Bridge-ordered matching commands.
    matches: Vec<protocol::acp::SlashCommand>,
    /// False when Escape dismissed this exact query.
    open: bool,
    highlight: usize,
}

/// One staged attachment: the wire draft plus the decoded raster gpui
/// renders as the thumbnail (kept so renders never re-decode base64).
#[derive(Clone)]
struct StagedImage {
    draft: AttachmentDraft,
    render: Option<Arc<gpui::Image>>,
}

/// A raw input on its way into the pipeline (ChatView `File`): bytes plus a
/// mime when the source carried one (clipboard format, extension map).
struct StageInput {
    bytes: Vec<u8>,
    mime: String,
}

// ---------------------------------------------------------------------------
// Row projection
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum PartView {
    /// Assistant text or thought, rendered as markdown and owned by its own
    /// state entity; `last_text` drives push_str-vs-set_text. A thought also
    /// carries its ThoughtCard fold state: `None` = auto (open while it is
    /// the turn's live tail, collapsed once the answer starts — the TS
    /// card's streaming rule), `Some(_)` = the user's last manual toggle,
    /// which defers permanently.
    Markdown {
        state: Entity<TextViewState>,
        last_text: String,
        thought: bool,
        fold: Option<bool>,
        /// The thought body's scroll state (persisted per part so the
        /// streaming bottom-pin can read the LAST frame's extent).
        scroll: gpui::ScrollHandle,
    },
    /// User text — plain, never markdown-parsed (pasted code must not be
    /// reinterpreted as formatting).
    Plain { text: String },
    /// Compact tool-call row for workbench density: status glyph, name, and
    /// the head of the detail line. `detail` is the FULL body behind the
    /// expand (E2 — ChatView.tsx `<details>` ToolCard); `fold_kind` marks a
    /// replay harness fold (`context-handoff` / `tool-transcript`), whose
    /// summary row stays clean (label only; a handoff shows the localized
    /// label, not the wire title); `expanded` is the manual fold state,
    /// collapsed by default like the TS `<details>`. `diffs` holds the
    /// precomputed unified rows of the ACP diff blocks (TS ToolCard's
    /// ReactDiffViewer), computed at the snapshot sync point so a frame
    /// never pays for the LCS.
    ToolCall {
        label: SharedString,
        status: String,
        detail_head: String,
        detail: String,
        fold_kind: Option<String>,
        expanded: bool,
        /// The ACP source blocks — the sync point's cheap change key for
        /// recomputing `diffs`.
        diff_src: Vec<protocol::acp::AcpDiffContent>,
        diffs: Vec<DiffBlock>,
    },
    /// Prompt-attachment echo: the raster decoded once from the part's data
    /// URL. SVG (and anything gpui cannot rasterize) stays on the stub.
    Image { src: Arc<gpui::Image> },
    /// Attachment echo placeholder for unrasterizable payloads (SVG).
    ImageStub,
}

#[derive(Clone)]
struct RowModel {
    id: String,
    role: ChatRole,
    collapsed: bool,
    parts: Vec<PartView>,
}

fn stream_motion() -> TextViewMotion {
    TextViewMotion::default()
        .with_stream_fade(Duration::from_millis(STREAM_FADE_MS))
        .with_stream_fade_stagger(Duration::from_millis(STREAM_FADE_STAGGER_MS))
}

fn new_markdown_part(text: &str, thought: bool, cx: &mut Context<Workbench>) -> PartView {
    let state = cx.new(|cx| {
        let mut state = TextViewState::markdown(text, cx);
        state.set_motion(stream_motion());
        state
    });
    PartView::Markdown {
        state,
        last_text: text.to_string(),
        thought,
        fold: None,
        scroll: gpui::ScrollHandle::new(),
    }
}

fn new_part_view(part: &ChatPart, role: ChatRole, cx: &mut Context<Workbench>) -> PartView {
    match part {
        ChatPart::Text { text } => match role {
            ChatRole::User => PartView::Plain { text: text.clone() },
            ChatRole::Assistant => new_markdown_part(text, false, cx),
        },
        ChatPart::Thought { text } => new_markdown_part(text, true, cx),
        ChatPart::Image { image } => match protocol::image::parse_data_url(image)
            .as_ref()
            .and_then(render_image_of)
        {
            Some(src) => PartView::Image { src },
            None => PartView::ImageStub,
        },
        ChatPart::ToolCall(tc) => PartView::ToolCall {
            label: tc.tool_name.clone().into(),
            status: tc.status.clone(),
            detail_head: tool_detail_head(&tc.detail),
            detail: tc.detail.clone(),
            fold_kind: tc.fold_kind.clone(),
            expanded: false,
            diff_src: tc.diffs.clone().unwrap_or_default(),
            diffs: blocks_of(tc.diffs.as_deref().unwrap_or(&[])),
        },
    }
}

/// First non-empty line of a tool detail, capped for density.
fn tool_detail_head(detail: &str) -> String {
    let line = detail.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let mut out: String = line.chars().take(160).collect();
    if line.chars().count() > 160 {
        out.push('…');
    }
    out
}

/// One ACP diff block's unified view — the TS `DiffBlock`
/// (ChatView.tsx:166): path header, the capped-sides notice, then the
/// +/- rows. Word-level diff and line numbers stay TS-side polish
/// (jsdiff/ReactDiffViewer); line coloring is the functional core. Colors
/// are the quiet-dark palette's emerald/red families (the theme's accent
/// pair for running/failed states).
fn render_diff_block(
    block: &DiffBlock,
    row_id: &str,
    part_index: usize,
    block_index: usize,
    theme: &component::Theme,
) -> gpui::Div {
    let mut card = div()
        .min_w_0()
        .my(px(6.))
        .mx_2()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .overflow_hidden();
    if !block.path.is_empty() {
        card = card.child(
            div()
                .id(SharedString::from(format!(
                    "tool-diff-path-{row_id}-{part_index}-{block_index}"
                )))
                .truncate()
                .px_2()
                .py(px(3.))
                .bg(theme.title_bar)
                .border_b_1()
                .border_color(theme.border)
                .text_color(theme.muted_foreground)
                .child(block.path.clone())
                .test_support(),
        );
    }
    if block.truncated {
        card = card.child(
            div()
                .id(SharedString::from(format!(
                    "tool-diff-trunc-{row_id}-{part_index}-{block_index}"
                )))
                .px_2()
                .py(px(3.))
                .bg(rgb(0x2a1d08))
                .text_color(theme.warning_foreground)
                .child(t!("chat.diffTruncated").to_string())
                .test_support(),
        );
    }
    let mut lines = div();
    for l in &block.lines {
        let (glyph, text_color, bg): (&str, Hsla, Option<Hsla>) = match l.kind {
            DiffLineKind::Add => ("+", rgb(0x6ee7b7).into(), Some(rgb(0x0f2a1e).into())),
            DiffLineKind::Del => ("-", rgb(0xfca5a5).into(), Some(rgb(0x33141a).into())),
            DiffLineKind::Context => (" ", theme.muted_foreground, None),
        };
        let mut row = div()
            .min_w_0()
            .flex()
            .px_2()
            .gap_1()
            .text_color(text_color);
        if let Some(bg) = bg {
            row = row.bg(bg);
        }
        lines = lines.child(
            row.child(div().flex_none().child(glyph))
                .child(div().min_w_0().child(l.text.clone())),
        );
    }
    card.child(lines)
}

/// TUI parity (martty stats dock, ChatScreen.tsx `fmtTokens`): `128000` ->
/// `128K`, `1500000` -> `1.5M`.
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

/// Context gauge percent (ChatScreen.tsx `fmtContext`'s head), clamped at
/// 100; a zero window reads as full.
fn context_pct(used: u64, size: u64) -> f64 {
    if size == 0 {
        100.0
    } else {
        ((used as f64 / size as f64) * 100.0).min(100.0)
    }
}

/// Context gauge as the TUI prints it (ChatScreen.tsx `fmtContext`) —
/// `17.5%/1.0M`, a one-decimal percent of the model window, clamped at 100.
fn fmt_context(used: u64, size: u64) -> String {
    format!("{:.1}%/{}", context_pct(used, size), fmt_tokens(size))
}

/// The session row's status dot (the TS ActivityBadge ladder minus the
/// text-only finished chip): awaiting > running > idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActivityDot {
    Awaiting,
    Running,
    Idle,
}

/// SessionList.tsx:29-87: the broadcast activity entry wins; discovery's
/// REST `running` status is the heartbeat fallback (ADR-0005) ONLY for rows
/// that have no activity entry at all — a known-idle entry must not be
/// overridden by a stale REST flag.
pub(crate) fn activity_dot(sess: &SessionRow) -> ActivityDot {
    if sess.awaiting {
        ActivityDot::Awaiting
    } else if sess.running || (sess.rest_running && !sess.activity_known) {
        ActivityDot::Running
    } else {
        ActivityDot::Idle
    }
}

/// The just-finished chip's 60s window (TS `FINISHED_WINDOW_MS`).
pub(crate) fn finished_fresh(sess: &SessionRow, now: u64) -> bool {
    sess.finished_at
        .is_some_and(|at| now < at.saturating_add(60_000))
}

/// One config option's picker data, shared by the composer's chip and the
/// session panel's row: the current value's display name (the raw value
/// when the choices do not list it, `—` when unset) and the menu choices as
/// `(value, name, active)`.
fn config_option_view(opt: &protocol::acp::ConfigOption) -> (String, Vec<(String, String, bool)>) {
    let current = opt
        .current_value
        .as_deref()
        .map(|v| {
            opt.options
                .as_ref()
                .and_then(|choices| choices.iter().find(|c| c.value == v))
                .map(|c| c.name.clone())
                .unwrap_or_else(|| v.to_string())
        })
        .unwrap_or_else(|| "—".to_string());
    let choices = opt
        .options
        .iter()
        .flatten()
        .map(|c| {
            (
                c.value.clone(),
                c.name.clone(),
                Some(c.value.as_str()) == opt.current_value.as_deref(),
            )
        })
        .collect();
    (current, choices)
}

/// The picker menu: every choice with a check on the active row; picking a
/// non-active row dispatches `session/set_config_option`. The builder runs
/// on every open, so it clones per run.
fn config_option_menu(
    menu: PopupMenu,
    runtime: &ProtocolRuntime,
    config_id: &str,
    choices: &[(String, String, bool)],
) -> PopupMenu {
    let mut m = menu;
    for (value, name, active) in choices.iter().cloned() {
        let runtime = runtime.clone();
        let config_id = config_id.to_string();
        // `new` (clickable Item), NOT `label` (a disabled caption).
        m = m.item(
            PopupMenuItem::new(name)
                .checked(active)
                .on_click(move |_, _, _| {
                    if !active {
                        runtime.send(UiCommand::SetConfigOption {
                            config_id: config_id.clone(),
                            value: value.clone(),
                        })
                    }
                }),
        );
    }
    m
}

/// A row's concatenated plain text (user speech) — the task-notification
/// parser's input, like the TS `parts.map(...).join("")`.
fn row_plain_text(row: &RowModel) -> String {
    let mut out = String::new();
    for part in &row.parts {
        if let PartView::Plain { text } = part {
            out.push_str(text);
        }
    }
    out
}

/// The `<status>`/`<summary>` value inside a task-notification block (the TS
/// regex `[^<]*?` — content with a nested tag does not match; empty on
/// miss).
fn tag_value(text: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    text.find(&open)
        .and_then(|start| {
            let inner = &text[start + open.len()..];
            inner.find(&close).map(|end| &inner[..end])
        })
        .filter(|content| !content.contains('<'))
        .map(str::trim)
        .unwrap_or_default()
        .to_string()
}

/// Harness task lifecycle notices arrive as user-role XML blocks; they are
/// system plumbing, not user speech — rendered as a centered system chip
/// (ChatView.tsx `parseTaskNotification`). Only a message that is EXACTLY
/// one notification block qualifies (the `^…$` anchor).
fn parse_task_notification(text: &str) -> Option<(String, String)> {
    const OPEN: &str = "<task-notification>";
    const CLOSE: &str = "</task-notification>";
    let t = text.trim();
    let body = t.strip_prefix(OPEN)?;
    body.strip_suffix(CLOSE)?;
    Some((tag_value(t, "status"), tag_value(t, "summary")))
}

/// The row's trailing copy affordance (E4 — ChatView.tsx:437
/// `ActionBarPrimitive.Copy`): writes the row's source text to the platform
/// clipboard; the icon flips to a checkmark for a beat after the click (the
/// TS `data-copied` swap).
fn copy_button(
    row: &RowModel,
    text: String,
    copied: bool,
    wb: &WeakEntity<Workbench>,
) -> Button {
    let row_id = row.id.clone();
    Button::new(SharedString::from(format!("copy-{}", row.id)))
        .ghost()
        .xsmall()
        .icon(if copied {
            KitIconName::Check
        } else {
            KitIconName::Copy
        })
        .tooltip(t!("chat.copy"))
        .on_click({
            let wb = wb.clone();
            move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                if let Some(wb) = wb.upgrade() {
                    wb.update(cx, |wb, cx| wb.mark_copied(&row_id, cx));
                }
            }
        })
}

/// Updates a part view in place. Returns true when its rendered content
/// changed (drives the scroller remeasure). Part kinds never transition
/// within one slot (replay appends parts; it does not rewrite kinds).
fn sync_part_view(view: &mut PartView, part: &ChatPart, cx: &mut Context<Workbench>) -> bool {
    match (view, part) {
        (
            PartView::Markdown {
                state,
                last_text,
                thought,
                ..
            },
            ChatPart::Text { text } | ChatPart::Thought { text },
        ) => {
            let _ = thought;
            if text == last_text {
                return false;
            }
            state.update(cx, |state, cx| {
                if text.starts_with(last_text.as_str()) && !last_text.is_empty() {
                    // Append-only delta — the streaming path (push_str keeps
                    // the incremental parse and the stream fade).
                    state.push_str(&text[last_text.len()..], cx);
                } else {
                    // Rewrite (replay reconcile, thought revision): parse the
                    // full replacement.
                    state.set_text(text, cx);
                }
            });
            *last_text = text.clone();
            true
        }
        (PartView::Plain { text }, ChatPart::Text { text: new_text }) => {
            if text == new_text {
                return false;
            }
            *text = new_text.clone();
            true
        }
        (
            PartView::ToolCall {
                label,
                status,
                detail_head,
                detail,
                fold_kind,
                diff_src,
                diffs,
                ..
            },
            ChatPart::ToolCall(tc),
        ) => {
            let head = tool_detail_head(&tc.detail);
            let new_diffs = tc.diffs.as_deref().unwrap_or(&[]);
            if *status == tc.status
                && *detail_head == head
                && label.as_ref() == tc.tool_name
                && *detail == tc.detail
                && *fold_kind == tc.fold_kind
                && diff_src.as_slice() == new_diffs
            {
                return false;
            }
            *label = tc.tool_name.clone().into();
            *status = tc.status.clone();
            *detail_head = head;
            *detail = tc.detail.clone();
            *fold_kind = tc.fold_kind.clone();
            // The LCS only re-runs when the source blocks changed (plain
            // Vec compare — no diffing to decide whether to diff).
            *diff_src = new_diffs.to_vec();
            *diffs = blocks_of(new_diffs);
            true
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Workbench
// ---------------------------------------------------------------------------

/// Serializes the window-opening UI tests process-wide: the kit's headless
/// platform keeps process-global mouse state (and some tests also flip
/// process-global env/locale), so parallel test windows can steal each
/// other's pointer mid-click. Shared with zconfig_pages' `window_lock`.
#[cfg(test)]
pub(crate) fn window_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Workbench {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        runtime: ProtocolRuntime,
        initial: WorkbenchSnapshot,
    ) -> Self {
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Message the agent — Enter to send, Shift+Enter for a newline")
                .submit_on_enter(true)
        });
        let composer_sub = cx.subscribe(&composer, |this, _emitter, event, cx| {
            if let InputEvent::PressEnter {
                secondary: false,
                shift: false,
            } = event
            {
                // An open "/" menu swallows Enter as a commit (the TS capture
                // handler beats the composer submit); only a closed menu sends.
                if let Some(name) = this
                    .slash_menu(cx)
                    .filter(|m| m.open)
                    .and_then(|m| m.matches.get(m.highlight).map(|c| c.name.clone()))
                {
                    this.stage_slash_select(&name, cx);
                    return;
                }
                this.submit_composer(cx);
            }
        });
        // The "/" menu's keyboard half (ChatView onKeyDownCapture): arrows
        // navigate, Tab/Enter commit, Escape dismisses — observed BEFORE the
        // textarea's own bindings, and consumed so the caret never moves
        // behind the menu. App-global: the composer-focus check gates it.
        let slash_weak = cx.entity().downgrade();
        let slash_key_sub = cx.intercept_keystrokes(move |event, window, cx| {
            let Some(wb) = slash_weak.upgrade() else {
                return;
            };
            let focused = wb
                .read(cx)
                .composer
                .read(cx)
                .focus_handle(cx)
                .is_focused(window);
            if !focused {
                return;
            }
            // Read-side gate first: with no OPEN menu there is nothing to
            // intercept, and the keystroke must reach the textarea's own
            // bindings untouched (no entity writes during its dispatch).
            let menu_open = wb.read(cx).slash_menu(cx).is_some_and(|m| m.open);
            if !menu_open {
                return;
            }
            let m = &event.keystroke.modifiers;
            // Platform combos (cmd/ctrl/alt) stay the platform's — only the
            // bare keys the TS handler intercepts (plus Shift+Tab, which TS
            // also commits) drive the menu.
            let platform_mod = m.control || m.alt || m.platform || m.function;
            let key = match event.keystroke.key.as_str() {
                "up" if !platform_mod => SlashKey::Up,
                "down" if !platform_mod => SlashKey::Down,
                "tab" if !platform_mod => SlashKey::Commit,
                "enter" if !platform_mod && !m.shift => SlashKey::Commit,
                "escape" if !platform_mod => SlashKey::Dismiss,
                _ => return,
            };
            let handled = wb.update(cx, |this, cx| this.on_slash_key(key, cx));
            if handled {
                cx.stop_propagation();
            }
        });
        let scroller = cx.new(|cx| MessageScrollerState::new(0, cx));
        // The sessions panel's search field (⌘K focuses it).
        let session_search =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search sessions"));
        // Re-render per keystroke: the sidebar's filter reads the value.
        let search_sub = cx.subscribe(&session_search, |_, _, _: &InputEvent, cx| cx.notify());
        let new_session_filter =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter projects"));
        let ns_filter_sub =
            cx.subscribe(&new_session_filter, |_, _, _: &InputEvent, cx| cx.notify());
        // The history panel's project-layer filter (TS ProjectHistoryDialog
        // :413-421) — the same shape as the chooser's.
        let history_filter =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter projects"));
        let history_filter_sub =
            cx.subscribe(&history_filter, |_, _, _: &InputEvent, cx| cx.notify());
        // The rename sheet's draft (G1) — Enter saves when non-empty (the TS
        // sheet's disabled-Save guard), every keystroke re-renders the button.
        let rename_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Session title"));
        let rename_sub = cx.subscribe(&rename_input, |this, _, event: &InputEvent, cx| {
            match event {
                InputEvent::PressEnter {
                    secondary: false,
                    shift: false,
                } => {
                    let can_save = !this.rename_input.read(cx).value().trim().is_empty();
                    if can_save && this.rename_target.is_some() {
                        this.submit_rename(cx);
                    }
                }
                _ => cx.notify(),
            }
        });
        let dock_sizes = cx.new(|_| ResizableState::default());
        let session_dock_sizes = cx.new(|_| ResizableState::default());
        // The composer is the primary surface — start focused.
        let focus = composer.read(cx).focus_handle(cx).clone();
        window.focus(&focus, cx);

        let mut this = Self {
            runtime,
            tab: initial.active.clone(),
            snap: initial,
            scroller,
            rows: Vec::new(),
            rows_session: None,
            composer,
            clear_composer: false,
            composer_prefill: None,
            staged: Vec::new(),
            notice: None,
            card: None,
            card_row: false,
            quit_dialog_open: false,
            quit_confirmed: false,
            dock_panel: DockPanel::Sessions,
            dock_open: true,
            dock_sizes,
            session_panel_open: false,
            files_page_open: false,
            files_page_session: None,
            files_page_focus: cx.focus_handle(),
            font_scale: crate::ui_prefs::load_font_size()
                .unwrap_or(crate::ui_prefs::FontScale::Medium),
            session_dock_sizes,
            session_search,
            _search_sub: search_sub,
            new_session_filter,
            _ns_filter_sub: ns_filter_sub,
            history_filter,
            _history_filter_sub: history_filter_sub,
            models_form: None,
            models_form_subs: Vec::new(),
            zconfig_form: None,
            zconfig_form_subs: Vec::new(),
            workflow_start_form: None,
            workflow_amend_form: None,
            workflow_meta_form: None,
            workflow_form_subs: Vec::new(),
            workflow_gate_sheet: false,
            workflow_script_open: false,
            workflow_delete_confirm: false,
            workflow_detail_key: None,
            workflow_raw_open: std::collections::HashSet::new(),
            workflow_phase_open: std::collections::HashMap::new(),
            workflow_step_open: std::collections::HashSet::new(),
            workflow_raw_run: None,
            hooks_open_event: None,
            backup_confirm: None,
            server_remove_armed: None,
            file_md_source: None,
            update_confirm: false,
            quota_confirm: None,
            copied_row: None,
            plan_open: false,
            thought_full: std::collections::HashSet::new(),
            files_show_hidden: false,
            rename_target: None,
            rename_prefill: None,
            rename_input,
            _rename_sub: rename_sub,
            slash_highlight: 0,
            slash_token: None,
            slash_dismissed: None,
            pending_slash_insert: None,
            _slash_key_sub: slash_key_sub,
            lightbox: None,
            _composer_sub: composer_sub,
        };

        // The quit guard's window-close half (⌘Q arrives as the QuitApp
        // action, registered in main.rs): veto the close while sessions run,
        // ask once, then let the confirmed close through.
        let weak = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            weak.update(cx, |wb, cx| wb.request_quit(window, cx))
                .unwrap_or(true)
        });
        this.apply_snapshot_inner(cx);
        this
    }

    // -- quit guard ----------------------------------------------------------

    /// The shared quit path for ⌘Q and window close. Returns whether the
    /// window may close now; with running sessions it shows the native
    /// confirm first (and returns false — the dialog's OK does the close).
    /// Quitting never stops the hub or the sessions (background-task
    /// semantics, requirements §3/§4): the dialog is stray-quit protection,
    /// not an interruption warning.
    pub fn request_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.quit_confirmed {
            return true;
        }
        let running = self.snap.running_session_count;
        if running == 0 {
            return true;
        }
        if self.quit_dialog_open {
            return false;
        }
        self.quit_dialog_open = true;
        let n = running;
        let weak = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let weak = weak.clone();
            alert
                .title("Quit with sessions running?")
                .description(format!(
                    "{n} session{s} still running on the hub. Quitting does NOT stop them — \
                     they keep running in the background and every chat stays reachable. \
                     This is only a stray-quit check.",
                    s = if n == 1 { " is" } else { "s are" },
                ))
                .confirm()
                .ok_text("Quit Anyway")
                .cancel_text("Keep Window")
                .on_ok(move |_, window, cx| {
                    let _ = weak.update(cx, |wb, _| {
                        wb.quit_confirmed = true;
                        wb.quit_dialog_open = false;
                    });
                    // The confirmed close: the last window closing quits the
                    // app (QuitMode::LastWindowClosed, main.rs) — the same
                    // outcome ⌘Q promised.
                    window.remove_window();
                    true
                })
                .on_cancel(|_, _, _| true)
        });
        false
    }

    fn on_quit_action(&mut self, _: &QuitApp, window: &mut Window, cx: &mut Context<Self>) {
        self.request_quit(window, cx);
    }

    // -- dock & shortcuts ----------------------------------------------------

    /// Rail toggle semantics: the docked panel's own icon collapses the
    /// dock; any other icon docks that panel (and opens the dock).
    fn toggle_dock_panel(&mut self, panel: DockPanel, cx: &mut Context<Self>) {
        if self.dock_open && self.dock_panel == panel {
            self.dock_open = false;
        } else {
            self.dock_open = true;
            self.dock_panel = panel;
            // Docking the config panel seeds the gate probe (batch M — TS
            // ZCodeConfigScreen's mount effect): the gated Workflows entry
            // cannot render until the verdict lands, and an unprobed verdict
            // is fail-closed. It also refreshes the quota card (TS
            // QuotaSection's mount effect) so the allowance is current
            // without a manual tap.
            if panel == DockPanel::Config {
                self.runtime.send(UiCommand::RefreshQuota);
                if self.snap.workflow_gate.is_none() {
                    self.runtime.send(UiCommand::ProbeWorkflowGate);
                }
            }
        }
        cx.notify();
    }

    /// The right session panel's toggle (TS ChatScreen's panel button).
    /// Opening refreshes the quota card (the TS QuotaSection mount effect)
    /// and probes the gate while unknown (the TS SessionPanel effect) — the
    /// workflows row cannot render until the verdict lands.
    pub(crate) fn toggle_session_panel(&mut self, cx: &mut Context<Self>) {
        self.session_panel_open = !self.session_panel_open;
        if self.session_panel_open {
            self.runtime.send(UiCommand::RefreshQuota);
            if self.snap.workflow_gate.is_none() {
                self.runtime.send(UiCommand::ProbeWorkflowGate);
            }
        }
        cx.notify();
    }

    /// The files panel's dotfile toggle (F1): the TS `showHidden` flip.
    pub(crate) fn toggle_files_hidden(&mut self, cx: &mut Context<Self>) {
        self.files_show_hidden = !self.files_show_hidden;
        cx.notify();
    }

    /// A local-shell failure from the files panel (e.g. the Finder open) —
    /// the same ephemeral notice slot the attachment pipeline uses.
    pub(crate) fn files_notice(&mut self, text: &str, cx: &mut Context<Self>) {
        self.notice = Some(text.to_string());
        cx.notify();
    }

    /// ⌘K: dock the sessions panel and focus its search field. A config
    /// page replaces the workbench surfaces (the dock included), so the
    /// action has nothing to focus there.
    fn on_focus_search(
        &mut self,
        _: &FocusSessionSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.snap.system_status.is_some()
            || self.snap.models_page.is_some()
            || self.snap.quota_page.is_some()
            || self.snap.usage_page.is_some()
            || self.snap.config_section_page.is_some()
            || self.snap.workflows_page.is_some()
        {
            return;
        }
        self.dock_open = true;
        self.dock_panel = DockPanel::Sessions;
        // Defer the focus so the newly docked panel's input paints first
        // (same discipline as the composer's clear-on-send).
        let search = self.session_search.clone();
        window.defer(cx, move |window, cx| {
            let handle = search.read(cx).focus_handle(cx).clone();
            window.focus(&handle, cx);
        });
        cx.notify();
    }

    /// The open full-page config surface's close command, if one is up.
    /// Esc-on-page is the TS settings screen's back button (backNav/Android
    /// back); ChatView's cancel only applies on the chat surface the page
    /// replaced.
    fn open_config_page_close(&self) -> Option<UiCommand> {
        if self.snap.system_status.is_some() {
            Some(UiCommand::CloseSystemStatus)
        } else if self.snap.models_page.is_some() {
            Some(UiCommand::CloseModels)
        } else if let Some(page) = self.snap.config_section_page.as_ref() {
            Some(UiCommand::CloseConfigPage { page: page.kind })
        } else if self.snap.quota_page.is_some() {
            Some(UiCommand::CloseQuota)
        } else if self.snap.usage_page.is_some() {
            Some(UiCommand::CloseUsage)
        } else if self.snap.workflows_page.is_some() {
            Some(UiCommand::CloseWorkflows)
        } else {
            None
        }
    }

    /// Esc: close the topmost layer first — the lightbox swallows it (K2),
    /// then the rename sheet, the new-session sheet (the TS back-handler
    /// stack, `creating` guard included via the command's no-op-while-busy);
    /// then an open config page closes (the settings screen's back);
    /// otherwise it is ChatView's Cancel — `session/cancel` on the running
    /// turn. (The "/" menu's Escape never reaches here: the keystroke
    /// interceptor consumes it before the global binding.)
    pub(crate) fn on_stop_turn(&mut self, _: &StopTurn, _window: &mut Window, cx: &mut Context<Self>) {
        if self.lightbox.take().is_some() {
            // View-local — closing it is purely ours.
        } else if self.rename_target.take().is_some() {
            // The rename sheet is view-local — closing it is purely ours.
        } else if self.snap.new_session.is_some() {
            self.runtime.send(UiCommand::NewSessionToggle);
        } else if self.snap.files.viewing.is_some() {
            // The file viewer is the topmost layer (mounted after every
            // page) — Esc unwinds it before anything it covers.
            self.runtime.send(UiCommand::FilesCloseView);
        } else if std::mem::take(&mut self.files_page_open) {
            // The Session Files page is view-local — closing it is purely
            // ours (the file viewer above it stays driver-owned).
        } else if let Some(cmd) = self.open_config_page_close() {
            self.runtime.send(cmd);
        } else if self.tab.as_ref().is_some_and(|t| t.running) {
            self.runtime.send(UiCommand::CancelTurn);
        }
        cx.notify();
    }

    /// Open the rename sheet for a sidebar row (G1). The prefill lands
    /// during render (the caller — a menu click — has no window).
    fn open_rename(
        &mut self,
        instance_id: String,
        session_id: String,
        title: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.rename_target = Some((instance_id, session_id));
        self.rename_prefill = Some(title.filter(|t| !t.is_empty()).unwrap_or_default());
        cx.notify();
    }

    /// The rename sheet's Save (TS `submitRename`): dispatch and close — the
    /// driver owns the trim/cap/no-op guard, the toast owns the outcome.
    fn submit_rename(&mut self, cx: &mut Context<Self>) {
        if let Some((instance_id, session_id)) = self.rename_target.take() {
            let title = self.rename_input.read(cx).value().to_string();
            self.runtime.send(UiCommand::RenameSession {
                instance_id,
                session_id,
                title,
            });
        }
        cx.notify();
    }

    /// ⌘1..9: select the tab strip's nth tab (0-based `index`), if open.
    fn select_tab_at(&mut self, index: usize, _cx: &mut Context<Self>) {
        if let Some(tab) = self.snap.tabs.get(index) {
            self.runtime.send(UiCommand::SelectTab { id: tab.id });
        }
    }

    fn on_select_tab1(&mut self, _: &SelectTab1, _: &mut Window, cx: &mut Context<Self>) {
        self.select_tab_at(0, cx);
    }

    fn on_select_tab2(&mut self, _: &SelectTab2, _: &mut Window, cx: &mut Context<Self>) {
        self.select_tab_at(1, cx);
    }

    fn on_select_tab3(&mut self, _: &SelectTab3, _: &mut Window, cx: &mut Context<Self>) {
        self.select_tab_at(2, cx);
    }

    fn on_select_tab4(&mut self, _: &SelectTab4, _: &mut Window, cx: &mut Context<Self>) {
        self.select_tab_at(3, cx);
    }

    fn on_select_tab5(&mut self, _: &SelectTab5, _: &mut Window, cx: &mut Context<Self>) {
        self.select_tab_at(4, cx);
    }

    fn on_select_tab6(&mut self, _: &SelectTab6, _: &mut Window, cx: &mut Context<Self>) {
        self.select_tab_at(5, cx);
    }

    fn on_select_tab7(&mut self, _: &SelectTab7, _: &mut Window, cx: &mut Context<Self>) {
        self.select_tab_at(6, cx);
    }

    fn on_select_tab8(&mut self, _: &SelectTab8, _: &mut Window, cx: &mut Context<Self>) {
        self.select_tab_at(7, cx);
    }

    fn on_select_tab9(&mut self, _: &SelectTab9, _: &mut Window, cx: &mut Context<Self>) {
        self.select_tab_at(8, cx);
    }

    /// Entry point for the runtime event pump (main.rs).
    pub fn on_runtime_event(&mut self, event: RuntimeEvent, cx: &mut Context<Self>) {
        match event {
            RuntimeEvent::Snapshot(snap) => {
                self.tab = snap.active.clone();
                self.snap = snap;
                // The form lives only inside the page (TS ConfigFormSheet
                // unmounts with it); a page closed under an open form drops
                // it so a reopen starts clean.
                if self.snap.models_page.is_none() && self.models_form.is_some() {
                    self.models_form = None;
                    self.models_form_subs.clear();
                }
                if self.snap.config_section_page.is_none() {
                    // Same contract for the section form, plus the two
                    // page-local gestures (the hooks fold, the armed
                    // restore confirm).
                    self.zconfig_form = None;
                    self.zconfig_form_subs.clear();
                    self.hooks_open_event = None;
                    self.backup_confirm = None;
                }
                // The workflows forms live only inside the page (the TS
                // sheets unmount with it); a page closed under an open form
                // drops it so a reopen starts clean. The GATE sheet is
                // deliberately not touched: its entry lives in the config
                // panel (rendered only with the page CLOSED), so "page is
                // None" is its NORMAL state, not a close signal.
                if self.snap.workflows_page.is_none() {
                    self.workflow_start_form = None;
                    self.workflow_amend_form = None;
                    self.workflow_meta_form = None;
                    self.workflow_form_subs.clear();
                }
                // The detail page's view-local gestures (batch M): the
                // script fold and the armed delete confirm reset with the
                // workflow (the TS component's local state).
                let detail_key = self
                    .snap
                    .workflows_page
                    .as_ref()
                    .and_then(|p| p.detail.as_ref())
                    .map(|d| format!("{:?}/{}/{}", d.scope, d.name, d.instance_id));
                if detail_key != self.workflow_detail_key {
                    self.workflow_detail_key = detail_key;
                    self.workflow_script_open = false;
                    self.workflow_delete_confirm = false;
                }
                // The run page's raw toggles are run-local state (the TS
                // remount drops them with the view); a run switch or close
                // clears, a supersede re-point included.
                let raw_run = self
                    .snap
                    .workflows_page
                    .as_ref()
                    .and_then(|p| p.run.as_ref())
                    .map(|r| r.run_id.clone());
                if raw_run != self.workflow_raw_run {
                    self.workflow_raw_open.clear();
                    // The phase/step folds are run-local state too (the TS
                    // remount drops them with the view).
                    self.workflow_phase_open.clear();
                    self.workflow_step_open.clear();
                    self.workflow_raw_run = raw_run;
                }
                // The spend gesture is page-local state (TS useState); a
                // page closed under an open confirm dialog drops it.
                if self.snap.quota_page.is_none() {
                    self.quota_confirm = None;
                }
                // The files page belongs to the session it was opened for:
                // a session switch under an open page re-seeds the browser
                // for the new target (the TS FileBrowser's mount effect),
                // and the page closes when there is no session left to
                // browse (the capability note would otherwise masquerade as
                // "no session").
                if self.files_page_open {
                    let page_session =
                        self.snap.active.as_ref().and_then(|t| t.session_id.clone());
                    if page_session != self.files_page_session {
                        self.files_page_session = page_session.clone();
                        self.files_page_open = page_session.is_some();
                        if self.files_page_open
                            && self.snap.active.as_ref().is_some_and(|t| t.fs_capable)
                        {
                            self.runtime.send(UiCommand::FilesNavigate {
                                path: String::new(),
                            });
                        }
                    }
                }
                // Same for the App Update page's install confirm.
                if !matches!(
                    self.snap.config_section_page.as_ref().map(|p| p.kind),
                    Some(crate::runtime::ConfigPageKind::AppUpdate)
                ) {
                    self.update_confirm = false;
                }
                self.apply_snapshot_inner(cx);
            }
            RuntimeEvent::ModelWriteResult { ok } => {
                // The TS form's `if (ok) onClose()`: a successful upsert
                // closes the form (the section reload lands via the
                // snapshot); a failure keeps it open for editing.
                if ok && self.models_form.is_some() {
                    self.models_form = None;
                    self.models_form_subs.clear();
                }
                cx.notify();
            }
            RuntimeEvent::ConfigWriteResult { ok } => {
                // The same contract for the section forms (batch I): a
                // switch or restore has no form and simply ignores this.
                if ok && self.zconfig_form.is_some() {
                    self.zconfig_form = None;
                    self.zconfig_form_subs.clear();
                }
                cx.notify();
            }
            RuntimeEvent::ComposerPrefill(draft) => {
                // editPending's return path: applied on the next render pass
                // (set_value is window-gated), then consumed.
                self.composer_prefill = Some(draft);
                cx.notify();
            }
        }
    }

    /// Reconcile the row projection with the active tab's messages. This is
    /// the identity-cache port: rows are keyed by message id; only new
    /// messages build new state entities, and only changed parts update.
    fn apply_snapshot_inner(&mut self, cx: &mut Context<Self>) {
        // The approval card rides as the LAST row: take it off first so the
        // reset/append bookkeeping below only ever deals with message rows
        // (render's reconcile_card puts the current card back at the end).
        if self.card_row {
            let count = self.rows.len() + 1;
            self.scroller
                .update(cx, |state, cx| state.splice(count - 1..count, 0, cx));
            self.card_row = false;
        }
        let messages = self
            .tab
            .as_mut()
            .map(|t| std::mem::take(&mut t.messages))
            .unwrap_or_default();
        let session_id = self.tab.as_ref().and_then(|t| t.session_id.clone());
        let session_changed = self.rows_session != session_id;
        if session_changed {
            self.rows.clear();
            self.rows_session = session_id;
            // The plan strip folds back on a session switch (the TS screen
            // remounts, so its uncontrolled `<details>` resets).
            self.plan_open = false;
            self.scroller.update(cx, |state, cx| state.reset(0, cx));
        }

        // Prefix stability: any id mismatch inside the existing range means a
        // replay rewrote history — or an older page PREPENDED (load-earlier):
        // the old rows still appear, shifted right by k. Build the k new rows
        // and PREPEND them, preserving the scroll anchor (ChatView's prepend
        // anchoring); a seam merge rewrites the boundary row's id, which
        // falls through to the full rebuild.
        let prefix_stable = !session_changed
            && self
                .rows
                .iter()
                .zip(messages.iter())
                .all(|(row, msg)| row.id == msg.id);
        if !prefix_stable && !session_changed && messages.len() > self.rows.len() {
            let k = messages.len() - self.rows.len();
            let suffix_stable = !self.rows.is_empty()
                && self
                    .rows
                    .iter()
                    .zip(messages[k..].iter())
                    .all(|(row, msg)| row.id == msg.id);
            if suffix_stable {
                let mut head: Vec<RowModel> =
                    messages[..k].iter().map(|m| new_row(m, cx)).collect();
                head.append(&mut self.rows);
                self.rows = head;
                self.scroller.update(cx, |state, cx| {
                    state.prepend(k, cx);
                });
            } else {
                self.rows.clear();
                for msg in &messages {
                    self.rows.push(new_row(msg, cx));
                }
                self.scroller
                    .update(cx, |state, cx| state.reset(messages.len(), cx));
            }
        } else if !prefix_stable {
            self.rows.clear();
            for msg in &messages {
                self.rows.push(new_row(msg, cx));
            }
            self.scroller
                .update(cx, |state, cx| state.reset(messages.len(), cx));
        }

        // Append new messages as rows.
        if messages.len() > self.rows.len() {
            let added = messages.len() - self.rows.len();
            for msg in &messages[self.rows.len()..] {
                self.rows.push(new_row(msg, cx));
            }
            self.scroller.update(cx, |state, cx| {
                state.append(added, cx);
            });
        }

        // Sync the tail message's parts in place (streaming updates land on
        // the last message; earlier messages are immutable replay content).
        let mut tail_changed = false;
        if let (Some(row), Some(msg)) = (self.rows.last_mut(), messages.last()) {
            for (view, part) in row.parts.iter_mut().zip(msg.parts.iter()) {
                tail_changed |= sync_part_view(view, part, cx);
            }
            if msg.parts.len() > row.parts.len() {
                for part in &msg.parts[row.parts.len()..] {
                    row.parts.push(new_part_view(part, msg.role, cx));
                }
                tail_changed = true;
            }
        }

        if tail_changed {
            let last = self.rows.len().saturating_sub(1);
            self.scroller.update(cx, |state, cx| {
                state.remeasure_items(last..last + 1, cx);
            });
        }
        if let Some(tab) = self.tab.as_mut() {
            tab.messages = messages;
        }
        cx.notify();
    }

    /// Rebuilds the approval card entity when the pending ask changed
    /// (request id + channel) and keeps the scroller's trailing card row in
    /// place. Runs in render: the card's Textarea/TextView state entities
    /// need a window to be created.
    fn reconcile_card(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use protocol::approval::PendingAsk;
        let key = self.tab.as_ref().and_then(|t| t.approval.as_ref()).map(|a| {
            (a.request_id(), matches!(a, PendingAsk::Elicitation(_)))
        });
        let current = self.card.as_ref().map(|c| c.read(cx).key());
        if key != current {
            self.card = self
                .tab
                .as_ref()
                .and_then(|t| t.approval.clone())
                .map(|ask| cx.new(|cx| ApprovalCard::new(window, ask, cx)));
        }
        if self.card.is_some() && !self.card_row {
            self.scroller.update(cx, |state, cx| state.append(1, cx));
            self.card_row = true;
        }
    }

    fn submit_composer(&mut self, cx: &mut Context<Self>) {
        let value = self.composer.read(cx).value().to_string();
        let text = value.trim().to_string();
        let pending = self.tab.as_ref().map(|t| t.pending.len()).unwrap_or(0);
        if text.is_empty() && self.staged.is_empty() {
            // The agent-client convention (ChatView.tsx:679): an EMPTY
            // composer with queued follow-ups force-sends the queue head,
            // interrupting the running turn.
            if pending > 0 {
                self.runtime.send(UiCommand::ForceSendPending);
            }
            return;
        }
        // set_value needs a window; the clear is applied on the next render
        // pass (see Render::render). The prompt leaves now — SessionCore
        // queues the draft when a turn is running (the ChatView pending
        // flow), so the composer clearing a frame later loses nothing.
        self.clear_composer = true;
        let images = self.take_staged();
        self.runtime.send(UiCommand::SendPrompt { text, images });
        cx.notify();
    }

    // -- "/" completion menu (batch K, ChatView.tsx `Composer`) -------------

    /// The menu derived from the LIVE composer text: the word under the
    /// caret, the bridge's matching commands, the dismissal memory, and the
    /// clamped highlight. `None` when there is no slash token at all.
    fn slash_menu(&self, cx: &App) -> Option<SlashMenuState> {
        let state = self.composer.read(cx);
        let value = state.value();
        let caret = state.cursor();
        let (start, end) = slash_token_range(&value, caret)?;
        let token = value[start..end].to_string();
        let commands = self.tab.as_ref()?.available_commands.clone();
        let matches = slash_matches(&token, &commands);
        if matches.is_empty() {
            return None;
        }
        let open = self.slash_dismissed.as_deref() != Some(token.as_str());
        let highlight = self.slash_highlight.min(matches.len() - 1);
        Some(SlashMenuState {
            token,
            start,
            end,
            matches,
            open,
            highlight,
        })
    }

    /// One intercepted keystroke while the menu is open (the TS capture
    /// handler): arrows clamp, Tab/Enter commit, Escape dismisses until the
    /// query changes. Returns whether the key was consumed.
    fn on_slash_key(&mut self, key: SlashKey, cx: &mut Context<Self>) -> bool {
        let Some(menu) = self.slash_menu(cx) else {
            return false;
        };
        if !menu.open {
            return false;
        }
        match key {
            SlashKey::Up => self.slash_highlight = menu.highlight.saturating_sub(1),
            SlashKey::Down => {
                self.slash_highlight = (menu.highlight + 1).min(menu.matches.len() - 1)
            }
            SlashKey::Commit => {
                let name = menu.matches[menu.highlight].name.clone();
                self.stage_slash_select(&name, cx);
                return true;
            }
            SlashKey::Dismiss => {
                // Stay closed for THIS query; any edit reopens (TS
                // `dismissedToken !== token`).
                self.slash_dismissed = Some(menu.token);
            }
        }
        cx.notify();
        true
    }

    /// Commits one command into the draft (TS `select`): the token is
    /// replaced by "/name " (the wire name keeps a "$" prefix verbatim),
    /// the caret parks right after it, and the inserted token joins the
    /// dismissal memory so the menu does not instantly reopen.
    fn stage_slash_select(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(menu) = self.slash_menu(cx) else {
            return;
        };
        let value = self.composer.read(cx).value().to_string();
        let inserted = format!("/{name} ");
        let mut text = String::with_capacity(value.len() + inserted.len());
        text.push_str(&value[..menu.start]);
        text.push_str(&inserted);
        text.push_str(value[menu.end..].trim_start());
        let caret = menu.start + inserted.len();
        self.pending_slash_insert = Some(PendingSlashInsert {
            text: text.clone(),
            position: caret_position_of(&text, caret),
        });
        self.slash_dismissed = Some(format!("/{name}"));
        self.slash_highlight = 0;
        self.slash_token = None;
        cx.notify();
    }

    /// Opens the image lightbox (K2): a message echo or a file preview hands
    /// over its decoded raster.
    pub(crate) fn open_lightbox(
        &mut self,
        src: Arc<gpui::Image>,
        caption: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.lightbox = Some(Lightbox { src, caption });
        cx.notify();
    }

    /// The drafts to put on the wire for `text`, or None when nothing to
    /// send (TS: the send button disables on `!value.trim() && !images`).
    fn outgoing_prompt(&self, text: &str) -> Option<(String, Vec<AttachmentDraft>)> {
        let text = text.trim().to_string();
        if text.is_empty() && self.staged.is_empty() {
            return None;
        }
        let images = self.staged.iter().map(|s| s.draft.clone()).collect();
        Some((text, images))
    }

    fn take_staged(&mut self) -> Vec<AttachmentDraft> {
        std::mem::take(&mut self.staged)
            .into_iter()
            .map(|s| s.draft)
            .collect()
    }

    // -- attachment staging (ChatView addFiles / onPaste / drag-drop) ------

    /// Runs one batch of raw inputs through the pipeline off the main
    /// thread and stages the survivors, enforcing the MAX_IMAGES cap with a
    /// notice (TS `chat.attachLimit`).
    fn stage_inputs(&mut self, inputs: Vec<StageInput>, cx: &mut Context<Self>) {
        if inputs.is_empty() {
            return;
        }
        let budget = MAX_IMAGES.saturating_sub(self.staged.len());
        if budget == 0 {
            self.notice = Some(t!("chat.attachLimit").into_owned());
            cx.notify();
            return;
        }
        let mut inputs = inputs;
        if inputs.len() > budget {
            inputs.truncate(budget);
            self.notice = Some(t!("chat.attachLimit").into_owned());
        }
        let prepare = cx
            .background_executor()
            .spawn(async move { inputs.into_iter().map(|i| (prepare_image(&i.bytes, &i.mime), i.mime)).collect::<Vec<_>>() });
        cx.spawn(async move |this, cx| {
            let results = prepare.await;
            if let Some(entity) = this.upgrade() {
                let _ = cx.update_entity(&entity, |this, cx| this.apply_prepared(results, cx));
            }
        })
        .detach();
    }

    /// Lands prepared drafts on the main thread: stage the good ones, note
    /// the failures (TS `imageTooLarge` / `attachFailed` toasts).
    fn apply_prepared(
        &mut self,
        results: Vec<(Result<AttachmentDraft, PrepareError>, String)>,
        cx: &mut Context<Self>,
    ) {
        for (result, mime) in results {
            match result {
                Ok(draft) => {
                    self.staged.push(StagedImage {
                        render: render_image_of(&draft),
                        draft,
                    });
                }
                Err(PrepareError::TooLarge) => {
                    self.notice =
                        Some(format!("{mime} is too large (over 5 MB passthrough cap)"));
                }
                Err(PrepareError::Unsupported) => {
                    self.notice = Some(format!("{}: {mime}", t!("chat.attachFailed")));
                }
            }
        }
        cx.notify();
    }

    /// Removes one staged image (the thumbnail's ×).
    fn remove_staged(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.staged.len() {
            self.staged.remove(index);
            cx.notify();
        }
    }

    /// One thought card's manual toggle (ThoughtCard `touched` + `setOpen`):
    /// the new state is the NEGATION of what the click visually toggled —
    /// `open_now` is the fold state the card rendered with (auto or manual),
    /// so the first click on an auto-open streaming card collapses it and
    /// keeps it that way even after the stream moves on.
    fn set_thought_fold(
        &mut self,
        row_id: &str,
        part_index: usize,
        open_now: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.rows.iter_mut().find(|r| r.id == row_id) else {
            return;
        };
        if let Some(PartView::Markdown {
            thought: true, fold, ..
        }) = row.parts.get_mut(part_index)
        {
            *fold = Some(!open_now);
            cx.notify();
        }
    }

    /// A settled thought card's "show all" toggle (batch U): lifts (or
    /// restores) the preview cap for that one card.
    fn toggle_thought_full(&mut self, row_id: &str, part_index: usize, cx: &mut Context<Self>) {
        let key = (row_id.to_string(), part_index);
        if !self.thought_full.remove(&key) {
            self.thought_full.insert(key);
        }
        cx.notify();
    }

    /// One tool card's expand toggle (E2 — the TS `<details>` summary click):
    /// flips the part's manual fold.
    fn set_tool_expanded(
        &mut self,
        row_id: &str,
        part_index: usize,
        expanded_now: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.rows.iter_mut().find(|r| r.id == row_id) else {
            return;
        };
        if let Some(PartView::ToolCall { expanded, .. }) = row.parts.get_mut(part_index) {
            *expanded = !expanded_now;
            cx.notify();
        }
    }

    /// A row copy landed (E4 — ActionBarPrimitive.Copy): hold the row's
    /// checkmark for a beat, then clear it. The clipboard write itself
    /// happens at the click site (`App::write_to_clipboard`).
    fn mark_copied(&mut self, row_id: &str, cx: &mut Context<Self>) {
        self.copied_row = Some(row_id.to_string());
        cx.notify();
        let row_id = row_id.to_string();
        let clear = cx
            .background_executor()
            .timer(Duration::from_millis(1400));
        cx.spawn(async move |this, cx| {
            clear.await;
            if let Some(entity) = this.upgrade() {
                let _ = cx.update_entity(&entity, |wb, cx| {
                    if wb.copied_row.as_deref() == Some(&row_id) {
                        wb.copied_row = None;
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    // -- sections ----------------------------------------------------------

    fn render_status_bar(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (dot_color, label) = match &self.snap.phase {
            Phase::Connected => (theme.accent, self.snap.phase.label()),
            phase @ (Phase::Bootstrapping
            | Phase::Discovering
            | Phase::Reconnecting
            | Phase::TokenGuide { .. }) => (theme.muted_foreground, phase.label()),
            Phase::Failed(_) => (theme.danger, self.snap.phase.label()),
        };

        // Left cluster (batch U — user: "the left side carried nothing
        // actionable"): the bridge's version while connected (the phase label
        // only while there is no bridge to name), the running-session count
        // (the quit guard's number) or the active turn, then any transient
        // note. The hub address left the bar — it lives in the config dock's
        // server book, where it can be acted on.
        let bridge = self
            .tab
            .as_ref()
            .and_then(|t| t.agent_version.as_deref())
            .filter(|_| matches!(self.snap.phase, Phase::Connected))
            .map(|agent| format!("bridge {agent}"));
        let activity = if self.snap.running_session_count > 0 {
            Some(format!(
                "{} session{} running",
                self.snap.running_session_count,
                if self.snap.running_session_count == 1 {
                    ""
                } else {
                    "s"
                }
            ))
        } else if self.tab.as_ref().is_some_and(|t| t.running) {
            Some("turn running".to_string())
        } else {
            None
        };
        let mut bar = div()
            .flex()
            .items_center()
            .gap_3()
            .h_9()
            .px_3()
            .flex_none()
            .relative()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.title_bar)
            .child(div().size_2().flex_none().rounded_full().bg(dot_color))
            // The left cluster owns the slack: every segment truncates so a
            // long note can never push the right cluster off-screen.
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .id("status-bridge")
                            .test_support()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(theme.foreground)
                            .child(bridge.unwrap_or_else(|| label.to_string())),
                    )
                    .when_some(activity, |this, text| {
                        this.child(
                            div()
                                .id("status-activity")
                                .test_support()
                                .flex_none()
                                .text_size(px(11.))
                                .text_color(theme.accent)
                                .child(text),
                        )
                    })
                    .when_some(self.snap.bootstrap_note.clone(), |this, note| {
                        this.child(
                            div()
                                .id("status-bar-note")
                                .test_support()
                                .min_w_0()
                                .truncate()
                                .text_size(px(11.))
                                .text_color(theme.muted_foreground)
                                .child(note),
                        )
                    }),
            );

        if let Phase::Failed(message) = &self.snap.phase {
            bar = bar.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.))
                    .text_color(theme.danger)
                    .child(message.clone()),
            );
            let runtime = self.runtime.clone();
            bar = bar.child(
                Button::new("retry")
                    .secondary()
                    .small()
                    .label("Retry")
                    .on_click(move |_, _, _| runtime.send(UiCommand::Retry)),
            );
        }

        // Right cluster: the hub's preformatted quota dock line (ADR-0021) —
        // byte-identical to the TUI's bottom dock and the mobile title bar's
        // tail segment, refreshed at connect and each turn end. Global account
        // state, so it sits opposite the bridge/activity cluster. It takes
        // whatever width the left cluster leaves free (no pixel cap — the
        // line used to clip while half the bar stood empty) and only
        // truncates past 60% of the bar, so an absurd line still cannot
        // push the left cluster off-screen.
        let mut right = div()
            .flex()
            .items_center()
            .gap_3()
            .ml_auto()
            .flex_none()
            .min_w_0()
            .max_w(relative(0.6));
        if let Some(dock) = self.snap.quota_dock.clone() {
            right = right.child(
                div()
                    .id("status-quota-dock")
                    .test_support()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.))
                    .font_family(theme.mono_font_family.clone())
                    .text_color(theme.muted_foreground)
                    .child(dock),
            );
        }
        bar = bar.child(right);
        bar.into_any_element()
    }

    // -- dock ----------------------------------------------------------------

    /// The left dock (ADR-0009's dock-style window): a rail of panel toggles
    /// plus the docked panel, resizable against the chat area through the
    /// splitter (`ResizableState` keeps the dragged width across
    /// collapse/expand and config-page round trips). Collapsed, the rail
    /// alone remains.
    fn render_dock(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();

        // The rail: one toggle per dockable panel.
        let mut rail = div()
            .w(px(40.))
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .gap_1()
            .p_1()
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.title_bar);
        rail = rail.child(self.render_rail_button(
            DockPanel::Sessions,
            KitIconName::MessageSquare,
            "Sessions — live sessions, history, and the ⌘K search",
            cx,
        ));
        rail = rail.child(self.render_rail_button(
            DockPanel::Config,
            IconName::Settings2,
            "Configuration — models, usage, quota, system status",
            cx,
        ));

        if !self.dock_open {
            // Collapsed: the rail plus the chat area at full width.
            return div()
                .flex()
                .flex_1()
                .min_h_0()
                .child(rail)
                .child(self.render_chat_area(cx))
                .into_any_element();
        }
        let panel = match self.dock_panel {
            DockPanel::Sessions => self.render_sidebar(cx),
            DockPanel::Config => self.render_config_panel(cx),
        };
        div()
            .flex()
            .flex_1()
            .min_h_0()
            .child(rail)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .child(
                        h_resizable("dock-split")
                            .with_state(&self.dock_sizes)
                            .child(
                                resizable_panel()
                                    .size(px(280.))
                                    .size_range(px(200.)..px(520.))
                                    .flex_none()
                                    .child(panel),
                            )
                            .child(resizable_panel().child(self.render_chat_area(cx))),
                    ),
            )
            .into_any_element()
    }

    /// The chat area: the chat column alone, or split against the right
    /// session panel while it is open (its own splitter — the left dock's
    /// sizes are a different pair).
    fn render_chat_area(&self, cx: &Context<Self>) -> AnyElement {
        if !self.session_panel_open {
            return self.render_chat_column(cx);
        }
        div()
            .flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .child(
                h_resizable("session-split")
                    .with_state(&self.session_dock_sizes)
                    .child(resizable_panel().child(self.render_chat_column(cx)))
                    .child(
                        resizable_panel()
                            .size(px(280.))
                            .size_range(px(220.)..px(480.))
                            .flex_none()
                            .child(self.render_session_panel(cx)),
                    ),
            )
            .into_any_element()
    }

    /// The chat column both dock states share: tab strip, the scroller, the
    /// ephemeral toast strip, and the composer.
    fn render_chat_column(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let runtime = self.runtime.clone();
        // The guided setup card replaces the column while it is up
        // (requirements §3.1): there is no hub to chat with yet, and the
        // exact write needs full-width visibility.
        if let Phase::TokenGuide {
            path,
            preview,
            error,
        } = &self.snap.phase
        {
            return self.render_token_guide(path, preview, error.as_deref(), cx);
        }
        let toast = self.snap.toast.as_ref().map(|t| t.text.clone());
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .child(self.render_tab_strip(cx))
            // The plan/todo strip (E3 — ChatScreen's PlanPanel above the
            // chat): only with entries from the active session's `plan`
            // update.
            .when_some(
                self.tab
                    .as_ref()
                    .and_then(|t| t.plan_entries.clone())
                    .filter(|entries| !entries.is_empty()),
                |this, entries| this.child(self.render_plan_bar(&entries, cx)),
            )
            .child(self.render_chat(cx))
            // The ephemeral toast strip (notify-toast semantics: approval
            // requested in a non-active session, attachment notices…).
            .when_some(toast, |this, text| {
                this.child(render_toast_strip(&runtime, &text, &theme))
            })
            .child(self.render_composer(cx))
            .into_any_element()
    }

    /// The guided remote-access setup card (Phase::TokenGuide): the exact
    /// file write a confirmation performs, shown in full before any button
    /// can trigger it (requirements §3.1 — never silently).
    fn render_token_guide(
        &self,
        path: &str,
        preview: &str,
        error: Option<&str>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let runtime = self.runtime.clone();
        let mut card = div()
            .flex()
            .flex_col()
            .gap_3()
            .max_w(px(640.))
            .w_full()
            .p_4()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.title_bar)
            .child(
                div()
                    .text_size(px(15.))
                    .text_color(theme.foreground)
                    .child("Enable remote access"),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(theme.muted_foreground)
                    .child(
                        "No hub is running and the config has no access token. Confirming \
                         writes the token below into your zcode-acp config, then starts the hub.",
                    ),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .font_family(theme.mono_font_family.clone())
                    .text_color(theme.foreground)
                    .child(path.to_string()),
            )
            .child(
                div()
                    .id("token-preview")
                    .max_h(px(240.))
                    .overflow_y_scroll()
                    .w_full()
                    .p_3()
                    .rounded_md()
                    .bg(theme.border)
                    .text_size(px(12.))
                    .font_family(theme.mono_font_family.clone())
                    .text_color(theme.muted_foreground)
                    .child(preview.to_string()),
            );
        if let Some(err) = error {
            card = card.child(
                div()
                    .text_size(px(13.))
                    .text_color(theme.danger)
                    .child(err.to_string()),
            );
        }
        card = card.child(
            div()
                .flex()
                .gap_2()
                .child(
                    Button::new("token-write")
                        .primary()
                        .label("Write and connect")
                        .on_click({
                            let runtime = runtime.clone();
                            move |_, _, _| runtime.send(UiCommand::ConfirmTokenWrite)
                        }),
                )
                .child(
                    Button::new("token-regenerate")
                        .secondary()
                        .label("Regenerate")
                        .on_click({
                            let runtime = runtime.clone();
                            move |_, _, _| runtime.send(UiCommand::Retry)
                        }),
                ),
        );
        div()
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .justify_center()
            .p_8()
            .child(card)
            .into_any_element()
    }

    fn render_rail_button(
        &self,
        panel: DockPanel,
        icon: impl Into<gpui_kit::component::Icon> + Clone,
        tooltip: &str,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let wb = cx.entity();
        let active = self.dock_open && self.dock_panel == panel;
        let color = if active {
            theme.foreground
        } else {
            theme.muted_foreground
        };
        let label = tooltip.to_string();
        div()
            .id(SharedString::from(format!("dock-rail-{}", panel.key())))
            .flex()
            .items_center()
            .justify_center()
            .size_8()
            .rounded_md()
            .cursor_pointer()
            .when(active, |this| this.bg(theme.secondary))
            .hover(|this| this.bg(theme.secondary.opacity(0.6)))
            .child(Icon::new(icon).size_4().text_color(color))
            .tooltip(move |window, cx| Tooltip::new(label.clone()).build(window, cx))
            .test_support()
            .on_click(move |_, _, cx| {
                wb.update(cx, |wb, cx| wb.toggle_dock_panel(panel, cx));
            })
    }

    /// The Servers section of the config dock — the Connection Profile book
    /// (CONTEXT.md): the implicit local hub, every saved remote hub, and the
    /// Add button. Add and Edit open the `ServerForm` dialog, so the section
    /// stays a list; switching restarts the dial on the new target.
    fn render_servers_section(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let runtime = self.runtime.clone();
        let mut column = div().flex().flex_col().gap_1();

        {
            let runtime = runtime.clone();
            column = column.child(
                div()
                    .flex()
                    .items_center()
                    .px_2()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.muted_foreground)
                            .child("Servers"),
                    )
                    .child(
                        div().ml_auto().child(
                            Button::new("server-add")
                                .ghost()
                                .xsmall()
                                .icon(IconName::Plus)
                                .tooltip("Add a remote server")
                                .on_click(move |_, window, cx| {
                                    ServerForm::open(None, runtime.clone(), window, cx)
                                }),
                        ),
                    ),
            );
        }

        // The implicit local entry: Hub Bootstrap dials (and spawns when
        // dark). No book record — `active_server_id: None` IS this entry.
        let local_active = self.snap.active_server_id.is_none();
        let mut local_row = div()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .rounded_md()
            .child(
                div()
                    .size_2()
                    .flex_none()
                    .rounded_full()
                    .bg(if local_active {
                        theme.accent
                    } else {
                        theme.border
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .flex_1()
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(theme.foreground)
                            .child("Local hub"),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(theme.muted_foreground)
                            .child("Spawned or discovered on this machine"),
                    ),
            );
        if !local_active {
            let runtime = runtime.clone();
            local_row = local_row.child(
                Button::new("server-local-switch")
                    .ghost()
                    .xsmall()
                    .icon(KitIconName::Plug)
                    .tooltip("Switch to the local hub")
                    .on_click(move |_, _, _| runtime.send(UiCommand::SwitchProfile { id: None })),
            );
        }
        column = column.child(local_row);

        for server in &self.snap.servers {
            let active = self.snap.active_server_id.as_deref() == Some(server.id.as_str());
            // Icon-only actions: the dock panel is narrow, and three text
            // buttons pushed each other out of the row.
            let mut row = div()
                .flex()
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .rounded_md()
                .child(
                    div()
                        .size_2()
                        .flex_none()
                        .mr_1()
                        .rounded_full()
                        .bg(if active { theme.accent } else { theme.border }),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .flex_1()
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(13.))
                                .text_color(theme.foreground)
                                .child(server.name.clone()),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.))
                                .text_color(theme.muted_foreground)
                                .font_family(theme.mono_font_family.clone())
                                .child(server.hub_url.clone()),
                        ),
                );
            if !active {
                let runtime = runtime.clone();
                let id = server.id.clone();
                row = row.child(
                    Button::new(SharedString::from(format!("server-switch-{}", server.id)))
                        .ghost()
                        .xsmall()
                        .icon(KitIconName::Plug)
                        .tooltip("Switch to this hub")
                        .on_click(move |_, _, _| {
                            runtime.send(UiCommand::SwitchProfile { id: Some(id.clone()) })
                        }),
                );
            }
            // Edit (TS connect.editServer): the dialog opens prefilled from
            // the record; Save keeps the active id and only re-dials when
            // the EDITED server was the active one (the driver's rule).
            {
                let runtime = runtime.clone();
                let server = server.clone();
                row = row.child(
                    Button::new(SharedString::from(format!("server-edit-{}", server.id)))
                        .ghost()
                        .xsmall()
                        .icon(KitIconName::Pencil)
                        .tooltip("Edit")
                        .on_click(move |_, window, cx| {
                            ServerForm::open(Some(&server), runtime.clone(), window, cx)
                        }),
                );
            }
            let runtime_remove = runtime.clone();
            let remove_id = server.id.clone();
            // Two-click delete (TS ConnectScreen:46-54): the first click
            // arms a 3s confirm window; the re-click validates the window
            // at CLICK time, so a stale armed label can never delete.
            let armed = matches!(&self.server_remove_armed, Some((id, at))
                if *id == server.id && at.elapsed() < std::time::Duration::from_secs(3));
            let wb_arm = cx.entity();
            let arm_id = server.id.clone();
            let remove = Button::new(SharedString::from(format!("server-remove-{}", server.id)))
                .xsmall();
            let remove = if armed {
                remove.danger().label("Confirm remove")
            } else {
                remove.ghost().icon(KitIconName::Trash).tooltip("Remove")
            };
            row = row.child(remove.on_click(move |_, _, cx| {
                let arm_id = arm_id.clone();
                let confirmed = wb_arm.update(cx, |wb, cx| {
                    let fresh = matches!(&wb.server_remove_armed, Some((id, at))
                        if *id == arm_id
                            && at.elapsed() < std::time::Duration::from_secs(3));
                    wb.server_remove_armed = if fresh {
                        None
                    } else {
                        Some((arm_id.clone(), std::time::Instant::now()))
                    };
                    cx.notify();
                    fresh
                });
                if confirmed {
                    runtime_remove.send(UiCommand::RemoveServer { id: remove_id.clone() });
                }
            }));
            column = column.child(row);
        }

        column.into_any_element()
    }

    // -- language (batch H3) ---------------------------------------------------

    /// Apply the settings panel's language pick: flip the process-global
    /// rust-i18n locale, persist the choice (the TS `saveLang` key's
    /// counterpart), and repaint — every chrome string resolves through
    /// `t!()` at paint time, so one notify switches the whole window (and
    /// gpui-kit's own component strings, which ride the same global).
    fn switch_locale(&mut self, locale: &'static str, cx: &mut Context<Self>) {
        if &*rust_i18n::locale() == locale {
            return;
        }
        rust_i18n::set_locale(locale);
        crate::ui_prefs::store_locale(locale);
        cx.notify();
    }

    /// The settings panel's language segment (SidePanel.tsx:167-187): two
    /// native-named options in one kit ButtonGroup, the active one selected.
    /// The catalogs carry exactly `en` + `zh-CN` (i18n.rs collapsed any zh*
    /// system locale already), so the segment never needs a third option.
    fn render_language_section(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let wb = cx.entity();
        let zh = rust_i18n::locale().starts_with("zh");
        div()
            .flex()
            .flex_col()
            .gap_1()
            .min_w_0()
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child(t!("panel.language").into_owned()),
            )
            .child(
                ButtonGroup::new("lang-segment")
                    .small()
                    .flex_1()
                    .min_w_0()
                    .child(Button::new("lang-en").label("English").selected(!zh))
                    .child(Button::new("lang-zh").label("中文").selected(zh))
                    .on_click(move |clicks, _, cx| {
                        // Single-select group: the click's index is the pick.
                        if let Some(&ix) = clicks.first() {
                            let locale = if ix == 0 {
                                crate::ui_prefs::LOCALE_EN
                            } else {
                                crate::ui_prefs::LOCALE_ZH
                            };
                            wb.update(cx, |wb, cx| wb.switch_locale(locale, cx));
                        }
                    }),
            )
            .into_any_element()
    }

    // -- font size -------------------------------------------------------------

    /// Apply the settings panel's font-size pick: persist (the TS
    /// `saveFontSize`) and repaint — the chat rows read the ladder at paint
    /// time, so one notify rescales every bubble.
    fn set_font_scale(&mut self, scale: crate::ui_prefs::FontScale, cx: &mut Context<Self>) {
        if self.font_scale == scale {
            return;
        }
        self.font_scale = scale;
        crate::ui_prefs::store_font_size(scale.key());
        cx.notify();
    }

    /// The settings panel's font-size segment (SidePanel.tsx:189-210): the
    /// three ladder steps in one kit ButtonGroup, the active one selected.
    /// A global preference, so it sits in the left dock beside the language
    /// segment — not in the session panel.
    fn render_font_size_section(&self, cx: &Context<Self>) -> AnyElement {
        use crate::ui_prefs::FontScale;
        let theme = cx.theme();
        let wb = cx.entity();
        let current = self.font_scale;
        div()
            .flex()
            .flex_col()
            .gap_1()
            .min_w_0()
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(theme.muted_foreground)
                    .child(t!("panel.fontSize").into_owned()),
            )
            .child(
                ButtonGroup::new("font-segment")
                    .small()
                    .flex_1()
                    .min_w_0()
                    .child(
                        Button::new("font-small")
                            .label(t!("panel.fontSizeSmall").into_owned())
                            .selected(current == FontScale::Small),
                    )
                    .child(
                        Button::new("font-medium")
                            .label(t!("panel.fontSizeMedium").into_owned())
                            .selected(current == FontScale::Medium),
                    )
                    .child(
                        Button::new("font-large")
                            .label(t!("panel.fontSizeLarge").into_owned())
                            .selected(current == FontScale::Large),
                    )
                    .on_click(move |clicks, _, cx| {
                        // Single-select group: the click's index is the pick.
                        let scale = match clicks.first() {
                            Some(0) => FontScale::Small,
                            Some(1) => FontScale::Medium,
                            Some(2) => FontScale::Large,
                            _ => return,
                        };
                        wb.update(cx, |wb, cx| wb.set_font_scale(scale, cx));
                    }),
            )
            .into_any_element()
    }

    /// The docked configuration panel: the entry list for the high-frequency
    /// config pages (requirements §4). The TS app reaches these from the
    /// settings panel; the workbench's equivalent home is this dock panel —
    /// the pages themselves stay full pages (ADR-0009).
    fn render_config_panel(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let runtime = self.runtime.clone();
        let mut column = div().flex().flex_col().gap_2().p_2();

        column = column.child(
            div()
                .text_size(px(12.))
                .font_family(theme.mono_font_family.clone())
                .text_color(theme.muted_foreground)
                .child("CONFIGURATION"),
        );

        column = column.child(self.render_servers_section(cx));

        // The language segment (batch H3, SidePanel.tsx:167-187): two
        // native-named options in one rounded track, the active one raised.
        // Both the header and the flip's effect resolve through t!() at
        // paint time, so the pick re-renders the whole chrome at once.
        column = column.child(self.render_language_section(cx));

        // The font-size segment (SidePanel.tsx:189-210): the chat ladder,
        // beside the language pick — both are appearance preferences.
        column = column.child(self.render_font_size_section(cx));

        // The hub staleness check (TS SettingsPanel `checkHubUpgrade`): one
        // button, busy while POST /api/upgrade flies, the verdict note under
        // it. The hub alone decides whether to restart; discovery's poll and
        // the tabs' reconnect ride out a respawn.
        {
            let runtime = runtime.clone();
            let busy = self.snap.hub_upgrade_busy;
            let mut block = div().flex().flex_col().gap_1().min_w_0().child(
                Button::new("hub-upgrade-check")
                    .secondary()
                    .small()
                    .w_full()
                    .label(
                        if busy {
                            t!("panel.upgradeBusy")
                        } else {
                            t!("panel.upgrade")
                        }
                        .into_owned(),
                    )
                    .disabled(busy)
                    .on_click(move |_, _, _| runtime.send(UiCommand::UpgradeHub)),
            );
            if let Some(note) = self.snap.hub_upgrade_note.as_deref() {
                block = block.child(
                    div()
                        .id("hub-upgrade-note")
                        .test_support()
                        .text_size(px(12.))
                        .text_color(theme.muted_foreground)
                        .child(note.to_string()),
                );
            }
            column = column.child(block);
        }

        // (icon, title, description, entry command constructor). The command
        // is built inside the click closure — click listeners are `Fn`, so a
        // prebuilt command would have to be Clone for nothing. The five
        // batch-I sections ride the same list (requirements §7); the
        // Workflows page joins only while the gate verdict says the feature
        // is ON (batch M — TS hides the row unless `workflowGate?.enabled
        // === true`; fail-closed, the gate switch row below is the way in).
        let mut entries: Vec<(KitIconName, &str, &str, fn() -> UiCommand)> = vec![
            (
                KitIconName::Boxes,
                "Models",
                "Selectable models, context windows, and model rules",
                || UiCommand::OpenModels,
            ),
            (
                KitIconName::ChartColumn,
                "Usage statistics",
                "The account's plan usage and this machine",
                || UiCommand::OpenUsage,
            ),
            (
                KitIconName::Gauge,
                "Plan quota",
                "Allowance windows and reset cards",
                || UiCommand::OpenQuota,
            ),
            (
                KitIconName::Activity,
                "System status",
                "Machine health: battery, CPU, memory, storage",
                || UiCommand::OpenSystemStatus,
            ),
            (
                KitIconName::Puzzle,
                "Skills",
                "Discovered skills and their enable state",
                || UiCommand::OpenConfigPage {
                    page: crate::runtime::ConfigPageKind::Skills,
                },
            ),
            (
                KitIconName::PlugZap,
                "MCP servers",
                "Configured MCP servers (stdio and remote)",
                || UiCommand::OpenConfigPage {
                    page: crate::runtime::ConfigPageKind::Mcp,
                },
            ),
            (
                KitIconName::Webhook,
                "Hooks",
                "Per-event commands the agent runs",
                || UiCommand::OpenConfigPage {
                    page: crate::runtime::ConfigPageKind::Hooks,
                },
            ),
            (
                KitIconName::Bot,
                "Agents",
                "Subagents: built-ins and personal agents",
                || UiCommand::OpenConfigPage {
                    page: crate::runtime::ConfigPageKind::Agents,
                },
            ),
            (
                KitIconName::ArchiveRestore,
                "Backups",
                "Restore an earlier version of a config file",
                || UiCommand::OpenConfigPage {
                    page: crate::runtime::ConfigPageKind::Backups,
                },
            ),
            (
                KitIconName::Download,
                "App update",
                "Check the release channel and install a newer app",
                || UiCommand::OpenConfigPage {
                    page: crate::runtime::ConfigPageKind::AppUpdate,
                },
            ),
        ];
        if self
            .snap
            .workflow_gate
            .as_ref()
            .is_some_and(|g| g.enabled)
        {
            entries.push((
                KitIconName::Workflow,
                "Workflows",
                "Saved dynamic workflows and their runs",
                || UiCommand::OpenWorkflows,
            ));
        }
        for (icon, title, description, open) in entries {
            let runtime = runtime.clone();
            column = column.child(
                div()
                    .id(SharedString::from(format!("config-entry-{title}")))
                    .test_support()
                    .flex()
                    .items_start()
                    .gap_2()
                    .px_2()
                    .py_2()
                    .rounded_md()
                    .hover(|this| this.bg(theme.secondary.opacity(0.6)))
                    .cursor_pointer()
                    .child(
                        Icon::new(icon)
                            .size_4()
                            .flex_none()
                            .text_color(theme.muted_foreground),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .text_color(theme.foreground)
                                    .child(title.to_string()),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(theme.muted_foreground)
                                    .child(description.to_string()),
                            ),
                    )
                    .on_click(move |_, _, _| runtime.send(open())),
            );
        }

        // The machine-level workflow switch (J2, bridge ≥0.53): ALWAYS
        // visible — with the remote verdict off this override is the only
        // way in, so unlike the gated page above it must not hide. The
        // subtitle names the switch's current position (auto until a probe
        // or a write says otherwise — fail-closed, never "off").
        let wb_gate = cx.entity();
        let gate_label = crate::workflows_page::gate_position_label(
            crate::workflows_page::gate_position(self.snap.workflow_gate.as_ref()),
        );
        column = column.child(
            div()
                .id("config-entry-Workflow switch")
                .test_support()
                .flex()
                .items_start()
                .gap_2()
                .px_2()
                .py_2()
                .rounded_md()
                .hover(|this| this.bg(theme.secondary.opacity(0.6)))
                .cursor_pointer()
                .child(
                    Icon::new(KitIconName::Workflow)
                        .size_4()
                        .flex_none()
                        .text_color(theme.muted_foreground),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .min_w_0()
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(theme.foreground)
                                .child(t!("zconfig.workflowGate").into_owned()),
                        )
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(theme.muted_foreground)
                                .child(gate_label),
                        ),
                )
                .on_click(move |_, _, cx| {
                    // Opening the sheet also seeds the verdict probe (the
                    // position label is only honest once read).
                    wb_gate.update(cx, |this, cx| {
                        this.workflow_gate_sheet = true;
                        if this.snap.workflow_gate.is_none() {
                            this.runtime.send(UiCommand::ProbeWorkflowGate);
                        }
                        cx.notify();
                    });
                }),
        );

        // The account quota card (TS SettingsPanel's `<QuotaSection />`):
        // hub-level, so it sits with the global settings as well as in the
        // session panel.
        if let Some(card) =
            crate::quota_card::render_quota_card(&self.snap.quota_card, &self.runtime, theme)
        {
            column = column.child(card);
        }

        div()
            .id("config-panel")
            .h_full()
            .w_full()
            .min_w_0()
            .flex_none()
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.title_bar)
            .overflow_y_scroll()
            .overflow_x_hidden()
            .child(column)
            .into_any_element()
    }

    // -- session panel (right) ----------------------------------------------

    /// The right session panel (TS SessionPanel): the active session's
    /// config options as full-width pickers, its turn stats (context,
    /// cache hit, speed — the title bar's readouts at reading distance),
    /// the gated workflows entry, and the shared quota card. Global settings
    /// stay in the config dock.
    fn render_session_panel(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let runtime = self.runtime.clone();
        let wb_close = cx.entity();
        let mut column = div().flex().flex_col().min_w_0();

        column = column.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .h_9()
                .px_3()
                .flex_none()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    div()
                        .text_size(px(12.))
                        .font_family(theme.mono_font_family.clone())
                        .text_color(theme.muted_foreground)
                        .child(t!("panel.session").to_uppercase()),
                )
                .child(
                    Button::new("session-panel-close")
                        .ghost()
                        .xsmall()
                        .icon(KitIconName::X)
                        .on_click(move |_, _, cx| {
                            wb_close.update(cx, |this, cx| this.toggle_session_panel(cx));
                        }),
                ),
        );

        let session_live = self
            .snap
            .active
            .as_ref()
            .and_then(|t| t.session_id.clone())
            .is_some();
        let caption = |text: String| {
            div()
                .px_3()
                .pt(px(12.))
                .pb(px(2.))
                .text_size(px(12.))
                .text_color(theme.muted_foreground.opacity(0.75))
                .child(text.to_uppercase())
        };

        // Session-scoped config — meaningless until a session is attached
        // (the TS guard), and the TS label fallback: a catalog miss shows
        // the bridge's own option name, never the raw key.
        if let Some(view) = self
            .snap
            .active
            .as_ref()
            .filter(|v| session_live && !v.config_options.is_empty())
        {
            column = column.child(caption(t!("config.title").into_owned()));
            for opt in &view.config_options {
                let key = format!("config.{}", opt.id);
                let translated = t!(&key);
                let label = if translated.as_ref() == key {
                    opt.name.clone().unwrap_or_else(|| opt.id.clone())
                } else {
                    translated.into_owned()
                };
                let (current, choices) = config_option_view(opt);
                let runtime = runtime.clone();
                let config_id = opt.id.clone();
                column = column.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .px_3()
                        .py(px(4.))
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(13.))
                                .text_color(theme.muted_foreground)
                                .child(label),
                        )
                        .child(
                            Button::new(SharedString::from(format!("session-cfg-{}", opt.id)))
                                .ghost()
                                .xsmall()
                                .min_w_0()
                                .label(current)
                                .dropdown_menu(move |menu, _, _| {
                                    config_option_menu(menu, &runtime, &config_id, &choices)
                                }),
                        ),
                );
            }
        }

        // The turn readouts (the title bar's segments, spelled out): only
        // the ones the session has reported.
        if let Some(tab) = self.tab.as_ref().filter(|_| session_live) {
            let mut rows: Vec<(&str, String)> = Vec::new();
            if let Some(usage) = tab.usage.filter(|u| u.size > 0) {
                rows.push(("Context", fmt_context(usage.used, usage.size)));
            }
            if let Some(hit) = tab.cache_hit {
                rows.push(("Cache hit", format!("{hit}%")));
            }
            if let Some(stats) = tab.turn_stats {
                rows.push(("Speed", format!("{} tok/s", stats.tps)));
                rows.push((
                    "Last turn",
                    format!("{} tok · {:.1}s", stats.output_tokens, stats.seconds),
                ));
            }
            if !rows.is_empty() {
                column = column.child(caption("Session stats".to_string()));
                let mut block = div()
                    .id("session-panel-stats")
                    .test_support()
                    .flex()
                    .flex_col();
                for (label, value) in rows {
                    block = block.child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .px_3()
                            .py(px(4.))
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
                                    .font_family(theme.mono_font_family.clone())
                                    .text_color(theme.foreground)
                                    .child(value),
                            ),
                    );
                }
                column = column.child(block);
            }
        }

        // The single workflow entry (batch M): shown when the gate verdict
        // is ON and a session is attached; everything workflow-specific
        // lives INSIDE the management page. Opening it also drops the files
        // page — one full page at a time.
        if session_live && self.snap.workflow_gate.as_ref().is_some_and(|g| g.enabled) {
            let runtime = runtime.clone();
            let wb = cx.entity();
            column = column.child(caption(t!("panel.workflows").into_owned())).child(
                Button::new("session-panel-workflows")
                    .ghost()
                    .small()
                    .w_full()
                    .icon(KitIconName::Workflow)
                    .label(t!("panel.workflowManage").into_owned())
                    .on_click(move |_, _, cx| {
                        wb.update(cx, |wb, cx| {
                            wb.files_page_open = false;
                            cx.notify();
                        });
                        runtime.send(UiCommand::OpenWorkflows);
                    }),
            );
        }

        // Session Files (F1): session-scoped, so its entry lives HERE — the
        // row opens the browser as a full page (left dock = global things,
        // right panel = this session's things).
        if session_live {
            let wb = cx.entity();
            column = column.child(caption(t!("files.title").into_owned())).child(
                Button::new("session-panel-files")
                    .ghost()
                    .small()
                    .w_full()
                    .icon(KitIconName::Folder)
                    .label(t!("files.title").into_owned())
                    .on_click(move |_, window, cx| {
                        let focus = wb.update(cx, |wb, cx| {
                            wb.open_files_page(cx);
                            wb.files_page_focus.clone()
                        });
                        // The row unmounts with the dock the page replaces;
                        // hand focus to the page so Esc still lands.
                        window.focus(&focus, cx);
                    }),
            );
        }

        let quota = crate::quota_card::render_quota_card(&self.snap.quota_card, &self.runtime, theme);
        let quota_present = quota.is_some();
        if let Some(card) = quota {
            column = column.child(card);
        }
        if !session_live && !quota_present {
            column = column.child(
                div()
                    .id("session-panel-empty")
                    .test_support()
                    .px_3()
                    .py_2()
                    .text_size(px(13.))
                    .text_color(theme.muted_foreground)
                    .child(t!("chat.noSession").into_owned()),
            );
        }

        div()
            .id("session-panel")
            .test_support()
            .h_full()
            .w_full()
            .min_w_0()
            .flex_none()
            .border_l_1()
            .border_color(theme.border)
            .bg(theme.title_bar)
            .overflow_y_scroll()
            .overflow_x_hidden()
            .child(column)
            .into_any_element()
    }

    // -- files page ----------------------------------------------------------

    /// Open the Session Files page from the right panel's row: seed a
    /// listing where the bridge advertises the capability (the TS
    /// FileBrowser mount effect — without it a request would just 404) and
    /// swap the chat surfaces for the page.
    pub(crate) fn open_files_page(&mut self, cx: &mut Context<Self>) {
        self.files_page_open = true;
        self.files_page_session = self.snap.active.as_ref().and_then(|t| t.session_id.clone());
        if self.tab.as_ref().is_some_and(|t| t.fs_capable) {
            self.runtime.send(UiCommand::FilesNavigate {
                path: String::new(),
            });
        }
        cx.notify();
    }

    fn close_files_page(&mut self, cx: &mut Context<Self>) {
        self.files_page_open = false;
        cx.notify();
    }

    /// The Session Files page (F1): the driver's listing state painted by
    /// `file_browser` as a full page — session-scoped, so it opens from the
    /// right session panel, never the left dock. Back returns to the chat;
    /// the view-local dotfile toggle and the loopback-gated Finder
    /// affordance (F2) ride the panel's own header.
    fn render_files_page(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let wb = cx.entity();
        div()
            .id("files-page")
            .test_support()
            .track_focus(&self.files_page_focus)
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .flex_none()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("files-page-back")
                            .ghost()
                            .small()
                            .icon(KitIconName::ArrowLeft)
                            .tooltip("Back to the chat")
                            .on_click(move |_, _, cx| {
                                wb.update(cx, |wb, cx| wb.close_files_page(cx));
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(14.))
                            .child(t!("files.title").into_owned()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(render_files_panel(
                        &self.snap.files,
                        &FilesPanelCtx {
                            runtime: self.runtime.clone(),
                            wb: cx.entity(),
                            show_hidden: self.files_show_hidden,
                            hub_addr: self.snap.hub_addr.as_deref(),
                            fs_capable: self.tab.as_ref().is_some_and(|t| t.fs_capable),
                        },
                        cx,
                    )),
            )
            .into_any_element()
    }

    // -- sessions panel ------------------------------------------------------

    /// The sessions dock panel: the search field (⌘K), discovery's live
    /// session list (Session Activity badges), and the Session History
    /// browser (ProjectHistoryDialog port).
    fn render_sidebar(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let mut column = div().flex().flex_col().gap_2().p_2();

        // The session search — the TS SessionList filter (⌘K focuses it).
        let query = self.session_search.read(cx).value().trim().to_lowercase();
        column = column.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    // NOTE: kit icon sizing goes through Sizable (.small()),
                    // never Styled's size_3_5() — this gpui fork generates
                    // fractional tailwind suffixes as RELATIVE lengths
                    // (3_5 = 60% of the parent), so a style-sized icon in a
                    // wide row renders giant.
                    Icon::new(IconName::Search)
                        .small()
                        .flex_none()
                        .text_color(theme.muted_foreground),
                )
                .child(
                    Input::new(&self.session_search)
                        .small()
                        .bordered(false)
                        .appearance(false)
                        .flex_1()
                        .min_w_0(),
                ),
        );

        // Section header: sessions + new + refresh. The `+` mirrors the tab
        // strip's new-tab (same NewSessionToggle chooser) — the list is the
        // sessions home, so starting one belongs here too.
        let runtime = self.runtime.clone();
        let runtime_new = self.runtime.clone();
        column = column.child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .text_size(px(12.))
                        .font_family(theme.mono_font_family.clone())
                        .text_color(theme.muted_foreground)
                        .child(t!("chat.sessions").to_uppercase()),
                )
                .child(div().flex_1())
                .child(
                    Button::new("sessions-new")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Plus)
                        .tooltip("New session — pick the project directory it belongs to")
                        .on_click(move |_, _, _| runtime_new.send(UiCommand::NewSessionToggle)),
                )
                .child(
                    Button::new("refresh-sessions")
                        .ghost()
                        .xsmall()
                        .icon(IconName::RefreshCw)
                        .tooltip("Refresh the live session list")
                        .on_click(move |_, _, _| runtime.send(UiCommand::RefreshSessions)),
                ),
        );

        // Live session rows (discovery), grouped by workspace heading. With a
        // query, only matching sessions render (their headings collapse away).
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut last_workspace: Option<&str> = None;
        for sess in &self.snap.sessions {
            if !query.is_empty() && !session_matches(sess, &query) {
                continue;
            }
            if last_workspace != Some(sess.workspace.as_str()) {
                last_workspace = Some(sess.workspace.as_str());
                rows.push(
                    div()
                        .mt_1()
                        .text_size(px(10.))
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(if sess.workspace.is_empty() {
                            "(no workspace)".to_string()
                        } else {
                            sess.workspace.clone()
                        })
                        .into_any_element(),
                );
            }
            rows.push(self.render_session_row(sess, cx).into_any_element());
        }
        if rows.is_empty() {
            rows.push(
                div()
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child(if query.is_empty() {
                        "No live sessions — start one from the editor, or browse history below."
                            .to_string()
                    } else {
                        "No matching sessions".to_string()
                    })
                    .into_any_element(),
            );
        }
        for row in rows {
            column = column.child(row);
        }

        // The Session History browser.
        column = column.child(self.render_history(cx));

        div()
            .id("sessions-panel")
            .h_full()
            .w_full()
            .min_w_0()
            .flex_none()
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.title_bar)
            // Rows truncate INSIDE: x must clip here or wide titles push the
            // whole panel past the resizable split into the chat area.
            .overflow_y_scroll()
            .overflow_x_hidden()
            .child(column)
            .into_any_element()
    }

    fn render_session_row(&self, sess: &SessionRow, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let runtime = self.runtime.clone();
        let title = sess
            .title
            .clone()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| short_id(&sess.session_id, "session"));
        let active = self.snap.active_tab == sess.open_tab && sess.open_tab.is_some();
        // 6px status dot (absolute — size_1_5() is RELATIVE here, see the
        // search-icon note).
        // The TS ActivityBadge ladder (SessionList.tsx:29-87): awaiting >
        // running > just-finished (60s) > idle; the discovery row's REST
        // `running` status is the heartbeat fallback (ADR-0005) for rows
        // with NO broadcast activity entry. The finished window's lapse
        // needs no timer — discovery's 4s poll repaints the rows anyway.
        let finished_fresh = finished_fresh(sess, now_ms());
        let mut dot = div().size(px(6.)).flex_none().rounded_full();
        dot = match activity_dot(sess) {
            ActivityDot::Awaiting => dot.border_1().border_color(theme.warning_foreground),
            ActivityDot::Running => dot.bg(theme.accent),
            ActivityDot::Idle => dot.bg(theme.muted_foreground.opacity(0.4)),
        };
        let instance_id = sess.instance_id.clone();
        let session_id = sess.session_id.clone();
        // The row's "…" actions (G-light): the TS long-press sheet as a
        // desktop dropdown — Rename and End session (mobile's sheet pair for
        // discovery rows; the tombstone delete lives on history rows).
        let wb = cx.entity();
        let menu_instance = sess.instance_id.clone();
        let menu_session = sess.session_id.clone();
        let menu_prefill = sess.title.clone();
        let menu_runtime = self.runtime.clone();
        let actions = div()
            .id(SharedString::from(format!(
                "session-menu-wrap-{}",
                sess.session_id
            )))
            .flex_none()
            // The trigger's click must not ALSO open the session (the row's
            // own on_click sits above in the bubble path).
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                Button::new(SharedString::from(format!("session-menu-{}", sess.session_id)))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Ellipsis)
                    .tooltip("Session actions")
                    .dropdown_menu(move |menu, _, _| {
                        let mut m = menu;
                        // The builder runs on every open — fresh captures.
                        let (wb, inst, sid, prefill) = (
                            wb.clone(),
                            menu_instance.clone(),
                            menu_session.clone(),
                            menu_prefill.clone(),
                        );
                        m = m.item(
                            PopupMenuItem::new("Rename…").on_click(move |_, _, cx| {
                                wb.update(cx, |wb, cx| {
                                    wb.open_rename(inst.clone(), sid.clone(), prefill.clone(), cx)
                                });
                            }),
                        );
                        let (runtime, inst, sid) = (
                            menu_runtime.clone(),
                            menu_instance.clone(),
                            menu_session.clone(),
                        );
                        m = m.item(
                            PopupMenuItem::new("End session").on_click(move |_, _, _| {
                                runtime.send(UiCommand::CloseSession {
                                    instance_id: inst.clone(),
                                    session_id: sid.clone(),
                                });
                            }),
                        );
                        m
                    }),
            );
        div()
            .id(SharedString::from(format!("session-row-{}", sess.session_id)))
            .test_support()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .rounded_md()
            .when(active, |this| this.bg(theme.secondary))
            .hover(|this| this.bg(theme.secondary.opacity(0.6)))
            .cursor_pointer()
            .child(dot)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(13.))
                    .text_color(theme.foreground)
                    .child(title),
            )
            .when(finished_fresh, |this| {
                // The TS just-finished badge: text only, no dot (the dot
                // above stays idle-colored while this chip shows).
                this.child(
                    div()
                        .id(SharedString::from(format!(
                            "session-finished-{}",
                            sess.session_id
                        )))
                        .test_support()
                        .flex_none()
                        .text_size(px(10.))
                        .text_color(theme.accent)
                        .child(t!("chat.activityFinished").into_owned()),
                )
            })
            .when(sess.open_tab.is_some(), |this| {
                this.child(
                    div()
                        .text_size(px(10.))
                        .text_color(theme.muted_foreground)
                        .child("open"),
                )
            })
            .child(actions)
            .on_click(move |_, _, _| {
                runtime.send(UiCommand::OpenSession {
                    instance_id: instance_id.clone(),
                    session_id: session_id.clone(),
                });
            })
    }

    fn render_history(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let runtime = self.runtime.clone();
        let mut column = div().flex().flex_col().gap_1();

        // Section toggle.
        column = column.child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .mt_2()
                .child(
                    div()
                        .text_size(px(12.))
                        .font_family(theme.mono_font_family.clone())
                        .text_color(theme.muted_foreground)
                        .child(t!("historyDialog.title").to_uppercase()),
                )
                .child(div().flex_1())
                .child(
                    Button::new("history-toggle")
                        .ghost()
                        .xsmall()
                        .icon(if self.snap.history.open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .tooltip("Browse a project's past sessions")
                        .on_click(move |_, _, _| runtime.send(UiCommand::HistoryToggle)),
                ),
        );

        if !self.snap.history.open {
            return column.into_any_element();
        }

        let Some(selected) = self.snap.history.selected.as_ref() else {
            // Project chooser.
            match (&self.snap.history.projects, &self.snap.history.projects_error) {
                (None, None) => {
                    column = column.child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.muted_foreground)
                            .child(t!("historyDialog.loading").into_owned()),
                    );
                }
                (None, Some(err)) => {
                    column = column.child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.danger)
                            .child(err.clone()),
                    );
                }
                (Some(list), _) => {
                    // The project-layer filter (TS ProjectHistoryDialog
                    // :413-421): case-insensitive path match, the same
                    // shape as the chooser's.
                    column = column
                        .child(div().px_1().child(Input::new(&self.history_filter).small()));
                    let q = self.history_filter.read(cx).value().trim().to_lowercase();
                    let shown: Vec<_> = list
                        .iter()
                        .filter(|p| q.is_empty() || p.workspace_path.to_lowercase().contains(&q))
                        .collect();
                    if shown.is_empty() {
                        column = column.child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme.muted_foreground)
                                .child(if list.is_empty() {
                                    "No known projects yet.".to_string()
                                } else {
                                    t!("historyDialog.empty").into_owned()
                                }),
                        );
                    }
                    for p in shown {
                        let runtime = self.runtime.clone();
                        let workspace = p.workspace_path.clone();
                        column = column.child(
                            div()
                            .id(SharedString::from(format!("history-project-{}", workspace)))
                                .flex()
                                .items_center()
                                .gap_2()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .hover(|this| this.bg(theme.secondary.opacity(0.6)))
                                .cursor_pointer()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(px(13.))
                                        .text_color(theme.foreground)
                                        .child(project_name(&p.workspace_path)),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .text_size(px(10.))
                                        .text_color(theme.muted_foreground)
                                        // TS ProjectHistoryDialog:529 — count
                                        // plus fmtRelative(lastActive).
                                        .child(project_meta_line(p.sessions, p.last_active)),
                                )
                                .on_click(move |_, _, _| {
                                    runtime.send(UiCommand::HistoryOpenProject {
                                        workspace_path: workspace.clone(),
                                    });
                                }),
                        );
                    }
                }
            }
            return column.into_any_element();
        };

        // The selected project's paged rows.
        column = self.render_history_list(selected, cx, column);
        column.into_any_element()
    }

    fn render_history_list(
        &self,
        selected: &HistorySelected,
        cx: &Context<Self>,
        mut column: gpui::Div,
    ) -> gpui::Div {
        let theme = cx.theme();
        let runtime2 = self.runtime.clone();
        column = column.child(
            div().flex().items_center().gap_1().child(
                Button::new("history-back")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ArrowLeft)
                    .tooltip("All projects")
                    .on_click(move |_, _, _| runtime2.send(UiCommand::HistoryBack)),
            ),
        );
        column = column.child(
            div()
                .text_size(px(10.))
                .text_color(theme.muted_foreground)
                .truncate()
                .child(selected.workspace_path.clone()),
        );

        if let Some(err) = &selected.error {
            column = column.child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.danger)
                    .child(err.clone()),
            );
        }
        if selected.loading_page {
            column = column.child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child(t!("historyDialog.waking").into_owned()),
            );
        }
        for row in &selected.rows {
            column = column.child(self.render_history_row(&selected.workspace_path, row, cx));
        }
        // Load more passes the server's composite cursor back VERBATIM;
        // `nextCursor: null` is the end of history.
        if selected.next_cursor.is_some() {
            let runtime3 = self.runtime.clone();
            column = column.child(
                Button::new("history-more")
                    .secondary()
                    .xsmall()
                    .w_full()
                    .label(if selected.loading_more {
                        t!("historyDialog.loadingMore").into_owned()
                    } else {
                        t!("historyDialog.loadMore").into_owned()
                    })
                    .disabled(selected.loading_more)
                    .on_click(move |_, _, _| runtime3.send(UiCommand::HistoryLoadMore)),
            );
        } else if !selected.rows.is_empty() && !selected.loading_page {
            column = column.child(
                div()
                    .text_size(px(10.))
                    .text_color(theme.muted_foreground)
                    .child(t!("historyDialog.end").into_owned()),
            );
        }
        column
    }

    fn render_history_row(
        &self,
        workspace_path: &str,
        row: &HistoryRow,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let runtime = self.runtime.clone();
        let title = row
            .title
            .clone()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| short_id(&row.session_id, "session"));
        let selected = self.snap.history.selected.as_ref();
        let resuming = selected
            .and_then(|s| s.resuming.as_ref())
            .map(|r| r == &row.session_id)
            .unwrap_or(false);
        let deleting = selected
            .and_then(|s| s.deleting.as_ref())
            .map(|r| r == &row.session_id)
            .unwrap_or(false);
        let workspace = workspace_path.to_string();
        let session_id = row.session_id.clone();
        // The row's "…" action (G3): the tombstone delete — the history
        // surface's own row action on mobile too (readOnly + onDelete).
        let menu_runtime = self.runtime.clone();
        let menu_session = row.session_id.clone();
        let actions = div()
            .id(SharedString::from(format!(
                "history-menu-wrap-{}",
                row.session_id
            )))
            .flex_none()
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                Button::new(SharedString::from(format!("history-menu-{}", row.session_id)))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Ellipsis)
                    .tooltip("Session actions")
                    .dropdown_menu(move |menu, _, _| {
                        let mut m = menu;
                        let (runtime, sid) = (menu_runtime.clone(), menu_session.clone());
                        m = m.item(
                            PopupMenuItem::new("Delete from history").on_click(move |_, _, _| {
                                runtime.send(UiCommand::DeleteHistorySession {
                                    session_id: sid.clone(),
                                });
                            }),
                        );
                        m
                    }),
            );
        div()
            .id(SharedString::from(format!("history-row-{}", row.session_id)))
            .test_support()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .rounded_md()
            .hover(|this| this.bg(theme.secondary.opacity(0.6)))
            .cursor_pointer()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(13.))
                    .text_color(theme.foreground)
                    .child(title),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .font_family(theme.mono_font_family.clone())
                    .text_color(if row.live {
                        theme.accent
                    } else {
                        theme.muted_foreground
                    })
                    .child(if resuming {
                        "resuming…".to_string()
                    } else if deleting {
                        "deleting…".to_string()
                    } else if row.live {
                        "live".to_string()
                    } else {
                        // TS SessionList:185 — fmtRelative(updatedAt); the
                        // history rows carry ISO stamps, so parse first and
                        // fall back to the raw text when the shape is odd.
                        row.updated_at
                            .as_deref()
                            .map(|iso| {
                                parse_iso_ms(iso)
                                    .map(|ms| fmt_relative(ms, now_ms()))
                                    .unwrap_or_else(|| iso.to_string())
                            })
                            .unwrap_or_default()
                    }),
            )
            .child(actions)
            .on_click(move |_, _, _| {
                runtime.send(UiCommand::OpenHistorySession {
                    workspace_path: workspace.clone(),
                    session_id: session_id.clone(),
                });
            })
            .into_any_element()
    }

    // -- tabs --------------------------------------------------------------

    /// One tab per attached session (one WS connection each, requirements
    /// §4); the `+` boots a fresh `session/new` tab.
    fn render_tab_strip(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let mut strip = div()
            .flex()
            .items_end()
            .gap_1()
            .h_8()
            .px_2()
            .flex_none()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.title_bar);

        for tab in &self.snap.tabs {
            strip = strip.child(self.render_tab(tab, cx));
        }
        if self.snap.tabs.is_empty() {
            strip = strip.child(
                div()
                    .px_3()
                    .pt_1()
                    .pb_1()
                    .text_size(px(13.))
                    .text_color(theme.muted_foreground)
                    .child("no session open"),
            );
        }

        let runtime = self.runtime.clone();
        strip = strip.child(
            Button::new("new-tab")
                .ghost()
                .xsmall()
                .icon(IconName::Plus)
                .tooltip("New session — pick the project directory it belongs to")
                .on_click(move |_, _, _| runtime.send(UiCommand::NewSessionToggle)),
        );

        // The tab strip stays global (sessions only): the model/mode/thought
        // chips moved down to the composer (batch S — the TUI keeps its
        // switchers at the input). The manage-workflows entry (batch M, the
        // TS SessionPanel's single workflow row) rides the trailing group:
        // shown when the gate verdict is ON and a session is attached; opens
        // the Workflows page.
        let session_live = self
            .snap
            .active
            .as_ref()
            .and_then(|t| t.session_id.clone())
            .is_some();
        let manage_row = (session_live
            && self.snap.workflow_gate.as_ref().is_some_and(|g| g.enabled))
            .then(|| {
                let runtime = self.runtime.clone();
                Button::new("session-manage-workflows")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(KitIconName::Workflow))
                    .tooltip(t!("panel.workflowManage").into_owned())
                    .on_click(move |_, _, _| runtime.send(UiCommand::OpenWorkflows))
                    .into_any_element()
            });
        // The right session panel's toggle (TS ChatScreen's header button):
        // always present — the panel also carries the hub-level quota card,
        // which needs no session.
        let wb_toggle = cx.entity();
        let toggle = Button::new("session-panel-toggle")
            .ghost()
            .xsmall()
            .icon(KitIconName::PanelRight)
            .selected(self.session_panel_open)
            .tooltip(t!("panel.session").into_owned())
            .on_click(move |_, _, cx| {
                wb_toggle.update(cx, |this, cx| this.toggle_session_panel(cx));
            });
        strip = strip.child(
            div()
                .ml_auto()
                .flex()
                .items_center()
                .gap_1()
                .min_w_0()
                .pr_1()
                .children(manage_row)
                .child(toggle),
        );
        strip.into_any_element()
    }

    /// One model/mode/thought chip (the TS ConfigSheet as a composer-side
    /// dropdown, batch S): the current value's display name labels the
    /// button, the menu lists every choice with a check on the active row;
    /// picking a non-active row dispatches `session/set_config_option` and
    /// the menu dismisses itself (the popover follows its DismissEvent).
    fn render_config_chip(
        &self,
        opt: &protocol::acp::ConfigOption,
        _cx: &Context<Self>,
    ) -> AnyElement {
        let runtime = self.runtime.clone();
        let title = opt.name.clone().unwrap_or_else(|| opt.id.clone());
        let config_id = opt.id.clone();
        let (current, choices) = config_option_view(opt);
        Button::new(SharedString::from(format!("cfg-{}", opt.id)))
            .ghost()
            .xsmall()
            .label(current)
            .tooltip(format!("{title}: pick the session's {title}"))
            .dropdown_menu(move |menu, _, _| {
                config_option_menu(menu, &runtime, &config_id, &choices)
            })
            .into_any_element()
    }

    /// The plan/todo strip (E3 — ChatScreen.tsx `PlanPanel`): a collapsed
    /// `Plan · done/total` summary above the chat that expands to the entry
    /// list — ✓ completed (struck out), › active, ○ pending.
    fn render_plan_bar(
        &self,
        entries: &[protocol::replay::PlanEntry],
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let wb = cx.entity();
        let open = self.plan_open;
        let done = entries
            .iter()
            .filter(|e| e.status.as_deref() == Some("completed"))
            .count();
        let mut bar = div()
            .flex()
            .flex_col()
            .flex_none()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.title_bar);
        bar = bar.child(
            div()
                .id("plan-toggle")
                .flex()
                .items_center()
                .gap_1()
                .px_3()
                .py(px(6.))
                .cursor_pointer()
                .hover(|h| h.bg(theme.secondary))
                .on_click(move |_, _, cx| {
                    wb.update(cx, |wb, cx| {
                        wb.plan_open = !wb.plan_open;
                        cx.notify();
                    });
                })
                .child(
                    Icon::new(IconName::ChevronDown)
                        .small()
                        .flex_none()
                        .when(!open, |icon| {
                            icon.rotate(radians(-std::f32::consts::FRAC_PI_2))
                        })
                        .text_color(theme.muted_foreground),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.))
                        .text_color(theme.muted_foreground)
                        .child(format!("{} · {}/{}", t!("chat.plan"), done, entries.len())),
                )
                .test_support(),
        );
        if open {
            bar = bar.child(
                div()
                    .id("plan-entries")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_3()
                    .pb_2()
                    .max_h(px(240.))
                    .overflow_y_scroll()
                    .children(entries.iter().enumerate().map(|(i, e)| {
                        let status = e.status.as_deref();
                        let (glyph, color) = match status {
                            Some("completed") => ("✓", theme.accent),
                            Some("active") => ("›", theme.foreground),
                            _ => ("○", theme.muted_foreground),
                        };
                        div()
                            .id(SharedString::from(format!("plan-entry-{i}")))
                            .flex()
                            .items_start()
                            .gap_2()
                            .min_w_0()
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(12.))
                                    .text_color(color)
                                    .child(glyph),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .text_size(px(13.))
                                    .when(status == Some("completed"), |this| {
                                        this.line_through()
                                            .text_color(theme.muted_foreground)
                                    })
                                    .when(status != Some("completed"), |this| {
                                        this.text_color(theme.muted_foreground)
                                    })
                                    .child(e.content.clone()),
                            )
                            .test_support()
                    })),
            );
        }
        bar.into_any_element()
    }

    fn render_tab(&self, tab: &TabSummary, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let active = self.snap.active_tab == Some(tab.id);
        // The strip shows the session's NAME (discovery's title — the rename
        // target); the short id is only the fallback before discovery names
        // it, never the steady state.
        let title = match (&tab.session_id, &tab.phase) {
            (Some(_), _) => tab
                .session_title
                .clone()
                .unwrap_or_else(|| short_id(tab.session_id.as_deref().unwrap_or("session"), "session")),
            (None, TabPhase::Failed(_)) => "failed".to_string(),
            (None, _) => "new session".to_string(),
        };
        // Owned copies for the 'static click closures — `tab` borrows the
        // snapshot and must not escape into them.
        let tab_id = tab.id;
        let phase_label = match &tab.phase {
            TabPhase::Connecting | TabPhase::Reconnecting => Some(tab.phase.label()),
            _ => None,
        };

        let runtime = self.runtime.clone();
        let mut tab_view = div()
            .id(("tab", tab.id))
            .flex()
            .items_center()
            .gap_2()
            .min_w_0()
            .px_3()
            .pt_1()
            .pb_1()
            .rounded_t_md()
            .text_size(px(13.))
            .font_family(theme.mono_font_family.clone())
            .cursor_pointer()
            .when(active, |this| {
                this.bg(theme.secondary).text_color(theme.foreground)
            })
            .when(!active, |this| {
                this.text_color(theme.muted_foreground)
                    .hover(|h| h.bg(theme.secondary.opacity(0.5)))
            })
            .on_click(move |_, _, _| runtime.send(UiCommand::SelectTab { id: tab_id }));

        // Session Activity badges: running dot / awaiting ring / pending
        // count; a non-connected phase shows its state instead.
        match phase_label {
            Some(label) => {
                tab_view = tab_view.child(
                    div()
                        .text_size(px(10.))
                        .text_color(theme.muted_foreground)
                        .child(label),
                );
            }
            None if matches!(tab.phase, TabPhase::Failed(_)) => {
                tab_view = tab_view.child(
                    div()
                        .text_size(px(10.))
                        .text_color(theme.danger)
                        .child("error"),
                );
            }
            None if tab.running => {
                tab_view = tab_view.child(
                    div().size(px(6.)).flex_none().rounded_full().bg(theme.accent),
                );
            }
            None if tab.awaiting => {
                tab_view = tab_view.child(
                    div()
                        .size(px(6.))
                        .flex_none()
                        .rounded_full()
                        .border_1()
                        .border_color(theme.warning_foreground),
                );
            }
            None => {}
        }
        tab_view = tab_view.child(
            div()
                .id(("tab-title", tab.id))
                .test_support()
                .min_w_0()
                // A long session name must not stretch its tab unboundedly:
                // the cap bounds the title so the ellipsis engages (and
                // crowded strips share the slack — the tab itself min_w_0s).
                .max_w(px(160.))
                .truncate()
                .child(title),
        );
        if tab.pending > 0 {
            tab_view = tab_view.child(
                div()
                    .text_size(px(10.))
                    .text_color(theme.muted_foreground)
                    .child(format!("+{}", tab.pending)),
            );
        }

        // The tab's close control (closing a tab only drops that tab's
        // connection — the session keeps running on the bridge).
        let runtime2 = self.runtime.clone();
        let close_id = tab.id;
        tab_view = tab_view.child(
            Button::new(("tab-close", tab.id))
                .ghost()
                .xsmall()
                .icon(IconName::CircleX)
                .tooltip("Close tab — the session keeps running on the hub")
                .on_click(move |_, _, _| runtime2.send(UiCommand::CloseTab { id: close_id })),
        );
        tab_view.into_any_element()
    }

    fn render_chat(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let mut column = div().flex().flex_col().flex_1().min_h_0();

        // The load-earlier control (ChatView.tsx:1067): remaining count when
        // the bridge reported totalMessages, generic otherwise. The page
        // PREPENDS with the stored cursor passed back verbatim.
        if self.tab.as_ref().is_some_and(|t| t.has_more) {
            let runtime = self.runtime.clone();
            let loading = self.tab.as_ref().is_some_and(|t| t.loading_earlier);
            let remaining = self.tab.as_ref().and_then(|t| {
                t.total_messages
                    .map(|total| total.saturating_sub(self.rows.len() as u64))
            });
            let label = if loading {
                t!("chat.loadingEarlier").into_owned()
            } else {
                match remaining {
                    // The catalog carries the plural form only (the TS page
                    // uses the same key for every count).
                    Some(n) if n > 0 => fill(
                        &t!("chat.loadEarlier"),
                        &[("n", &n.to_string())],
                    ),
                    _ => t!("chat.loadEarlierGeneric").into_owned(),
                }
            };
            column = column.child(
                div()
                    .flex()
                    .justify_center()
                    .flex_none()
                    .py_2()
                    .child(
                        Button::new("load-earlier")
                            .secondary()
                            .small()
                            .label(label)
                            .disabled(loading)
                            .tooltip("session/load_earlier — the stored cursor rides back verbatim")
                            .on_click(move |_, _, _| runtime.send(UiCommand::LoadEarlier)),
                    ),
            );
        }

        if self.rows.is_empty() && self.card.is_none() {
            let message = if self.snap.tabs.is_empty() {
                "No session open — pick one from the sidebar or start a new tab.".to_string()
            } else if self.tab.as_ref().is_some_and(|t| t.loading_session) {
                t!("chat.loadingSession").into_owned()
            } else if matches!(
                self.tab.as_ref().map(|t| &t.phase),
                Some(TabPhase::Reconnecting)
            ) {
                t!("chat.reconnecting").into_owned()
            } else if matches!(
                self.tab.as_ref().map(|t| &t.phase),
                Some(TabPhase::Failed(_))
            ) {
                "This tab failed — close it and open the session again.".to_string()
            } else if self
                .tab
                .as_ref()
                .and_then(|t| t.session_id.as_deref())
                .is_some()
            {
                "Empty session — say something below.".to_string()
            } else {
                "Waiting for a session to attach…".to_string()
            };
            column = column.child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .id("chat-empty-state")
                            .test_support()
                            .text_color(theme.muted_foreground)
                            .text_size(px(13.))
                            .child(message),
                    ),
            );
            return column.into_any_element();
        }

        let rows = self.rows.clone();
        let card = self.card.clone();
        let runtime = self.runtime.clone();
        let weak = cx.weak_entity();
        let running = self.tab.as_ref().is_some_and(|t| t.running);
        let copied = self.copied_row.clone();
        let weak_rows = weak.clone();
        let scroller = self.scroller.clone();
        let fade = theme.background;
        // The (user, agent) chat text sizes off the font ladder — the rows
        // read them at paint time so the segment's pick rescales everything.
        let chat_font = self.font_scale.chat_px();
        let thought_full = self.thought_full.clone();
        column
            .child(
                MessageScroller::new("chat", scroller, move |index, _window, cx| {
                    let is_last_row = index + 1 == rows.len();
                    if let Some(row) = rows.get(index) {
                        render_row(
                            row,
                            is_last_row,
                            running,
                            copied.as_deref(),
                            &weak_rows,
                            chat_font,
                            &thought_full,
                            cx,
                        )
                    } else if let Some(card) = card.as_ref() {
                        // The trailing approval card row (inline in the stream —
                        // requirements §2).
                        render_approval_card(card, &runtime, &weak, cx)
                    } else {
                        div().into_any_element()
                    }
                })
                .with_bottom_fade(fade)
                .with_list_style(gpui::StyleRefinement::default().px(px(12.)))
                .flex_1()
                .min_h_0(),
            )
            .into_any_element()
    }

    fn render_composer(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        // flex_none: the message list shrinks, never the composer — the
        // input stays visible whatever the stream above is doing.
        let mut column = div().flex().flex_col().gap_1().flex_none();
        // The pending queue, one row per draft (ChatView's pending bubbles):
        // semi-transparent, text + image count, with the three affordances —
        // Send now (hidden while replaying/disconnected), Edit (back into
        // the composer), and discard.
        if let Some(t) = self.tab.as_ref() {
            // TS ChatView:932 — Send now needs connState "open" AND the
            // replay finished; the phase flips Connected before load_session
            // runs, so loading_session alone would leave a dead button up
            // through the whole replay (force_send refuses while loading).
            let connected =
                matches!(t.phase, TabPhase::Connected) && !t.loading_session;
            for (index, draft) in t.pending.iter().enumerate() {
                let runtime = self.runtime.clone();
                let mut row = div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(theme.secondary)
                    .opacity(0.85);
                row = row.child(
                    div()
                        .flex_none()
                        .text_size(px(9.))
                        .text_color(theme.muted_foreground)
                        .child(t!("chat.pending").into_owned()),
                );
                row = row.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(13.))
                        .text_color(theme.foreground)
                        .child(if draft.text.is_empty() {
                            "(images only)".to_string()
                        } else {
                            draft.text.clone()
                        }),
                );
                if draft.images > 0 {
                    row = row.child(
                        div()
                            .flex_none()
                            .text_size(px(10.))
                            .text_color(theme.muted_foreground)
                            .child(format!("{} img", draft.images)),
                    );
                }
                if connected {
                    row = row.child(
                        Button::new(("pending-send", index))
                            .ghost()
                            .xsmall()
                            .label(t!("chat.sendNow").into_owned())
                            .tooltip("Interrupt the running turn and send this one first")
                            .on_click(move |_, _, _| {
                                runtime.send(UiCommand::ForceSendPendingAt { index })
                            }),
                    );
                }
                let runtime = self.runtime.clone();
                row = row.child(
                    Button::new(("pending-edit", index))
                        .ghost()
                        .xsmall()
                        .label(t!("chat.editPending").into_owned())
                        .tooltip("Back into the composer for changes")
                        .on_click(move |_, _, _| runtime.send(UiCommand::EditPending { index })),
                );
                let runtime = self.runtime.clone();
                row = row.child(
                    Button::new(("pending-discard", index))
                        .ghost()
                        .xsmall()
                        .icon(IconName::CircleX)
                        .tooltip("Discard")
                        .on_click(move |_, _, _| {
                            runtime.send(UiCommand::DiscardPending { index })
                        }),
                );
                column = column.child(row);
            }
            if !t.pending.is_empty() {
                column = column.child(
                    div()
                        .text_size(px(10.))
                        .text_color(theme.muted_foreground)
                        .child("queued — the next one sends when the running turn settles"),
                );
            }
        }
        if let Some(notice) = &self.notice {
            column = column.child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.danger)
                    .child(notice.clone()),
            );
        }
        // Staged thumbnails (ChatView's strip): raster + remove button.
        if !self.staged.is_empty() {
            let wb = cx.entity();
            let mut strip = div().flex().items_center().gap_2();
            for (index, staged) in self.staged.iter().enumerate() {
                let wb = wb.clone();
                let thumb = match &staged.render {
                    Some(src) => img(src.clone())
                        .size_16()
                        .flex_none()
                        .rounded_md()
                        .border_1()
                        .border_color(theme.border)
                        .object_fit(gpui::ObjectFit::Cover)
                        .into_any_element(),
                    None => div()
                        .size_16()
                        .flex_none()
                        .rounded_md()
                        .border_1()
                        .border_color(theme.border)
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(9.))
                        .text_color(theme.muted_foreground)
                        .child("svg")
                        .into_any_element(),
                };
                strip = strip.child(
                    div()
                        .relative()
                        .flex_none()
                        .child(thumb)
                        .child(
                            div().absolute().top_0().right_0().child(
                                Button::new(("remove-attachment", index))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::CircleX)
                                    .tooltip("Remove attachment")
                                    .on_click(move |_, _, cx| {
                                        wb.update(cx, |this, cx| this.remove_staged(index, cx));
                                    }),
                            ),
                        ),
                );
            }
            let count = self.staged.len();
            strip = strip.child(
                div()
                    .text_size(px(10.))
                    .text_color(theme.muted_foreground)
                    .child(format!("{count}/{MAX_IMAGES}")),
            );
            column = column.child(strip);
        }
        // The "/" completion menu (K1): mounted through the deferred layer
        // anchored to the textarea's top-left, growing upward (mobile's
        // above-composer listbox). Input-bounds lag one frame — acceptable:
        // the composer does not move while typing.
        if let Some(menu) = self.slash_menu(cx).filter(|m| m.open) {
            let anchor = self.composer.read(cx).input_bounds().origin;
            column = column.child(
                deferred(
                    anchored()
                        .anchor(Anchor::BottomLeft)
                        .position(anchor)
                        .child(render_slash_menu(&menu, &cx.entity().downgrade(), &theme)),
                )
                .with_priority(1),
            );
        }
        let mut controls = div().flex().items_end().gap_2();
        // The gallery pick (ChatView's ImagePlus): native file dialog.
        let wb = cx.entity();
        controls = controls.child(
            Button::new("attach")
                .ghost()
                .small()
                .icon(KitIconName::ImagePlus)
                .tooltip("Attach images — paste or drop them here too")
                .on_click({
                    let wb = wb.clone();
                    move |_, _window, cx| {
                        // Fn closures may fire repeatedly; hand the async
                        // task its own clone.
                        let wb = wb.clone();
                        let rx = cx.prompt_for_paths(PathPromptOptions {
                            files: true,
                            directories: false,
                            multiple: true,
                            prompt: Some("Choose images".into()),
                        });
                        cx.spawn(async move |cx| {
                            let Ok(Ok(Some(paths))) = rx.await else {
                                return;
                            };
                            let load = cx
                                .background_executor()
                                .spawn(async move { read_image_files(paths) });
                            let inputs = load.await;
                            let _ =
                                cx.update_entity(&wb, |wb, cx| wb.stage_inputs(inputs, cx));
                        })
                        .detach();
                    }
                }),
        );
        // Auto-grow (TS ChatView:830-846 — rows=1, min 36px, max 128px):
        // the height tracks the draft's logical lines; wrapped-long-line
        // overflow keeps scrolling internally inside the 128px cap.
        let composer_lines = self.composer.read(cx).value().lines().count().max(1);
        let composer_h = px((36. + (composer_lines.saturating_sub(1) as f32) * 18.).min(128.));
        controls = controls.child(
            Textarea::new(&self.composer)
                .h(composer_h)
                .flex_1()
                // Desktop paste: clipboard rasters and copied files ride
                // the same pipeline as picks (ChatView onPaste).
                .on_paste({
                    let wb = cx.entity();
                    move |item: &ClipboardItem, _window, cx| {
                        paste_inputs(item).map(|inputs| {
                            wb.update(cx, |wb, cx| wb.stage_inputs(inputs, cx));
                        })
                        .is_some()
                    }
                }),
        );
        if self.tab.as_ref().is_some_and(|t| t.running) {
            let runtime = self.runtime.clone();
            controls = controls.child(
                Button::new("stop")
                    .secondary()
                    .small()
                    .icon(IconName::Square)
                    .tooltip("Cancel the running turn")
                    .on_click(move |_, _, _| runtime.send(UiCommand::CancelTurn)),
            );
        }
        let runtime = self.runtime.clone();
        let composer = self.composer.clone();
        let wb = cx.entity();
        let can_send = !self.staged.is_empty()
            || !self.composer.read(cx).value().trim().is_empty();
        controls = controls.child(
            Button::new("send")
                .primary()
                .small()
                .label(t!("chat.send").into_owned())
                .disabled(!can_send)
                .tooltip("Enter — queues while a turn is running")
                .on_click(move |_, window, cx| {
                    let text = composer.read(cx).value().to_string();
                    let Some((text, images)) = wb.read(cx).outgoing_prompt(&text) else {
                        return;
                    };
                    composer.update(cx, |state, cx| state.set_value("", window, cx));
                    wb.update(cx, |this, cx| {
                        this.staged.clear();
                        this.notice = None;
                        cx.notify();
                    });
                    runtime.send(UiCommand::SendPrompt { text, images });
                }),
        );
        column = column.child(controls);
        // The composer's footer line (batch T): the session's readouts on the
        // left (E1 — ChatScreen's header line `context 17.5%/1.0M` plus the
        // absolute pair, bare cache percent, `tok/s`, and the last turn's
        // tokens · wall time), and the config chips tucked into the SAME
        // row's right end — one line for both beats a dedicated chips row
        // (the TUI keeps its switchers at the input; a separate strip above
        // the textarea cost vertical space for a glance-level affordance).
        // The full-width pickers stay in the session panel.
        {
            let tab = self.tab.as_ref();
            let usage = tab.and_then(|t| t.usage).filter(|u| u.size > 0);
            let has_stats = usage.is_some()
                || tab.is_some_and(|t| t.cache_hit.is_some() || t.turn_stats.is_some());
            let chip_opts: &[protocol::acp::ConfigOption] = tab
                .filter(|t| matches!(t.phase, TabPhase::Connected))
                .map(|t| t.config_options.as_slice())
                .filter(|opts| !opts.is_empty())
                .unwrap_or(&[]);
            if has_stats || !chip_opts.is_empty() {
                // Left group first: the readouts clip (never wrap — the
                // whole point of the shared line is saving the vertical
                // space) when a narrow composer cannot hold everything.
                let mut stats = div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .min_w_0()
                    .overflow_hidden()
                    .text_size(px(12.))
                    .font_family(theme.mono_font_family.clone())
                    .text_color(theme.muted_foreground);
                if let Some(usage) = usage {
                    stats = stats.child(
                        div()
                            .id("stat-context")
                            .flex_none()
                            // The enriched variant (TUI's `fmtContext` head
                            // plus the absolute pair) — `fmt_context` itself
                            // already embeds the denominator, so composing
                            // would print the window size twice.
                            .child(format!(
                                "ctx {:.1}% · {}/{}",
                                context_pct(usage.used, usage.size),
                                fmt_tokens(usage.used),
                                fmt_tokens(usage.size)
                            ))
                            .test_support(),
                    );
                }
                if let Some(hit) = tab.and_then(|t| t.cache_hit) {
                    stats = stats.child(
                        div()
                            .id("stat-cache")
                            .flex_none()
                            .child(format!("cache {hit}%"))
                            .test_support(),
                    );
                }
                if let Some(turn) = tab.and_then(|t| t.turn_stats) {
                    stats = stats.child(
                        div()
                            .id("stat-tps")
                            .flex_none()
                            .child(format!("{} tok/s", turn.tps))
                            .test_support(),
                    );
                    stats = stats.child(
                        div()
                            .id("stat-turn")
                            .flex_none()
                            .child(format!(
                                "{} tok · {:.1}s",
                                fmt_tokens(turn.output_tokens),
                                turn.seconds
                            ))
                            .test_support(),
                    );
                }
                let row = div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .min_w_0()
                    .child(stats)
                    .child(
                        div()
                            .id("composer-chips")
                            .test_support()
                            .ml_auto()
                            .flex()
                            .items_center()
                            .gap_2()
                            .children(
                                chip_opts
                                    .iter()
                                    .map(|opt| self.render_config_chip(opt, cx))
                                    .collect::<Vec<_>>(),
                            ),
                    );
                column = column.child(row);
            }
        }

        let mut composer = div()
            .id("composer")
            .test_support()
            .flex_none()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.title_bar)
            .p_3()
            .child(column);
        // Context progress hairline (ChatScreen's header-bottom gauge,
        // riding the composer's top edge now): clamped at 100%, inside the
        // composer's own bounds so no sibling can overpaint it.
        if let Some(usage) = self
            .tab
            .as_ref()
            .and_then(|t| t.usage)
            .filter(|u| u.size > 0)
        {
            let pct = (((usage.used as f32 / usage.size as f32) * 100.0).min(100.0) / 100.0) as f32;
            composer = composer
                .relative()
                .child(
                    div()
                        .id("context-progress")
                        .absolute()
                        .top_0()
                        .left_0()
                        .h(px(2.))
                        .w(relative(pct))
                        .bg(theme.accent)
                        .test_support(),
                );
        }
        composer.into_any_element()
    }
}

fn new_row(msg: &ChatMessage, cx: &mut Context<Workbench>) -> RowModel {
    let mut parts = Vec::with_capacity(msg.parts.len());
    for part in &msg.parts {
        parts.push(new_part_view(part, msg.role, cx));
    }
    RowModel {
        id: msg.id.clone(),
        role: msg.role,
        collapsed: msg.collapsed.unwrap_or(false),
        parts,
    }
}

// ---------------------------------------------------------------------------
// Attachment helpers
// ---------------------------------------------------------------------------

/// Decodes a draft into the raster gpui renders (thumbnails, echo parts).
/// SVG keeps no raster — the pipeline passes those through untouched and
/// gpui cannot rasterize vector bytes, so callers fall back to a chip.
fn render_image_of(draft: &AttachmentDraft) -> Option<Arc<gpui::Image>> {
    let format = ImageFormat::from_mime_type(&draft.mime_type)?;
    if format == ImageFormat::Svg {
        return None;
    }
    let bytes = decode_attachment_bytes(draft)?;
    Some(Arc::new(gpui::Image::from_bytes(format, bytes)))
}

// -- "/" completion pure units (batch K, ChatView.tsx:576-635) --------------

/// The whitespace-delimited word around the caret (byte range), ChatView's
/// token scan: scan back/forward while the neighboring char is not
/// whitespace. A caret resting exactly on a following word's start edge
/// counts as no word (`tokenStart === caret && tokenEnd > caret` → ""). Only
/// words starting with "/" complete — the caller decides.
fn slash_token_range(value: &str, caret: usize) -> Option<(usize, usize)> {
    let bytes = value.as_bytes();
    let caret = caret.min(bytes.len());
    // Snap the caret onto a char boundary (defensive; cursor() is a byte
    // offset but a malformed selection could park mid-char).
    let mut caret = caret;
    while caret > 0 && !value.is_char_boundary(caret) {
        caret -= 1;
    }
    let is_ws =
        |b: u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c');
    let mut start = caret;
    while start > 0 && !is_ws(bytes[start - 1]) {
        start -= 1;
        while start > 0 && !value.is_char_boundary(start) {
            start -= 1;
        }
    }
    let mut end = caret;
    while end < bytes.len() && !is_ws(bytes[end]) {
        end += 1;
        while end < bytes.len() && !value.is_char_boundary(end) {
            end += 1;
        }
    }
    if start == caret && end > caret {
        return None; // the caret sits on the NEXT word's start edge
    }
    if value[start..].starts_with('/') {
        Some((start, end))
    } else {
        None
    }
}

/// Bridge-ordered prefix matches (TS `matches`): the query is the token
/// after "/", lowercased; skill commands ride a "$" grouping prefix that is
/// stripped on BOTH sides so "/td" matches "$tdd" and "/$tdd" does too.
fn slash_matches(token: &str, commands: &[protocol::acp::SlashCommand]) -> Vec<protocol::acp::SlashCommand> {
    let q = strip_dollar(&token[1..]).to_lowercase();
    commands
        .iter()
        .filter(|c| strip_dollar(&c.name.to_lowercase()).starts_with(&q))
        .cloned()
        .collect()
}

/// Skill commands ride a "$" grouping prefix ("/$tdd" on the wire); the
/// completion query strips it on BOTH sides so "/td" matches "$tdd".
fn strip_dollar(s: &str) -> &str {
    s.strip_prefix('$').unwrap_or(s)
}

/// lsp-style (line, character) of a byte offset — parks the caret after a
/// committed command (the TS `setSelectionRange` park; `set_value` resets a
/// multi-line selection to 0..0, so the park must follow it).
fn caret_position_of(text: &str, byte_offset: usize) -> (u32, u32) {
    let mut line = 0u32;
    let mut character = 0u32;
    for (i, ch) in text.char_indices() {
        if i >= byte_offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            character = 0;
        } else {
            character += 1;
        }
    }
    (line, character)
}

/// Reads dropped/picked files off the main thread, keeping only images
/// (TS `!f.type.startsWith("image/") continue`) — the mime comes from the
/// bytes when the extension says nothing.
fn read_image_files(paths: Vec<std::path::PathBuf>) -> Vec<StageInput> {
    paths
        .into_iter()
        .filter_map(|path| {
            let bytes = std::fs::read(&path).ok()?;
            let mime = sniff_mime(&bytes)?.to_string();
            Some(StageInput { bytes, mime })
        })
        .collect()
}

/// Extracts paste-able image payloads from a clipboard item: rasters carry
/// their format, copied files go through the same read-and-sniff as drops.
fn paste_inputs(item: &ClipboardItem) -> Option<Vec<StageInput>> {
    let mut inputs = Vec::new();
    let mut paths = Vec::new();
    for entry in &item.entries {
        match entry {
            ClipboardEntry::Image(image) => inputs.push(StageInput {
                bytes: image.bytes.clone(),
                mime: image.format.mime_type().to_string(),
            }),
            ClipboardEntry::ExternalPaths(files) => paths.extend(files.paths().iter().cloned()),
            ClipboardEntry::String(_) => {}
        }
    }
    if inputs.is_empty() && paths.is_empty() {
        return None; // plain text paste falls through to the textarea
    }
    inputs.extend(read_image_files(paths));
    if inputs.is_empty() {
        None
    } else {
        Some(inputs)
    }
}

/// One attachment echo in the chat stream: bounded raster, ring, contain.
/// Clicking opens the lightbox (K2) — the row hands its raster over.
fn render_attachment_echo(
    src: &Arc<gpui::Image>,
    theme: &component::Theme,
    wb: &WeakEntity<Workbench>,
    id: SharedString,
) -> AnyElement {
    let open_src = src.clone();
    let wb = wb.clone();
    div()
        .p_1()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .id(id)
        .test_support()
        .on_click(move |_, _, cx| {
            let _ = wb.update(cx, |this, cx| this.open_lightbox(open_src.clone(), None, cx));
        })
        .child(
            img(src.clone())
                .w(px(240.))
                .h(px(240.))
                .rounded_sm()
                .object_fit(gpui::ObjectFit::Contain),
        )
        .into_any_element()
}

/// The "/" completion menu surface (K2, ChatView's listbox): one row per
/// bridge-ordered match — mono "/name" plus the truncated description (and
/// the input hint, TS `description — hint`). Mounted by the composer through
/// `deferred(anchored(...))`, so it floats above the workbench.
fn render_slash_menu(
    menu: &SlashMenuState,
    wb: &WeakEntity<Workbench>,
    theme: &component::Theme,
) -> AnyElement {
    let mut list = div()
        .id("slash-menu")
        .test_support()
        .flex()
        .flex_col()
        .w(px(420.))
        .max_h(px(224.))
        .overflow_y_scroll()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.title_bar);
    for (ix, cmd) in menu.matches.iter().enumerate() {
        let selected = ix == menu.highlight;
        let name = cmd.name.clone();
        let mut description = cmd.description.clone().unwrap_or_default();
        if let Some(hint) = cmd.input.as_ref().and_then(|i| i.hint.as_deref()) {
            if !hint.is_empty() {
                if description.is_empty() {
                    description = hint.to_string();
                } else {
                    description = format!("{description} — {hint}");
                }
            }
        }
        let row = div()
            .id(SharedString::from(format!("slash-row-{ix}")))
            .test_support()
            .flex()
            .items_baseline()
            .gap_2()
            .px_3()
            .py_1()
            .min_w_0()
            .when(selected, |this| this.bg(theme.secondary))
            .hover(|this| this.bg(theme.secondary))
            .on_click({
                let wb = wb.clone();
                move |_, _, cx| {
                    let _ =
                        wb.update(cx, |this, cx| this.stage_slash_select(&name, cx));
                }
            })
            .child(
                div()
                    .flex_none()
                    .text_size(px(13.))
                    .text_color(if selected {
                        theme.foreground
                    } else {
                        theme.muted_foreground
                    })
                    .child(format!("/{}", cmd.name)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child(description),
            );
        list = list.child(row);
    }
    list.into_any_element()
}

fn short_id(id: &str, fallback: &str) -> String {
    if id.is_empty() {
        fallback.to_string()
    } else {
        format!("{}…", &id[..id.len().min(8)])
    }
}

/// ProjectHistoryDialog's `projectName`: the last path segment.
/// `{count} sessions · {relative}` — the TS project rows' meta line
/// (`t("…sessionCount", {count})` + `fmtRelative(lastActive)`, both the
/// chooser and the history dialog); the relative half drops when the
/// stamp is absent.
fn project_meta_line(sessions: u64, last_active: u64) -> String {
    let count = fill(
        &t_count("projectDialog.sessionCount", sessions),
        &[("count", &sessions.to_string())],
    );
    let rel = fmt_relative(last_active, now_ms());
    if rel.is_empty() {
        count
    } else {
        format!("{count} · {rel}")
    }
}

fn project_name(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

/// The TS session-list filter (SessionList.tsx `filtered`): a session matches
/// when its title OR workspace base name contains the (already lowercased,
/// trimmed) query.
fn session_matches(sess: &SessionRow, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    sess.title
        .as_deref()
        .unwrap_or_default()
        .to_lowercase()
        .contains(query)
        || project_name(&sess.workspace).to_lowercase().contains(query)
}

fn render_row(
    row: &RowModel,
    is_last_row: bool,
    running: bool,
    copied: Option<&str>,
    wb: &WeakEntity<Workbench>,
    chat_font: (f32, f32),
    thought_full: &std::collections::HashSet<(String, usize)>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    match row.role {
        ChatRole::User => {
            render_user_row(row, copied, wb, chat_font, theme).into_any_element()
        }
        ChatRole::Assistant => render_assistant_row(
            row,
            is_last_row,
            running,
            copied,
            wb,
            chat_font,
            thought_full,
            theme,
        )
        .into_any_element(),
    }
}

fn gutter(label: &str, color: gpui::Hsla) -> gpui::Div {
    div()
        .w_12()
        .flex_none()
        .text_size(px(12.))
        .text_color(color)
        .child(label.to_string())
}

fn render_user_row(
    row: &RowModel,
    copied: Option<&str>,
    wb: &WeakEntity<Workbench>,
    chat_font: (f32, f32),
    theme: &component::Theme,
) -> gpui::Div {
    // Harness task lifecycle notices are system plumbing, not user speech
    // (E5 — ChatView.tsx `parseTaskNotification`): a centered chip with the
    // summary and status, no bubble, no copy affordance.
    if let Some((status, summary)) = parse_task_notification(&row_plain_text(row)) {
        let mut chip = div()
            .id(SharedString::from(format!("notice-{}", row.id)))
            .flex()
            .items_center()
            .gap_1()
            .min_w_0()
            .max_w(relative(0.8))
            .justify_center()
            .px_2()
            .py_1()
            .rounded_full()
            .border_1()
            .border_color(theme.border)
            .bg(theme.title_bar)
            .text_size(px(12.))
            .text_color(theme.muted_foreground)
            .child(
                Icon::new(KitIconName::Bell)
                    .small()
                    .flex_none()
                    .text_color(theme.muted_foreground),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .child(summary.is_empty().then(|| t!("chat.taskNotice").into_owned()).unwrap_or(summary)),
            );
        if !status.is_empty() {
            chip = chip.child(
                div()
                    .flex_none()
                    .child(format!("· {}", status.to_uppercase())),
            );
        }
        return div().flex().min_w_0().justify_center().child(chip.test_support());
    }
    if row.collapsed {
        // Replay harness fold (`_meta.zcode.collapsed`): the wall of context
        // text collapses to a chip, like ChatView's collapsed ids.
        return div()
            .flex()
            .gap_3()
            .child(gutter("You", theme.muted_foreground))
            .child(
            div()
                .px_2()
                .py_1()
                .rounded_md()
                .bg(theme.secondary)
                .text_size(px(12.))
                .text_color(theme.muted_foreground)
                .child(t!("chat.contextHandoff").into_owned()),
        );
    }
    let mut bubble = div().flex().flex_col().gap_2().min_w_0();
    for (part_index, part) in row.parts.iter().enumerate() {
        bubble = match part {
            PartView::Plain { text } => bubble.child(
                div()
                    .text_size(px(chat_font.0))
                    .line_height(relative(1.45))
                    .text_color(theme.foreground)
                    .child(text.clone()),
            ),
            PartView::ImageStub => bubble.child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child("[image attachment]"),
            ),
            // Click to enlarge (K2) — one stable id per row part.
            PartView::Image { src } => bubble.child(render_attachment_echo(
                src,
                theme,
                wb,
                SharedString::from(format!("msg-image-{}-{part_index}", row.id)),
            )),
            // TextView resolves its body size off the wrapper's text_size —
            // the ladder rides that inheritance.
            PartView::Markdown { state, .. } => bubble.child(
                div()
                    .text_size(px(chat_font.0))
                    .p_2()
                    .rounded_md()
                    .bg(theme.background)
                    .child(TextView::new(state)),
            ),
            PartView::ToolCall { .. } => bubble,
        };
    }
    let text = row_plain_text(row);
    div()
        .flex()
        .gap_3()
        .child(gutter("You", theme.muted_foreground))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(theme.secondary)
                .child(bubble),
        )
        .when(!text.trim().is_empty(), |this| {
            this.child(copy_button(row, text, copied == Some(row.id.as_str()), wb))
        })
}

fn render_assistant_row(
    row: &RowModel,
    is_last_row: bool,
    running: bool,
    copied: Option<&str>,
    wb: &WeakEntity<Workbench>,
    chat_font: (f32, f32),
    thought_full: &std::collections::HashSet<(String, usize)>,
    theme: &component::Theme,
) -> gpui::Div {
    let mut column = div().flex().flex_col().gap_2().min_w_0();
    let last_part = row.parts.len().saturating_sub(1);
    for (part_index, part) in row.parts.iter().enumerate() {
        column = match part {
            PartView::Markdown {
                state,
                thought: true,
                fold,
                scroll,
                ..
            } => {
                // ThoughtCard (ChatView.tsx:264): auto-open while it is the
                // turn's live tail, auto-collapse once the answer starts;
                // a manual toggle (fold = Some) defers permanently. Content
                // scrolls inside a capped window like the TS max-h-52.
                let streaming = running && is_last_row && part_index == last_part;
                let open = fold.unwrap_or(streaming);
                let label = if streaming {
                    t!("chat.thinking").into_owned()
                } else {
                    t!("chat.thought").into_owned()
                };
                let mut card = div()
                    .flex()
                    .flex_col()
                    .my_1()
                    .rounded_lg()
                    .bg(theme.secondary)
                    .overflow_hidden();
                let row_id = row.id.clone();
                let open_now = open;
                card = card.child(
                    div()
                        .id(SharedString::from(format!("thought-{}", row.id)))
                        .test_support()
                        .flex()
                        .items_center()
                        .gap_1()
                        .px_3()
                        .py(px(6.))
                        .cursor_pointer()
                        .hover(|h| h.bg(theme.title_bar))
                        .on_click({
                            let wb = wb.clone();
                            move |_, _window, cx| {
                                if let Some(wb) = wb.upgrade() {
                                    wb.update(cx, |wb, cx| {
                                        wb.set_thought_fold(&row_id, part_index, open_now, cx)
                                    });
                                }
                            }
                        })
                        .child(
                            Icon::new(IconName::ChevronDown)
                                .small()
                                .flex_none()
                                .when(!open, |icon| {
                                    icon.rotate(radians(-std::f32::consts::FRAC_PI_2))
                                })
                                .text_color(theme.muted_foreground),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme.muted_foreground)
                                .child(label),
                        ),
                );
                if open && streaming {
                    // TS ChatView:282-287 — while streaming AND open, pin
                    // the capped body to its bottom every render so the
                    // newest tokens stay in view. The handle carries the
                    // previous frame's extent (one-frame lag, like the TS
                    // post-render effect). The inner scroller is fine HERE:
                    // the pin owns the viewport anyway.
                    scroll.scroll_to_bottom();
                    card = card.child(
                        div()
                            .id(SharedString::from(format!("thought-body-{}", row.id)))
                            .test_support()
                            .track_scroll(scroll)
                            .max_h(px(208.))
                            .overflow_y_scroll()
                            .px_3()
                            .pb_2()
                            .text_size(px(chat_font.1))
                            .text_color(theme.muted_foreground)
                            .child(TextView::new(state)),
                    );
                } else if open {
                    // A SETTLED card gets no nested scroller (batch U): the
                    // chat list's ScrollableMask takes the wheel in the
                    // capture phase whenever the list can still move, so an
                    // inner scroll window only worked at the list's edges
                    // ("sometimes the session scrolls, sometimes the
                    // thought"). The body clips at the TS max-h-52 cap and
                    // expands inline on demand; the session scroll owns
                    // navigation.
                    let full = thought_full.contains(&(row.id.clone(), part_index));
                    card = card.child(
                        div()
                            .id(SharedString::from(format!("thought-body-{}", row.id)))
                            .test_support()
                            .when(!full, |this| this.max_h(px(208.)).overflow_hidden())
                            .px_3()
                            .pb_1()
                            .text_size(px(chat_font.1))
                            .text_color(theme.muted_foreground)
                            .child(TextView::new(state)),
                    );
                    let row_id = row.id.clone();
                    let wb = wb.clone();
                    card = card.child(
                        div().flex().justify_end().px_2().pb_1().child(
                            Button::new(SharedString::from(format!("thought-more-{}", row.id)))
                                .ghost()
                                .xsmall()
                                .label(if full { "Collapse" } else { "Show all" })
                                .on_click(move |_, _window, cx| {
                                    if let Some(wb) = wb.upgrade() {
                                        wb.update(cx, |wb, cx| {
                                            wb.toggle_thought_full(&row_id, part_index, cx)
                                        });
                                    }
                                }),
                        ),
                    );
                }
                column.child(card)
            }
            // TextView resolves its body size off the wrapper's text_size —
            // the ladder rides that inheritance (no wrapper = the window's
            // 16px default, which is the Medium step).
            PartView::Markdown { state, .. } => column.child(
                div()
                    .text_size(px(chat_font.1))
                    .child(TextView::new(state)),
            ),
            PartView::Plain { text } => column.child(
                div()
                    .text_size(px(chat_font.1))
                    .line_height(relative(1.55))
                    .text_color(theme.foreground)
                    .child(text.clone()),
            ),
            PartView::ToolCall {
                label,
                status,
                detail_head,
                detail,
                fold_kind,
                expanded,
                diff_src: _,
                diffs,
            } => {
                // ToolCard (E2 — ChatView.tsx:226 `<details>`): the summary
                // row (glyph, name, detail head) is the toggle; the body is
                // the FULL detail in a capped, scrollable window. Replay
                // folds (`context-handoff` / `tool-transcript`) keep the
                // summary clean — a handoff shows the localized label (the
                // wire title is English-only plumbing), the body text
                // behind the expand.
                let done = status == "completed";
                let glyph = if done { "✓" } else { "…" };
                let color = if done {
                    theme.muted_foreground
                } else {
                    theme.accent
                };
                let display: SharedString =
                    match fold_kind.as_deref() {
                        Some("context-handoff") => t!("chat.contextHandoff").into_owned().into(),
                        _ => label.clone(),
                    };
                let open_now = *expanded;
                let row_id = row.id.clone();
                let mut card = div()
                    .flex()
                    .flex_col()
                    .my_1()
                    .min_w_0()
                    .rounded_lg()
                    .bg(theme.secondary)
                    .overflow_hidden();
                card = card.child(
                    div()
                        .id(SharedString::from(format!("tool-{}-{}", row.id, part_index)))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .cursor_pointer()
                        .hover(|h| h.bg(theme.title_bar))
                        .on_click({
                            let wb = wb.clone();
                            move |_, _window, cx| {
                                if let Some(wb) = wb.upgrade() {
                                    wb.update(cx, |wb, cx| {
                                        wb.set_tool_expanded(&row_id, part_index, open_now, cx)
                                    });
                                }
                            }
                        })
                        .child(
                            div()
                                .flex_none()
                                .font_family(theme.mono_font_family.clone())
                                .text_size(px(12.))
                                .text_color(color)
                                .child(glyph),
                        )
                        .child(
                            div()
                                .flex_none()
                                .font_family(theme.mono_font_family.clone())
                                .text_size(px(12.))
                                .text_color(theme.foreground)
                                .child(display),
                        )
                        .when(fold_kind.is_none() && !detail_head.is_empty(), |this| {
                            this.child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .truncate()
                                    .font_family(theme.mono_font_family.clone())
                                    .text_size(px(12.))
                                    .text_color(theme.muted_foreground)
                                    .child(detail_head.clone()),
                            )
                        })
                        .child(
                            div().ml_auto().flex_none().child(
                                Icon::new(IconName::ChevronDown)
                                    .small()
                                    .flex_none()
                                    .when(!*expanded, |icon| {
                                        icon.rotate(radians(-std::f32::consts::FRAC_PI_2))
                                    })
                                    .text_color(theme.muted_foreground),
                            ),
                        )
                        .test_support(),
                );
                if *expanded {
                    // TS ToolCard body order: the diff blocks first, then the
                    // plain detail <pre> under them (ChatView.tsx:240-255).
                    let mut body = div()
                        .id(SharedString::from(format!(
                            "tool-detail-{}-{}",
                            row.id, part_index
                        )))
                        .max_h(px(256.))
                        .overflow_y_scroll()
                        .min_w_0()
                        .border_t_1()
                        .border_color(theme.border)
                        .font_family(theme.mono_font_family.clone())
                        .text_size(px(12.))
                        .line_height(relative(1.45));
                    for (bi, block) in diffs.iter().enumerate() {
                        body = body.child(render_diff_block(
                            block,
                            &row.id,
                            part_index,
                            bi,
                            theme,
                        ));
                    }
                    body = body.child(
                        div()
                            .px_2()
                            .py(px(6.))
                            .text_color(theme.muted_foreground)
                            .child(if detail.is_empty() {
                                "…".to_string()
                            } else {
                                detail.clone()
                            }),
                    );
                    card = card.child(body.test_support());
                }
                column.child(card)
            }
            PartView::ImageStub => column.child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child("[image attachment]"),
            ),
            PartView::Image { src } => column.child(render_attachment_echo(
                src,
                theme,
                wb,
                SharedString::from(format!("msg-image-{}-{part_index}", row.id)),
            )),
        };
    }
    // The trailing copy affordance (E4): the message's markdown source —
    // text parts only, thoughts and tool cards stay out (the TS ActionBar
    // copies the text blocks).
    let text = row
        .parts
        .iter()
        .filter_map(|p| match p {
            PartView::Markdown {
                thought: false,
                last_text,
                ..
            } => Some(last_text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    div()
        .flex()
        .gap_3()
        .child(gutter("Agent", theme.accent))
        .child(div().flex_1().min_w_0().child(column))
        .when(!text.trim().is_empty(), |this| {
            this.child(copy_button(row, text, copied == Some(row.id.as_str()), wb))
        })
}

impl Render for Workbench {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.clear_composer {
            self.clear_composer = false;
            let composer = self.composer.clone();
            window.defer(cx, move |window, cx| {
                composer.update(cx, |state, cx| state.set_value("", window, cx));
            });
        }
        // The rename sheet's prefill (open_rename deferred it here — the
        // menu click that opened the sheet had no window for set_value).
        if let Some(text) = self.rename_prefill.take() {
            let input = self.rename_input.clone();
            window.defer(cx, move |window, cx| {
                input.update(cx, |state, cx| state.set_value(&text, window, cx));
            });
        }
        if let Some(draft) = self.composer_prefill.take() {
            // editPending's prefill (ChatView `prefill`): text re-enters the
            // composer; drafts re-stage with decoded rasters so the strip
            // shows what a paste would. Enforces the MAX_IMAGES cap the same
            // way stage_inputs does.
            let composer = self.composer.clone();
            let text = draft.text.clone();
            let staged: Vec<StagedImage> = draft
                .images
                .into_iter()
                .take(MAX_IMAGES)
                .map(|d| StagedImage {
                    render: render_image_of(&d),
                    draft: d,
                })
                .collect();
            window.defer(cx, move |window, cx| {
                composer.update(cx, |state, cx| state.set_value(&text, window, cx));
            });
            self.staged = staged;
        }
        // A committed "/" completion lands the same windowless way: swap the
        // draft, then park the caret right after the inserted command (the
        // TS rAF park — set_value alone would strand it at offset 0).
        if let Some(insert) = self.pending_slash_insert.take() {
            let composer = self.composer.clone();
            window.defer(cx, move |window, cx| {
                composer.update(cx, |state, cx| {
                    state.set_value(insert.text, window, cx);
                    let (line, character) = insert.position;
                    state.set_cursor_position(
                        gpui_kit::base::input::Position::new(line, character),
                        window,
                        cx,
                    );
                });
            });
        }
        // The "/" menu's highlight resets when the query token changes (TS
        // `useEffect([token])`); the token itself is paint-derived, so the
        // compare happens here where &mut self exists.
        let live_token = self.slash_menu(cx).map(|m| m.token);
        if self.slash_token.as_deref() != live_token.as_deref() {
            self.slash_token = live_token;
            self.slash_highlight = 0;
        }
        // The inline approval card reconciles here: its interactive state
        // entities need a window (see reconcile_card).
        self.reconcile_card(window, cx);
        let toast = self.snap.toast.as_ref().map(|t| t.text.clone());
        let runtime = self.runtime.clone();
        let theme = cx.theme();
        // A snap page (driver-owned) or the files page replaces the
        // workbench surfaces below the status bar. A snap page wins over
        // the files page — its open path closes ours anyway; the guard just
        // keeps one page on screen if the states ever overlap.
        let snap_page_open = self.snap.system_status.is_some()
            || self.snap.models_page.is_some()
            || self.snap.quota_page.is_some()
            || self.snap.usage_page.is_some()
            || self.snap.config_section_page.is_some()
            || self.snap.workflows_page.is_some();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .font_family(cx.theme().font_family.clone())
            // Dragged files land anywhere in the workbench: read + sniff off
            // the main thread, stage what decodes (the third entry point
            // after the attach dialog and clipboard paste).
            .on_drop(cx.listener(|_this, paths: &ExternalPaths, _window, cx| {
                let paths = paths.paths().to_vec();
                let load = cx
                    .background_executor()
                    .spawn(async move { read_image_files(paths) });
                cx.spawn(async move |this, cx| {
                    let inputs = load.await;
                    if let Some(entity) = this.upgrade() {
                        let _ = cx.update_entity(&entity, |this, cx| {
                            this.stage_inputs(inputs, cx)
                        });
                    }
                })
                .detach();
            }))
            // ⌘Q (and the app menu's Quit item) lands here for the quit
            // guard before anything closes; so do ⌘K (session search) and
            // ⌘1..9 (tab selection) — the shortcuts fire from any focus
            // inside the workbench tree.
            .on_action(cx.listener(Self::on_quit_action))
            .on_action(cx.listener(Self::on_focus_search))
            .on_action(cx.listener(Self::on_stop_turn))
            .on_action(cx.listener(Self::on_select_tab1))
            .on_action(cx.listener(Self::on_select_tab2))
            .on_action(cx.listener(Self::on_select_tab3))
            .on_action(cx.listener(Self::on_select_tab4))
            .on_action(cx.listener(Self::on_select_tab5))
            .on_action(cx.listener(Self::on_select_tab6))
            .on_action(cx.listener(Self::on_select_tab7))
            .on_action(cx.listener(Self::on_select_tab8))
            .on_action(cx.listener(Self::on_select_tab9))
            .child(self.render_status_bar(cx))
            // An open configuration page replaces the workbench surfaces
            // below the status bar (the TS configuration screens replace the
            // chat/picker screens wholesale).
            .when_some(
                if !snap_page_open && !self.files_page_open {
                    Some(self.render_dock(cx))
                } else {
                    None
                },
                |this, content| this.child(content),
            )
            .when_some(
                self.snap.models_page.clone(),
                // The form replaces the page while open (TS ConfigFormSheet:
                // "a full screen of its own … a half-open form never sits
                // behind the section list").
                |this, _page| {
                    this.child(if self.models_form.is_some() {
                        self.render_models_form(cx)
                    } else {
                        self.render_models_page(cx)
                    })
                },
            )
            .when_some(
                self.snap.system_status.clone(),
                |this, page| {
                    this.child(render_system_status_page(
                        &page,
                        self.snap.config_notice.as_deref(),
                        self.snap.pending_restart,
                        // The restart addresses the ACTIVE TAB's bridge (the
                        // desktop's connected-session instance); with no tab
                        // open the note shows its no-instance hint instead.
                        self.snap.active_tab.is_some(),
                        toast.as_deref(),
                        &runtime,
                        cx,
                    ))
                },
            )
            .when_some(
                self.snap.quota_page.clone(),
                |this, _page| this.child(self.render_quota_page(cx)),
            )
            .when_some(
                self.snap.usage_page.clone(),
                |this, page| {
                    this.child(render_usage_page(
                        &page,
                        self.snap.config_notice.as_deref(),
                        self.snap.pending_restart,
                        // The restart addresses the ACTIVE TAB's bridge (the
                        // desktop's connected-session instance); with no tab
                        // open the note shows its no-instance hint instead.
                        self.snap.active_tab.is_some(),
                        &runtime,
                        &theme,
                    ))
                },
            )
            // The ZCode-config section pages (batch I): the open form
            // replaces the page while open, exactly like the Models form.
            .when_some(
                self.snap.config_section_page.clone(),
                |this, _page| {
                    this.child(if self.zconfig_form.is_some() {
                        self.render_zconfig_form(cx)
                    } else {
                        crate::zconfig_pages::render_config_section_page(self, cx)
                    })
                },
            )
            // The Workflows page (batch J): the same contract — the open
            // start/amend/meta form replaces the page while open.
            .when_some(
                self.snap.workflows_page.clone(),
                |this, _page| {
                    this.child(if self.workflow_start_form.is_some() {
                        self.render_workflow_start_form(cx)
                    } else if self.workflow_amend_form.is_some() {
                        self.render_workflow_amend_form(cx)
                    } else if self.workflow_meta_form.is_some() {
                        self.render_workflow_meta_form(cx)
                    } else {
                        crate::workflows_page::render_workflows_page(self, cx)
                    })
                },
            )
            // The Session Files page (F1): the same replacement contract as
            // the snap pages, but view-local — the right session panel's
            // row owns the open, back/Esc own the close.
            .when(self.files_page_open && !snap_page_open, |this| {
                this.child(self.render_files_page(cx))
            })
            // The WorkflowGate sheet (J2) sits above the config surfaces —
            // the TS bottom sheet is the topmost layer while open.
            .when(self.workflow_gate_sheet, |this| {
                this.child(self.render_workflow_gate_sheet(cx))
            })
            // The new-session chooser rides ON TOP of whatever surface is
            // below (the TS dialog is a fixed full-screen overlay).
            .when_some(
                self.snap.new_session.clone(),
                |this, view| this.child(self.render_new_session(&view, cx)),
            )
            // The rename sheet (G1) sits above the chooser — the TS action
            // sheet is the topmost layer while open.
            .when(self.rename_target.is_some(), |this| {
                this.child(self.render_rename_sheet(cx))
            })
            // The file viewer (F3) is the topmost layer — the TS FileViewer
            // full-screen reader; later siblings paint above, so this goes
            // last.
            .when_some(self.snap.files.viewing.clone(), |this, view| {
                // The viewer's local affordances (mobile FileViewer header):
                // an absolute path exists only where the hub is loopback AND
                // a listing names the workspace root (the canLocal pair).
                let local_abs = self
                    .snap
                    .hub_addr
                    .as_deref()
                    .filter(|addr| is_loopback_hub(addr))
                    .zip(self.snap.files.listing.as_ref().map(|l| l.root.clone()))
                    .map(|(_, root)| absolute_dir(&root, &view.path));
                let md_source =
                    matches!(&self.file_md_source, Some((p, true)) if *p == view.path);
                this.child(render_file_viewer(
                    &view,
                    &runtime,
                    &cx.entity().downgrade(),
                    local_abs,
                    md_source,
                    cx,
                ))
            })
            // The lightbox (K2) rides the kit's DEFERRED layer — it paints
            // above every non-deferred element regardless of tree order, so
            // no later sibling can overpaint it (the gpui-kit no-z-index
            // rule; same mechanism the kit's dialogs use).
            .when(self.lightbox.is_some(), |this| {
                this.child(self.render_lightbox(window, cx))
            })
    }
}

impl Workbench {
    /// The image lightbox overlay (K2, ChatView's Lightbox simplified): a
    /// near-black backdrop with the raster contained, Esc or a backdrop
    /// click closes. Mounted through `deferred(anchored(...))` — the kit's
    /// overlay layer, never sibling paint order.
    fn render_lightbox(&self, window: &mut Window, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let wb = cx.entity();
        let Some(boxed) = self.lightbox.as_ref() else {
            return div().into_any_element();
        };
        let viewport = window.viewport_size();
        // Pixel caps off the viewport (relative() here resolves against the
        // auto-sized card, which has no definite size to be relative TO).
        let cap_w = (viewport.width * 0.9).max(px(120.));
        let cap_h = (viewport.height * 0.85).max(px(120.));
        // The card swallows its own click so an inspect drag does not
        // dismiss; anything else on the backdrop closes.
        let card = div()
            .id("lightbox-card")
            .test_support()
            .flex()
            .flex_col()
            .items_center()
            .gap_2()
            .min_w_0()
            .max_w(cap_w)
            .max_h(cap_h)
            .p_2()
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .child(
                img(boxed.src.clone())
                    .max_w(cap_w)
                    .max_h(cap_h)
                    .object_fit(gpui::ObjectFit::Contain),
            )
            .when_some(boxed.caption.clone(), |this, caption| {
                this.child(
                    div()
                        .id("lightbox-caption")
                        .test_support()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.))
                        .text_color(theme.muted_foreground)
                        .child(caption),
                )
            });
        let backdrop = div()
            .id("lightbox-backdrop")
            .test_support()
            .flex()
            .items_center()
            .justify_center()
            .absolute()
            .top_0()
            .left_0()
            .w(viewport.width)
            .h(viewport.height)
            .bg(gpui::black().opacity(0.85))
            .on_mouse_down(MouseButton::Left, {
                let wb = wb.clone();
                move |_, _, cx| {
                    wb.update(cx, |this, cx| {
                        this.lightbox = None;
                        cx.notify();
                    });
                }
            })
            .child(card);
        deferred(
            anchored()
                .anchor(Anchor::TopLeft)
                .position(point(px(0.), px(0.)))
                .child(backdrop),
        )
        // Priority 2: above the slash menu's 1, so the modal backdrop both
        // covers the menu and wins its hit-tests (deferred draws sort by
        // priority ascending — the no-z-index rule's overlay discipline).
        .with_priority(2)
        .into_any_element()
    }
    /// The rename sheet (G1, TS SessionList's action sheet in rename mode):
    /// a prefilled single-line draft; Save dispatches the UiCommand and
    /// closes (the driver owns trim/cap; the toast owns the outcome). Esc
    /// and the backdrop close without sending.
    fn render_rename_sheet(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let wb = cx.entity();
        let can_save = !self.rename_input.read(cx).value().trim().is_empty();
        let card = div()
            .id("rename-card")
            .flex()
            .flex_col()
            .gap_2()
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
                    .text_size(px(13.))
                    .text_color(theme.foreground)
                    .child("Rename session"),
            )
            .child(
                div()
                    .min_w_0()
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child("The title shows here, in the editor, and in the ZCode app."),
            )
            .child(Input::new(&self.rename_input).small())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("rename-cancel")
                            .secondary()
                            .small()
                            .label("Cancel")
                            .on_click({
                                let wb = wb.clone();
                                move |_, _, cx| {
                                    wb.update(cx, |wb, cx| {
                                        wb.rename_target = None;
                                        cx.notify();
                                    })
                                }
                            }),
                    )
                    .child(
                        Button::new("rename-save")
                            .primary()
                            .small()
                            .label("Save")
                            .disabled(!can_save)
                            .on_click({
                                let wb = wb.clone();
                                move |_, _, cx| {
                                    wb.update(cx, |wb, cx| wb.submit_rename(cx))
                                }
                            }),
                    ),
            );
        div()
            .id("rename-sheet")
            .test_support()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .p_6()
            .bg(theme.background.opacity(0.65))
            .on_click({
                let wb = wb.clone();
                move |_, _, cx| {
                    wb.update(cx, |wb, cx| {
                        wb.rename_target = None;
                        cx.notify();
                    })
                }
            })
            .child(card)
            .into_any_element()
    }

    /// The new-session chooser (TS ProjectCreateDialog): a modal overlay —
    /// the hub's known projects, with a browse mode walking the machine's
    /// directories. Picking incubates a serve bridge for that workspace and
    /// opens a fresh session tab in it, so a new session is never a mystery
    /// about WHICH directory it belongs to.
    fn render_new_session(&self, view: &NewSessionView, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let runtime = self.runtime.clone();
        let creating = view.creating.clone();

        let mut card = div()
            .flex()
            .flex_col()
            .gap_2()
            .w_full()
            .max_w(px(560.))
            .max_h(px(500.))
            .p_4()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.title_bar);

        // Header: mode title, the browse/projects switch, close.
        card = card.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_size(px(14.))
                        .text_color(theme.foreground)
                        .child(if view.browse.is_some() {
                            "Browse directories".to_string()
                        } else {
                            "New session".to_string()
                        }),
                )
                .child(div().flex_1())
                .when(view.browse.is_none(), |this| {
                    this.child(
                        Button::new("ns-browse")
                            .ghost()
                            .xsmall()
                            .label("Browse")
                            .tooltip("Walk the machine's directories instead")
                            .on_click({
                                let runtime = runtime.clone();
                                move |_, _, _| {
                                    runtime.send(UiCommand::NewSessionBrowse { path: None })
                                }
                            }),
                    )
                })
                .when(view.browse.is_some(), |this| {
                    this.child(
                        Button::new("ns-projects")
                            .ghost()
                            .xsmall()
                            .label("Projects")
                            .tooltip("Back to the known-projects list")
                            .on_click({
                                let runtime = runtime.clone();
                                move |_, _, _| runtime.send(UiCommand::NewSessionCloseBrowse)
                            }),
                    )
                })
                .child(
                    Button::new("ns-close")
                        .ghost()
                        .xsmall()
                        .icon(IconName::CircleX)
                        .tooltip("Close — Esc")
                        .on_click({
                            let runtime = runtime.clone();
                            move |_, _, _| runtime.send(UiCommand::NewSessionToggle)
                        }),
                ),
        );

        // The known-projects filter (TS ProjectCreateDialog's input) — only
        // meaningful in projects mode; browse mode walks the whole machine.
        if view.browse.is_none() {
            card = card.child(
                div().px_1().child(Input::new(&self.new_session_filter).small()),
            );
        }

        // The scrollable list: browse mode or the known-projects list builds
        // a plain column; the wrapper owns the id (scroll state).
        let content = if let Some(browse) = &view.browse {
            self.render_ns_browse(browse, creating.as_deref(), cx)
        } else {
            let mut list = div().flex().flex_col().gap_1();
            // Known projects: the hub's whitelist, one row per workspace.
            if let Some(err) = &view.error {
                list = list.child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.warning_foreground)
                        .child(err.clone()),
                );
            }
            match &view.projects {
                None => {
                    list = list.child(
                        div()
                            .text_size(px(13.))
                            .text_color(theme.muted_foreground)
                            .child("loading projects…"),
                    );
                }
                Some(projects) if projects.is_empty() => {
                    list = list.child(
                        div()
                            .text_size(px(13.))
                            .text_color(theme.muted_foreground)
                            .child("No known projects yet — use Browse to pick a directory."),
                    );
                }
                Some(projects) => {
                    let q = self
                        .new_session_filter
                        .read(cx)
                        .value()
                        .trim()
                        .to_lowercase();
                    let rows: Vec<_> = projects
                        .iter()
                        .filter(|p| q.is_empty() || p.workspace_path.to_lowercase().contains(&q))
                        .collect();
                    if rows.is_empty() {
                        list = list.child(
                            div()
                                .id("ns-no-match")
                                .test_support()
                                .text_size(px(13.))
                                .text_color(theme.muted_foreground)
                                .child(format!("no project matches \"{q}\"")),
                        );
                    }
                    for p in rows {
                        let runtime = runtime.clone();
                        let workspace_path = p.workspace_path.clone();
                        let busy = creating.as_deref() == Some(workspace_path.as_str());
                        let name = workspace_path
                            .rsplit('/')
                            .next()
                            .unwrap_or(workspace_path.as_str())
                            .to_string();
                        let mut row = div()
                            .id(SharedString::from(format!("ns-project-{workspace_path}")))
                            .test_support()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py(px(6.))
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|h| h.bg(theme.secondary))
                            .on_click({
                                let runtime = runtime.clone();
                                let ws = workspace_path.clone();
                                move |_, _, _| {
                                    runtime.send(UiCommand::NewSessionCreate {
                                        workspace_path: ws.clone(),
                                    })
                                }
                            });
                        row = row.child(
                            div()
                                .flex()
                                .flex_col()
                                .min_w_0()
                                .flex_1()
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(px(13.))
                                        .text_color(theme.foreground)
                                        .child(name),
                                )
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(px(10.))
                                        .font_family(theme.mono_font_family.clone())
                                        .text_color(theme.muted_foreground)
                                        .child(workspace_path.clone()),
                                ),
                        );
                        row = row.child(
                            div()
                                .flex_none()
                                .text_size(px(10.))
                                .text_color(if busy {
                                    theme.accent
                                } else {
                                    theme.muted_foreground
                                })
                                .child(if busy {
                                    "creating…".to_string()
                                } else {
                                    project_meta_line(p.sessions, p.last_active)
                                }),
                        );
                        list = list.child(row);
                    }
                }
            }
            list
        };
        card = card.child(
            div()
                .id("ns-body")
                .flex()
                .flex_col()
                .min_h_0()
                .overflow_y_scroll()
                .child(content),
        );

        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .p_6()
            .bg(theme.background.opacity(0.65))
            .child(card)
            .into_any_element()
    }

    /// The browse mode's directory listing (TS ProjectCreateDialog browse):
    /// resolved path, "start here" gated on `creatable`, parent navigation,
    /// subdirectory rows, and the truncated-list note.
    fn render_ns_browse(
        &self,
        browse: &FsBrowseView,
        creating: Option<&str>,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let theme = cx.theme();
        let runtime = self.runtime.clone();
        let mut body = div().flex().flex_col().gap_1();
        body = body.child(
            div()
                .min_w_0()
                .truncate()
                .text_size(px(10.))
                .font_family(theme.mono_font_family.clone())
                .text_color(theme.muted_foreground)
                .child(if browse.loading {
                    "reading…".to_string()
                } else {
                    browse.path.clone()
                }),
        );
        if let Some(err) = &browse.error {
            body = body.child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.warning_foreground)
                    .child(err.clone()),
            );
        }
        // Start here — only where the hub allows a session (creatable).
        let busy_here = creating == Some(browse.path.as_str());
        body = body.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .py_1()
                .child(
                    Button::new("ns-start-here")
                        .primary()
                        .small()
                        .label("Start session here")
                        .disabled(!browse.creatable || creating.is_some())
                        .tooltip("Only known projects and remote.projectRoots directories can start a session")
                        .on_click({
                            let runtime = runtime.clone();
                            let ws = browse.path.clone();
                            move |_, _, _| {
                                runtime.send(UiCommand::NewSessionCreate {
                                    workspace_path: ws.clone(),
                                })
                            }
                        }),
                )
                .when(busy_here, |this| {
                    this.child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.accent)
                            .child("creating…"),
                    )
                })
                .when(!browse.creatable, |this| {
                    this.child(
                        div()
                            .text_size(px(10.))
                            .text_color(theme.muted_foreground)
                            .child("not allowed here — go into a project directory"),
                    )
                }),
        );
        if let Some(parent) = &browse.parent {
            let runtime = runtime.clone();
            let parent_label = parent.clone();
            let parent = parent.clone();
            body = body.child(
                div()
                    .id("ns-parent")
                    .test_support()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|h| h.bg(theme.secondary))
                    .on_click(move |_, _, _| {
                        runtime.send(UiCommand::NewSessionBrowse {
                            path: Some(parent.clone()),
                        })
                    })
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(12.))
                            .text_color(theme.muted_foreground)
                            .child("↑"),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .font_family(theme.mono_font_family.clone())
                            .text_color(theme.muted_foreground)
                            .child(parent_label),
                    ),
            );
        }
        if !browse.loading && browse.entries.is_empty() && browse.error.is_none() {
            body = body.child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child("No subdirectories."),
            );
        }
        for name in &browse.entries {
            let runtime = runtime.clone();
            let path = format!("{}/{}", browse.path.trim_end_matches('/'), name);
            body = body.child(
                div()
                    .id(SharedString::from(format!("ns-dir-{path}")))
                    .test_support()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|h| h.bg(theme.secondary))
                    .on_click({
                        let runtime = runtime.clone();
                        let path = path.clone();
                        move |_, _, _| {
                            runtime.send(UiCommand::NewSessionBrowse {
                                path: Some(path.clone()),
                            })
                        }
                    })
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(12.))
                            .text_color(theme.muted_foreground)
                            .child("▸"),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.))
                            .text_color(theme.foreground)
                            .child(name.clone()),
                    ),
            );
        }
        if browse.truncated {
            body = body.child(
                div()
                    .text_size(px(10.))
                    .text_color(theme.muted_foreground)
                    .child("list truncated — enter a subdirectory to keep walking"),
            );
        }
        body
    }
}

/// Pure-logic units of batch E: the TUI-parity formatters and the
/// task-notification parser.
#[cfg(test)]
mod display_unit_tests {
    // Explicit imports only (the file-root kit glob shadows #[test]).
    use super::{context_pct, fmt_context, fmt_tokens, parse_task_notification, PartView, RowModel};
    use protocol::chat::ChatRole;

    fn plain_row(text: &str) -> RowModel {
        RowModel {
            id: "m1".to_string(),
            role: ChatRole::User,
            collapsed: false,
            parts: vec![PartView::Plain {
                text: text.to_string(),
            }],
        }
    }

    #[test]
    fn token_formatting_matches_the_tui_dock() {
        // ChatScreen.tsx fmtTokens: >=100 of the unit rounds to an integer
        // (`128000` -> `128K`), below keeps one decimal (`1.0M`).
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_000), "1.0K");
        assert_eq!(fmt_tokens(12_800), "12.8K");
        assert_eq!(fmt_tokens(128_000), "128K");
        assert_eq!(fmt_tokens(1_000_000), "1.0M");
        assert_eq!(fmt_tokens(1_500_000), "1.5M");
        assert_eq!(fmt_tokens(128_000_000), "128M");
    }

    #[test]
    fn context_gauge_clamps_and_formats() {
        assert_eq!(fmt_context(175_000, 1_000_000), "17.5%/1.0M");
        assert_eq!(fmt_context(1_200_000, 1_000_000), "100.0%/1.0M");
        // size 0 never paints on the TS side; the formatter stays total.
        assert_eq!(fmt_context(0, 0), "100.0%/0");
        // The composer stat's enriched head (batch S): the percent comes
        // from the same clamp, the absolute pair rides after it.
        assert_eq!(context_pct(175_000, 1_000_000), 17.5);
        assert_eq!(context_pct(1_200_000, 1_000_000), 100.0);
    }

    #[test]
    fn task_notification_parses_status_and_summary() {
        let (status, summary) = parse_task_notification(
            "<task-notification>\n <status> completed </status>\n <summary>build ok</summary>\n</task-notification>",
        )
        .expect("a full block parses");
        assert_eq!(status, "completed");
        assert_eq!(summary, "build ok");
        // Missing tags yield empty strings, not None.
        let (status, summary) =
            parse_task_notification("<task-notification>plain</task-notification>").unwrap();
        assert_eq!((status.as_str(), summary.as_str()), ("", ""));
    }

    #[test]
    fn non_notification_text_is_not_a_notice() {
        assert!(parse_task_notification("just a question").is_none());
        assert!(parse_task_notification(
            "prefix <task-notification><status>x</status></task-notification>"
        )
        .is_none());
        assert!(parse_task_notification(
            "<task-notification><status>x</status></task-notification> suffix"
        )
        .is_none());
        // A nested tag inside <status> breaks the TS `[^<]*?` match.
        let (status, _) = parse_task_notification(
            "<task-notification><status>a<b</status><summary>s</summary></task-notification>",
        )
        .unwrap();
        assert_eq!(status, "");
    }

    #[test]
    fn row_plain_text_concatenates_only_text_parts() {
        let mut row = plain_row("one");
        row.parts.push(PartView::ImageStub);
        row.parts.push(PartView::Plain {
            text: "two".to_string(),
        });
        // The TS join("") has no separator.
        assert_eq!(super::row_plain_text(&row), "onetwo");
    }
}

/// Batch K pure units: the "/" completion's caret token scan, the "$"-aware
/// prefix matcher, and the post-commit caret park arithmetic.
#[cfg(test)]
mod slash_unit_tests {
    // Explicit imports only (the file-root kit glob shadows #[test]).
    use super::{caret_position_of, slash_matches, slash_token_range};
    use protocol::acp::SlashCommand;

    fn cmd(name: &str) -> SlashCommand {
        SlashCommand {
            name: name.to_string(),
            description: None,
            input: None,
        }
    }

    #[test]
    fn token_scan_takes_the_word_around_the_caret() {
        // Mid-word caret: the whole word.
        assert_eq!(slash_token_range("hello /wor", 10), Some((6, 10)));
        // Any word position completes — a slash mid-draft is still a token
        // (only the harness executes start-of-draft commands).
        assert_eq!(slash_token_range("run /new now", 6), Some((4, 8)));
        // A caret on the NEXT word's start edge sees no word yet ("" in TS);
        // mid-word still completes.
        assert_eq!(slash_token_range("hi /cmd", 3), None);
        assert_eq!(slash_token_range("hi /cmd", 6), Some((3, 7)));
        // Non-slash words never complete.
        assert_eq!(slash_token_range("hello", 2), None);
        // Bare "/" at the caret is a token (empty query lists everything).
        assert_eq!(slash_token_range("/", 1), Some((0, 1)));
        // The scan respects char boundaries (multibyte neighbors).
        assert_eq!(slash_token_range("哈 /tdd", 6), Some((4, 8)));
    }

    #[test]
    fn matcher_strips_the_dollar_on_both_sides_and_keeps_order() {
        let commands = vec![cmd("plan"), cmd("$tdd"), cmd("Prompt")];
        let names = |token: &str| {
            slash_matches(token, &commands)
                .into_iter()
                .map(|c| c.name)
                .collect::<Vec<_>>()
        };
        // Empty query: every command, bridge order.
        assert_eq!(names("/"), vec!["plan", "$tdd", "Prompt"]);
        // "$tdd" matches a plain "/td" (marker stripped on both sides) and
        // the explicit "/$t" spelling alike.
        assert_eq!(names("/td"), vec!["$tdd"]);
        assert_eq!(names("/$t"), vec!["$tdd"]);
        // Prefix matching is case-insensitive.
        assert_eq!(names("/PR"), vec!["Prompt"]);
        // No match: empty.
        assert!(names("/zz").is_empty());
    }

    #[test]
    fn caret_park_counts_lines_and_characters() {
        assert_eq!(caret_position_of("/cmd rest", 5), (0, 5));
        assert_eq!(caret_position_of("one\ntwo /cmd", 10), (1, 6));
        // Multibyte chars count as one character.
        assert_eq!(caret_position_of("哈\n/tdd x", 6), (1, 2));
        // Offset at 0 parks at the origin.
        assert_eq!(caret_position_of("abc", 0), (0, 0));
    }
}

/// Headless UI checks for the newest chrome — the only window tests kept
/// (the rest of the surface is judged by hand): the status bar's quota line
/// and the servers dialog.
#[cfg(test)]
mod ui_tests {
    // Explicit imports only — the file-root kit glob would shadow #[test].
    use super::Workbench;
    use crate::runtime::{
        Phase, ProtocolRuntime, TabPhase, TabSummary, UiCommand, WorkbenchSnapshot,
    };
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{
        px, AnyWindowHandle, App, AppContext as _, Bounds, Entity, Point, Size, TestAppContext,
        WindowBounds, WindowOptions,
    };

    fn connected_snapshot() -> WorkbenchSnapshot {
        let mut snap = WorkbenchSnapshot::initial();
        snap.phase = Phase::Connected;
        snap.tabs = vec![TabSummary {
            id: 7,
            session_id: Some("sess_1".to_string()),
            session_title: None,
            phase: TabPhase::Connected,
            running: false,
            pending: 0,
            awaiting: false,
        }];
        snap.active_tab = Some(7);
        snap
    }

    fn open(
        cx: &mut App,
        snap: WorkbenchSnapshot,
        width: f32,
    ) -> (AnyWindowHandle, Entity<Workbench>, ProtocolRuntime) {
        let runtime = ProtocolRuntime::inert();
        let (handle, wb) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: Size {
                        width: px(width),
                        height: px(700.),
                    },
                })),
                ..Default::default()
            },
            cx,
            {
                let runtime = runtime.clone();
                move |window, cx| cx.new(|cx| Workbench::new(window, cx, runtime, snap))
            },
        )
        .expect("open test window");
        (handle, wb, runtime)
    }

    /// The quota line takes the width the left cluster leaves free instead
    /// of clipping at a fixed cap — and still stops at 60% of the bar, so
    /// an absurd line cannot squeeze the bridge label out.
    #[gpui_kit::test]
    fn status_bar_quota_line_uses_the_free_width(cx: &mut TestAppContext) {
        let _windows = super::window_test_lock();
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let mut snap = connected_snapshot();
        snap.quota_dock = Some(
            "GLM coding plan · 5h 42% ↻ 2h13m · week 12% ↻ 3d 4h · month 61% · $3.50"
                .to_string(),
        );
        let (handle, _wb, _runtime) = cx.update(|cx| open(cx, snap, 1000.));
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            let dock = window.find("status-quota-dock").bounds();
            assert!(
                dock.size.width > px(400.),
                "a long line must run past the old 340px cap, got {:?}",
                dock.size.width
            );
            assert!(window.find("status-bridge").visible());
        })
        .unwrap();

        let mut snap = connected_snapshot();
        snap.quota_dock = Some("x".repeat(400));
        let (handle, _wb, _runtime) = cx.update(|cx| open(cx, snap, 1000.));
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            let dock = window.find("status-quota-dock").bounds();
            assert!(
                dock.size.width <= px(610.),
                "the line must stop at 60% of the bar, got {:?}",
                dock.size.width
            );
            let bridge = window.find("status-bridge").bounds();
            assert!(bridge.right() <= dock.left(), "the bridge label keeps its place");
        })
        .unwrap();
    }

    /// Add and Edit open the servers dialog: Edit prefills the record and
    /// Save dispatches `UpdateServer` for it (then closes); the Add form
    /// refuses to submit while empty.
    #[gpui_kit::test]
    fn servers_dialog_edits_and_adds(cx: &mut TestAppContext) {
        let _windows = super::window_test_lock();
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let mut snap = connected_snapshot();
        snap.servers = vec![protocol::hub::SavedServer {
            id: "srv-1".to_string(),
            name: "Remote dev box".to_string(),
            hub_url: "https://hub.example:8377".to_string(),
            token: "tok".to_string(),
        }];
        snap.active_server_id = Some("srv-1".to_string());
        let (handle, _wb, runtime) = cx.update(|cx| open(cx, snap, 1000.));
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            window.click("dock-rail-config", cx);
            window.draw(cx).clear(cx);
            assert!(window.try_find("server-form").is_none(), "no form until asked");

            window.click("server-edit-srv-1", cx);
            window.draw(cx).clear(cx);
            assert!(window.find("server-form").visible(), "Edit opens the dialog");
            assert_eq!(
                window.find("server-form-save").label().as_deref(),
                Some("Save")
            );
            window.click("server-form-save", cx);
            window.draw(cx).clear(cx);
            assert!(
                runtime.recorded_commands().iter().any(|c| matches!(
                    c,
                    UiCommand::UpdateServer { id, name, hub_url, token }
                        if id == "srv-1"
                            && name == "Remote dev box"
                            && hub_url == "https://hub.example:8377"
                            && token == "tok"
                )),
                "Save writes the prefilled record back"
            );
            assert!(
                window.try_find("server-form").is_none(),
                "Save closes the dialog"
            );

            window.click("server-add", cx);
            window.draw(cx).clear(cx);
            assert!(window.find("server-form").visible(), "Add opens the dialog");
            assert_eq!(
                window.find("server-form-save").label().as_deref(),
                Some("Add and connect")
            );
            window.click("server-form-save", cx);
            window.draw(cx).clear(cx);
            assert!(
                !runtime
                    .recorded_commands()
                    .iter()
                    .any(|c| matches!(c, UiCommand::AddServer { .. })),
                "an empty form never submits"
            );
            window.click("server-form-cancel", cx);
            window.draw(cx).clear(cx);
            assert!(window.try_find("server-form").is_none(), "Cancel closes");
        })
        .unwrap();
    }
}
