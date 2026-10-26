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
//!   A dropped NON-image file or directory never bounces — its path lands
//!   in the composer as a reference the prompt sends as text.

use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    div, img, px, relative, AnyElement, App, ClipboardEntry, ClipboardItem, Context, Entity,
    ExternalPaths, ImageFormat, MouseButton, PathPromptOptions, SharedString, Subscription, Window,
};
use gpui_kit::component;
use gpui_kit::component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::text::{TextView, TextViewMotion, TextViewState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    h_resizable, resizable_panel, v_resizable, ActiveTheme as _, Disableable as _, Icon, IconName,
    ResizableState, Selectable as _, Sizable as _,
};
use gpui_kit::TestSupportExt as _;
use gpui_kit::{assets::IconName as KitIconName, base::TextSelection, *};
use protocol::chat::{AttachmentDraft, ChatMessage, ChatPart, ChatRole};
use protocol::image::{
    decode_attachment_bytes, prepare_image, sniff_mime, PrepareError, MAX_IMAGES,
};
use rust_i18n::t;

use crate::approval_card::{render_approval_card, render_toast_strip, ApprovalCard};
use crate::diff_view::{blocks_of, DiffBlock, DiffLineKind};
use crate::file_browser::{
    absolute_dir, is_loopback_hub, render_file_viewer, render_files_header, render_files_panel,
    FilesPanelCtx,
};
use crate::i18n::{fill, fmt_relative, now_ms, parse_iso_ms, t_count};
use crate::models_page::ModelsForm;
use crate::quota_page::SpendStage;
use crate::runtime::{
    FsBrowseView, HistoryRow, HistorySelected, NewSessionView, Phase, ProtocolRuntime,
    RuntimeEvent, SessionRow, TabPhase, TabSummary, TabView, UiCommand, WorkbenchSnapshot,
};
use crate::server_form::ServerForm;
use crate::system_status::render_system_status_page;
use crate::terminal_panel;
use crate::ui_prefs::ui_text;
use crate::usage_page::render_usage_page;
use crate::workflows_page::{WorkflowAmendForm, WorkflowMetaForm, WorkflowStartForm};
use crate::zconfig_pages::ZConfigForm;

/// Stream-reveal timing for streamed markdown chunks (the Claude-style fade
/// TextView ships; numbers from the gpui-component defaults).
const STREAM_FADE_MS: u64 = 280;
const STREAM_FADE_STAGGER_MS: u64 = 10;

/// "Stop Turns & Quit" holds the window open this long after sending the
/// `session/cancel` notifications — fire-and-forget frames need a beat to
/// reach the socket before the process may exit.
const QUIT_FLUSH_MS: u64 = 800;
/// The left rail's fixed width — the drawer anchors flush against its right
/// edge (taffy's border-box default makes `w(px(40))` the full 40px).
const RAIL_WIDTH: f32 = 40.;
/// The overlay drawer's fixed width: wide enough for the settings entries
/// and the quota rows, narrow enough that the chat beside it stays
/// usable at typical window sizes.
const DRAWER_WIDTH: f32 = 380.;

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
        ToggleTerminal,
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

/// What the left overlay drawer shows. The drawer slides out beside the
/// rail and floats above the workbench (a drawer, not a docked panel):
/// nothing below is reflowed or replaced, and it closes through its X, Esc,
/// or the rail toggle — never a modal backdrop. Both surfaces are view-
/// local; each seeds its reads on open (see [`Workbench::toggle_drawer`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrawerSurface {
    /// The settings list (TS SettingsPanel): the server book, appearance,
    /// and the config-page entries.
    Settings,
    /// The full quota display (TS QuotaSection at drawer scale): every
    /// provider section the hub reports. The plan-quota page with its reset
    /// cards is a separate full page — its entry lives in Settings.
    Quota,
}

impl DrawerSurface {
    /// Stable element-id fragment for the rail toggles.
    fn key(self) -> &'static str {
        match self {
            DrawerSurface::Settings => "config",
            DrawerSurface::Quota => "quota",
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
    /// The composer textarea. Bound to the ACTIVE session: a tab switch
    /// parks the outgoing tab's draft in [`Self::tab_drafts`] and adopts
    /// the incoming one's, so text typed for one conversation can never
    /// ride into another (send targets the active tab).
    composer: Entity<TextareaState>,
    /// The composer's clear-on-send, applied during render (set_value is
    /// window-gated and the Enter subscription has no window).
    clear_composer: bool,
    /// A queued draft returned for editing (RuntimeEvent::ComposerPrefill —
    /// ChatView `editPending`): applied during render like clear_composer,
    /// then dropped. Text re-enters the composer; drafts re-stage with their
    /// decoded rasters so the strip shows the same thumbnails a paste would.
    composer_prefill: Option<protocol::chat::PromptDraft>,
    /// A dropped non-image file or directory, as path references waiting to
    /// append to the composer text — parked for the render pass because
    /// `set_value` needs a window (the composer_prefill route).
    composer_append: Option<String>,
    /// The incoming tab's draft on a switch, adopted during render (the
    /// composer_prefill route: set_value needs a window).
    composer_restore: Option<(String, Vec<StagedImage>)>,
    /// Per-tab parked composer drafts (text + staged attachments), keyed by
    /// tab id — the storage behind the composer's active-session binding.
    tab_drafts: std::collections::HashMap<u64, (String, Vec<StagedImage>)>,
    /// Staged attachments waiting for the next send (ChatView `images`),
    /// capped at [`MAX_IMAGES`]. The ACTIVE tab's live set; a switch parks
    /// it into `tab_drafts`.
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
    /// Quit-dialog state: the modal is up (one at a time), a stop-all is
    /// flushing its cancels before the window may close, and the flag that
    /// lets the confirmed pass-through close without asking again.
    quit_dialog_open: bool,
    quit_stopping: bool,
    quit_confirmed: bool,
    /// The left dock (view-local layout, never wire state): the sessions
    /// panel's open/collapsed flag and the splitter's persisted sizes.
    dock_open: bool,
    dock_sizes: Entity<ResizableState>,
    /// Viewport width the dock states were last laid out against: a change
    /// resets both splitters (their stored PIXEL sizes belong to the OLD
    /// width and otherwise pin the chat past a narrowed window).
    last_viewport_w: Option<gpui::Pixels>,
    /// The right overlay drawer (view-local layout): settings or plan quota,
    /// floating above the surfaces below without reflowing them.
    drawer: Option<DrawerSurface>,
    /// The right session panel: the active session's file browser. View-local
    /// like the left dock, with its own splitter.
    session_panel_open: bool,
    session_dock_sizes: Entity<ResizableState>,
    /// Per-tab embedded terminals (view-local, never wire state): each tab
    /// owns its own dock, spawned lazily on that tab's first open — the
    /// tab's session workspace is the shell's cwd — and kept alive across
    /// hide/show and tab switches. Closing the tab drops its dock, which
    /// kills the shell (`TerminalDock::drop`). The vec is the tab's pane
    /// row (split terminals): each pane is a full dock; the cap lives in
    /// terminal_panel (`MAX_TERMINAL_PANES`). Under test (cfg!(test)) the
    /// rail toggle never spawns (placeholder only) — `split_terminal`
    /// does, so pane logic stays testable.
    tab_terminals: std::collections::HashMap<u64, Vec<terminal_panel::TerminalDock>>,
    /// Tabs whose terminal dock is shown (`tab_terminals` keeps the shell
    /// alive while hidden — the flag only paints).
    tab_terminal_open: std::collections::HashSet<u64>,
    /// The chat ↔ terminal vertical split's sizes (shared: the panel height
    /// is a layout fact, not per-tab state).
    terminal_sizes: Entity<ResizableState>,
    /// The font ladder (TS `fontSize`): scales the reading text through the
    /// chat px pairs and every other UI role through the paint-time factor
    /// (`ui_prefs::ui_text` reads the published step).
    font_scale: crate::ui_prefs::FontScale,
    /// The session the open panel's listing was seeded FOR — a session
    /// switch under an open panel re-reads the new session's root instead of
    /// leaving the previous session's browse on screen.
    files_panel_session: Option<String>,
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
    /// The Models page's provider endpoint form (base URL / API type / key):
    /// `None` = closed. Separate from `models_form` — the two edit different
    /// files and never open together.
    pub(crate) provider_form: Option<crate::models_page::ProviderFormState>,
    pub(crate) provider_form_subs: Vec<Subscription>,
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
    /// The Agents page's armed delete: the name whose delete has been pressed
    /// once. Deleting loses the prompt body with no undo from the app, so it
    /// takes the same two-gesture confirm as a backup restore.
    pub(crate) agent_delete_confirm: Option<String>,
    /// The Skills page's armed delete: the skill path whose delete has been
    /// pressed once — the directory removal has no undo either.
    pub(crate) skill_delete_confirm: Option<String>,
    /// The MCP page's armed delete: the server name whose delete has been
    /// pressed once — the config entry is gone with no undo.
    pub(crate) mcp_delete_confirm: Option<String>,
    /// The Models page's armed delete: `providerId/modelId` of the row whose
    /// delete has been pressed once.
    pub(crate) model_delete_confirm: Option<String>,
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
    /// Manual fold verdicts for adjacent tool-call RUNS, keyed by (row id,
    /// index of the run's first part). Absent = never toggled, so the run
    /// follows its own state: open while a call is still in flight
    /// (watchable as it happens), folded once every call settled. A present
    /// entry is the user's permanent verdict (the ThoughtCard rule).
    tool_run_open: std::collections::HashMap<(String, usize), bool>,
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

/// The card summary's display name, forced onto ONE line: the wire title is
/// the raw command and a heredoc/`&&` chain carries real newlines — rendered
/// as-is it turns the summary into a multi-line wall. Whitespace runs collapse
/// to single spaces and the result is capped for measure cost; the row's
/// `truncate` handles the remaining width.
fn tool_display_name(tool_name: &str) -> String {
    let mut out = String::with_capacity(tool_name.len().min(240));
    let mut pending_space = false;
    for ch in tool_name.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
        if out.chars().count() >= 240 {
            out.push('…');
            break;
        }
    }
    out
}

/// The name shown in the aggregated tool-run summary: raw backend tool name
/// when present, else the segment before the first colon ("Bash: npm test" →
/// "Bash") — the TS `toolNameOf`, so a group reads like its rows.
fn tool_short_name(label: &str) -> String {
    let head = label.split(':').next().unwrap_or(label).trim();
    let name = if head.is_empty() { label.trim() } else { head };
    tool_display_name(name)
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
        let mut row = div().min_w_0().flex().px_2().gap_1().text_color(text_color);
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
/// The composer stat composes its own enriched variant from [`context_pct`],
/// so this spelling has no caller — kept only for the clamp's TS parity test.
#[cfg(test)]
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
fn copy_button(row: &RowModel, text: String, copied: bool, wb: &WeakEntity<Workbench>) -> Button {
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

/// The chat row's right-click menu (the E4 copy affordance as a context
/// menu): the window's LIVE text selection first, then the row's message
/// source — the same text the trailing copy button writes. The selection is
/// captured at OPEN time, not click time: this framework clears the
/// selection during a left press (the menu item IS one), so a click-time
/// read would always come back empty.
fn chat_row_menu(
    text: String,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
    move |menu, window, cx| {
        let mut m = menu;
        let selection = TextSelection::has_selection(window, cx)
            .then(|| TextSelection::selected_text(window, cx))
            .filter(|s| !s.trim().is_empty());
        let has_text = !text.trim().is_empty();
        if let Some(selected) = selection {
            m = m.item(
                PopupMenuItem::new("Copy selection").on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(selected.clone()));
                }),
            );
            if has_text {
                m = m.separator();
            }
        }
        if has_text {
            let text = text.clone();
            m = m.item(
                PopupMenuItem::new("Copy message text").on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                }),
            );
        }
        m
    }
}

/// Drag-and-drop payload of one tab (strip reorder): the dragged tab's id.
/// A newtype — not a bare `u64` — so the drop targets can name exactly this
/// drag (`on_drop::<TabDragId>`), never some other u64 payload.
#[derive(Clone, Copy, PartialEq, Eq)]
struct TabDragId(u64);

/// The pill that follows the cursor while a tab drags (gpui renders the
/// constructor's entity under the pointer; see `on_drag`).
struct TabDragPreview(SharedString);

impl Render for TabDragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.title_bar)
            .font_family(theme.mono_font_family.clone())
            .text_size(ui_text(13.))
            .text_color(theme.foreground)
            .max_w(px(240.))
            .truncate()
            .child(self.0.clone())
    }
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
        let rename_input = cx.new(|cx| InputState::new(window, cx).placeholder("Session title"));
        let rename_sub = cx.subscribe(
            &rename_input,
            |this, _, event: &InputEvent, cx| match event {
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
            },
        );
        let dock_sizes = cx.new(|_| ResizableState::default());
        let session_dock_sizes = cx.new(|_| ResizableState::default());
        let terminal_sizes = cx.new(|_| ResizableState::default());
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
            composer_append: None,
            composer_restore: None,
            tab_drafts: std::collections::HashMap::new(),
            staged: Vec::new(),
            notice: None,
            card: None,
            card_row: false,
            quit_dialog_open: false,
            quit_stopping: false,
            quit_confirmed: false,
            dock_open: true,
            dock_sizes,
            last_viewport_w: None,
            drawer: None,
            session_panel_open: false,
            files_panel_session: None,
            tab_terminals: std::collections::HashMap::new(),
            tab_terminal_open: std::collections::HashSet::new(),
            terminal_sizes,
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
            provider_form: None,
            provider_form_subs: Vec::new(),
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
            agent_delete_confirm: None,
            skill_delete_confirm: None,
            mcp_delete_confirm: None,
            model_delete_confirm: None,
            server_remove_armed: None,
            file_md_source: None,
            update_confirm: false,
            quota_confirm: None,
            copied_row: None,
            plan_open: false,
            thought_full: std::collections::HashSet::new(),
            tool_run_open: std::collections::HashMap::new(),
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
    /// window may close now; with turns THIS app started still running it
    /// opens the quit dialog first (and returns false — a dialog button
    /// does the close). Turns other clients started are never ours to
    /// stop, so they never gate quitting: nothing of ours running means
    /// the window closes with no dialog at all.
    pub fn request_quit(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.quit_confirmed {
            return true;
        }
        if self.quit_dialog_open || self.quit_stopping {
            return false;
        }
        if self.snap.own_running_session_count == 0 {
            return true;
        }
        self.quit_dialog_open = true;
        cx.notify();
        false
    }

    /// "Stop Turns & Quit" from the quit dialog: `session/cancel` on every
    /// own running turn, then a short flush window before the window
    /// actually closes — the cancels are fire-and-forget notifications, and
    /// removing the window at once could exit the process before the
    /// sockets write them out.
    fn stop_own_turns_and_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.runtime.send(UiCommand::CancelOwnTurns);
        self.quit_dialog_open = false;
        self.quit_stopping = true;
        cx.notify();
        let handle = window.window_handle();
        let flush = cx
            .background_executor()
            .timer(Duration::from_millis(QUIT_FLUSH_MS));
        cx.spawn(async move |this, cx| {
            flush.await;
            if let Some(entity) = this.upgrade() {
                let _ = cx.update_entity(&entity, |wb, cx| {
                    wb.quit_stopping = false;
                    wb.quit_confirmed = true;
                    cx.notify();
                });
            }
            // The confirmed close: the last window closing quits the app
            // (QuitMode::LastWindowClosed, main.rs) — the same outcome ⌘Q
            // promised.
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        })
        .detach();
    }

    fn on_quit_action(&mut self, _: &QuitApp, window: &mut Window, cx: &mut Context<Self>) {
        self.request_quit(window, cx);
    }

    // -- dock & shortcuts ----------------------------------------------------

    /// Rail toggle for the sessions dock: the rail icon collapses an open
    /// dock and reopens a collapsed one.
    fn toggle_dock(&mut self, cx: &mut Context<Self>) {
        self.dock_open = !self.dock_open;
        cx.notify();
    }

    // -- embedded terminal ---------------------------------------------------

    /// View-local toggle (the drawers' shape): the dock spawns lazily on
    /// the first open — in the ACTIVE tab's session workspace — and survives
    /// hide/show and tab switches; reopening never respawns a live shell.
    /// Each tab's terminal is its own (a shell in tab A never follows the
    /// switch to tab B); with no active tab there is nothing to toggle.
    /// Closing returns the keyboard to the composer when a shell view held
    /// it: a hidden handle KEEPS focus in gpui, and keys dispatched from a
    /// focus that is no longer in the rendered frame silently go nowhere —
    /// ⌃` itself would stop reopening the panel.
    pub(crate) fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.snap.active_tab else {
            return;
        };
        if self.tab_terminal_open.contains(&id) {
            self.tab_terminal_open.remove(&id);
            let focused_terminal = self.tab_terminals.values().flatten().any(|dock| {
                window.focused(cx) == Some(dock.focus.clone())
            });
            if focused_terminal {
                let composer = self.composer.read(cx).focus_handle(cx).clone();
                window.focus(&composer, cx);
            }
        } else {
            self.tab_terminal_open.insert(id);
            let no_panes = self
                .tab_terminals
                .get(&id)
                .is_none_or(|panes| panes.is_empty());
            if no_panes && !cfg!(test) {
                self.spawn_terminal(cx);
            }
        }
        cx.notify();
    }

    /// The split button: append a pane to the ACTIVE tab's row (fresh shell
    /// in the same session workspace), up to the cap. Unlike the rail
    /// toggle this spawns under test too — pane logic stays testable; the
    /// placeholder never paints once a pane exists.
    pub(crate) fn split_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.snap.active_tab else {
            return;
        };
        if self
            .tab_terminals
            .get(&id)
            .is_some_and(|panes| panes.len() >= terminal_panel::MAX_TERMINAL_PANES)
        {
            return;
        }
        let cwd = self.active_workspace();
        let dock = terminal_panel::spawn_dock(cwd.as_deref(), cx.entity().downgrade(), cx);
        match dock {
            Ok(dock) => {
                let focus = dock.focus.clone();
                self.tab_terminals.entry(id).or_default().push(dock);
                self.tab_terminal_open.insert(id);
                // The new pane takes the keyboard — it is what the click
                // asked for.
                window.focus(&focus, cx);
                cx.notify();
            }
            Err(e) => self.notice = Some(format!("{}: {e}", t!("terminal.spawnFailed"))),
        }
    }

    /// One pane leaves — the shell-exit callback's deferred close AND the
    /// pane's × button (same outcome: that shell goes away). `exited` is
    /// the view that owns the shell — it may be ANY tab's pane (a parked
    /// tab's shell dying while hidden), and a shell the restart already
    /// replaced still lives in the previous frame's element tree for one
    /// draw, its late exit closing nothing. The LAST pane takes the panel
    /// with it (the old single-dock behavior).
    pub(crate) fn close_terminal(&mut self, exited: gpui::EntityId, cx: &mut Context<Self>) {
        let mut hit = None;
        for (id, panes) in self.tab_terminals.iter() {
            if let Some(ix) = panes.iter().position(|d| d.view.entity_id() == exited) {
                hit = Some((*id, ix));
                break;
            }
        }
        let Some((id, ix)) = hit else {
            return;
        };
        let emptied = self.tab_terminals.get_mut(&id).is_some_and(|panes| {
            if ix < panes.len() {
                panes.remove(ix);
            }
            panes.is_empty()
        });
        if emptied {
            self.tab_terminals.remove(&id);
            self.tab_terminal_open.remove(&id);
        }
        cx.notify();
    }

    pub(crate) fn on_toggle_terminal(
        &mut self,
        _: &ToggleTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_terminal(window, cx);
        // Opening hands the keyboard to the shell at once; closing has
        // already returned the focus to the composer (toggle_terminal).
        if let Some(id) = self.snap.active_tab {
            if let Some(dock) = self
                .tab_terminals
                .get(&id)
                .filter(|_| self.tab_terminal_open.contains(&id))
                .and_then(|panes| panes.last())
            {
                window.focus(&dock.focus, cx);
            }
        }
    }

    /// Kill ONE pane's shell and spawn a fresh one in its place (same
    /// index, CURRENT session's workspace) — the point of the pane's
    /// restart button (a `cd` elsewhere in the old shell is not undone, it
    /// is abandoned).
    pub(crate) fn restart_terminal(
        &mut self,
        pane: gpui::EntityId,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.snap.active_tab else {
            return;
        };
        let Some(ix) = self
            .tab_terminals
            .get(&id)
            .and_then(|panes| panes.iter().position(|d| d.view.entity_id() == pane))
        else {
            return;
        };
        let cwd = self.active_workspace();
        let dock = terminal_panel::spawn_dock(cwd.as_deref(), cx.entity().downgrade(), cx);
        match dock {
            Ok(dock) => {
                if let Some(panes) = self.tab_terminals.get_mut(&id) {
                    if ix < panes.len() {
                        // Replace in place: the assignment drops the old
                        // dock, which kills its shell.
                        panes[ix] = dock;
                    }
                }
                cx.notify();
            }
            Err(e) => self.notice = Some(format!("{}: {e}", t!("terminal.spawnFailed"))),
        }
    }

    fn spawn_terminal(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.snap.active_tab else {
            return;
        };
        let cwd = self.active_workspace();
        let dock = terminal_panel::spawn_dock(cwd.as_deref(), cx.entity().downgrade(), cx);
        match dock {
            Ok(dock) => {
                self.tab_terminals.entry(id).or_default().push(dock);
            }
            Err(e) => self.notice = Some(format!("{}: {e}", t!("terminal.spawnFailed"))),
        }
    }

    /// The active tab's workspace path — the shell's cwd (the tab strip's
    /// project identity, TS parity).
    fn active_workspace(&self) -> Option<String> {
        let active = self.snap.active_tab?;
        self.snap
            .tabs
            .iter()
            .find(|t| t.id == active)?
            .workspace
            .clone()
    }

    /// Whether the embedded terminal owns the keyboard right now. The
    /// open-flag gate matters on HIDE: gpui keeps a hidden handle's
    /// focus until something else takes it, so without the gate Esc would
    /// keep yielding to an invisible shell — the whole app's Esc ladder
    /// (sheet, drawer, cancel turn) dead until the next click.
    fn terminal_focused(&self, window: &Window, cx: &App) -> bool {
        let Some(id) = self.snap.active_tab else {
            return false;
        };
        self.tab_terminal_open.contains(&id)
            && self.tab_terminals.get(&id).is_some_and(|panes| {
                panes
                    .iter()
                    .any(|dock| window.focused(cx) == Some(dock.focus.clone()))
            })
    }

    /// Rail toggle for the overlay drawer: one slot, so opening a surface
    /// closes the other. Both surfaces are view-local, but each seeds its
    /// reads on open — both paint the quota card state, so the open kicks
    /// its read (TS QuotaSection's mount effect); settings additionally
    /// seeds the workflow gate probe (the gated entry cannot render without
    /// the verdict; unprobed is fail-closed).
    fn toggle_drawer(&mut self, surface: DrawerSurface, cx: &mut Context<Self>) {
        let opening = self.drawer != Some(surface);
        self.drawer = opening.then_some(surface);
        if opening {
            self.runtime.send(UiCommand::RefreshQuota);
            if surface == DrawerSurface::Settings && self.snap.workflow_gate.is_none() {
                self.runtime.send(UiCommand::ProbeWorkflowGate);
            }
        }
        cx.notify();
    }

    /// The right session panel's toggle (TS ChatScreen's panel button).
    /// Opening seeds the file listing — the browser's mount effect: without
    /// a read the panel would show an empty body where the bridge
    /// advertises the capability.
    pub(crate) fn toggle_session_panel(&mut self, cx: &mut Context<Self>) {
        self.session_panel_open = !self.session_panel_open;
        if self.session_panel_open {
            self.seed_files_panel();
        }
        cx.notify();
    }

    /// Read the active session's directory into the panel unless the listing
    /// on screen already belongs to it: a reopen keeps the directory the
    /// user browsed to (the listing survives a panel close), while a switch
    /// to another session starts back at that session's root.
    fn seed_files_panel(&mut self) {
        let Some(session) = self.snap.active.as_ref().and_then(|t| t.session_id.clone()) else {
            // Nothing to browse — the panel paints its no-session note.
            self.files_panel_session = None;
            return;
        };
        if !self.tab.as_ref().is_some_and(|t| t.fs_capable) {
            self.files_panel_session = Some(session);
            return;
        }
        let same = self.files_panel_session.as_deref() == Some(session.as_str());
        self.files_panel_session = Some(session);
        if same && self.snap.files.listing.is_some() {
            return;
        }
        let path = if same {
            self.snap.files.path.clone()
        } else {
            String::new()
        };
        self.runtime.send(UiCommand::FilesNavigate { path });
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
    /// action has nothing to focus there; the overlay drawer replaces
    /// nothing — search works underneath it.
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
    /// replaced. The overlay drawer is NOT here — the drawer branch in
    /// on_stop_turn owns it (it replaces no surface).
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

    /// Esc: close the topmost layer first — the quit dialog swallows it
    /// (dismiss, keep the window; a flushing stop-all is NOT cancellable),
    /// then the lightbox (K2), the rename sheet, the new-session sheet (the
    /// TS back-handler stack, `creating` guard included via the command's
    /// no-op-while-busy); then an open config page closes (the settings
    /// screen's back); the overlay drawer before it (a drawer above a page
    /// is the closer layer, though opening a page normally closes the
    /// drawer already); otherwise it is ChatView's Cancel — `session/cancel`
    /// on the running turn. (The "/" menu's Escape never reaches here: the
    /// keystroke interceptor consumes it before the global binding.)
    pub(crate) fn on_stop_turn(
        &mut self,
        _: &StopTurn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The embedded terminal owns the keyboard while focused: Esc is the
        // shell's (vim's mode switch), never this app's unwind ladder.
        if self.terminal_focused(window, cx) {
            return;
        }
        if self.quit_dialog_open {
            // Esc on the quit dialog keeps the window — modal dismiss only
            // (a flushing stop-all below is NOT cancellable).
            self.quit_dialog_open = false;
        } else if self.lightbox.take().is_some() {
            // View-local — closing it is purely ours.
        } else if self.rename_target.take().is_some() {
            // The rename sheet is view-local — closing it is purely ours.
        } else if self.snap.new_session.is_some() {
            self.runtime.send(UiCommand::NewSessionToggle);
        } else if self.snap.history.open {
            // The history dialog rides above the chooser, below the file
            // viewer (mount order in render).
            self.runtime.send(UiCommand::HistoryToggle);
        } else if self.snap.files.viewing.is_some() {
            // The file viewer is the topmost layer (mounted after every
            // page) — Esc unwinds it before anything it covers.
            self.runtime.send(UiCommand::FilesCloseView);
        } else if self.drawer.take().is_some() {
            // The drawer is view-local — closing it is purely ours.
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
                // The composer is bound to the ACTIVE session: a switch parks
                // the outgoing tab's draft and adopts the incoming one's
                // during the next render. Without this, text typed for one
                // conversation rides into whichever session the switch lands
                // on — the send path targets the active tab.
                if self.snap.active_tab != snap.active_tab {
                    if let Some(old) = self.snap.active_tab {
                        // A clear-on-send still pending means the text
                        // already left (the clear lags a frame) — park empty.
                        let text = if self.clear_composer {
                            String::new()
                        } else {
                            self.composer.read(cx).value().to_string()
                        };
                        let draft = (text, std::mem::take(&mut self.staged));
                        self.tab_drafts.insert(old, draft);
                    }
                    // Also on the switch-to-nothing (last tab closed): the
                    // composer must not keep the closed session's text.
                    self.composer_restore = Some(
                        snap.active_tab
                            .and_then(|new| self.tab_drafts.remove(&new))
                            .unwrap_or_default(),
                    );
                }
                self.snap = snap;
                // Closed tabs' parked drafts go with them — the decoded
                // rasters they hold are not small — and their terminals
                // too: dropping the dock kills the shell.
                let live: std::collections::HashSet<u64> =
                    self.snap.tabs.iter().map(|t| t.id).collect();
                self.tab_drafts.retain(|id, _| live.contains(id));
                self.tab_terminals.retain(|id, _| live.contains(id));
                self.tab_terminal_open.retain(|id| live.contains(id));
                // Full config pages replace the drawer's navigation context —
                // their entries live IN the drawer, so the page taking over
                // below closes it. The drawer itself is view-local, so
                // closing is purely ours.
                let full_page_open = self.snap.system_status.is_some()
                    || self.snap.models_page.is_some()
                    || self.snap.quota_page.is_some()
                    || self.snap.usage_page.is_some()
                    || self.snap.config_section_page.is_some()
                    || self.snap.workflows_page.is_some();
                if full_page_open {
                    self.drawer = None;
                }
                // The quit dialog's reason can expire under it: the last own
                // turn settled while the user was deciding. Drop the modal —
                // there is nothing left to choose. (A flushing stop-all is
                // keyed on quit_stopping and rides through.)
                if self.quit_dialog_open && self.snap.own_running_session_count == 0 {
                    self.quit_dialog_open = false;
                }
                // The form lives only inside the page (TS ConfigFormSheet
                // unmounts with it); a page closed under an open form drops
                // it so a reopen starts clean.
                if self.snap.models_page.is_none() && self.models_form.is_some() {
                    self.models_form = None;
                    self.models_form_subs.clear();
                }
                if self.snap.config_section_page.is_none() {
                    // Same contract for the section form, plus the page-local
                    // gestures (the hooks fold, the armed confirms).
                    self.zconfig_form = None;
                    self.zconfig_form_subs.clear();
                    self.hooks_open_event = None;
                    self.backup_confirm = None;
                    self.skill_delete_confirm = None;
                    self.mcp_delete_confirm = None;
                    self.agent_delete_confirm = None;
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
                // The files panel belongs to the session it was seeded for:
                // a session switch under the open panel re-reads the new
                // session's root (the TS FileBrowser's mount effect); with
                // no session left there is nothing to browse, so the panel
                // closes.
                if self.session_panel_open {
                    let panel_session =
                        self.snap.active.as_ref().and_then(|t| t.session_id.clone());
                    if panel_session != self.files_panel_session {
                        self.session_panel_open = panel_session.is_some();
                        self.seed_files_panel();
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
        let key = self
            .tab
            .as_ref()
            .and_then(|t| t.approval.as_ref())
            .map(|a| (a.request_id(), matches!(a, PendingAsk::Elicitation(_))));
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
        let prepare = cx.background_executor().spawn(async move {
            inputs
                .into_iter()
                .map(|i| (prepare_image(&i.bytes, &i.mime), i.mime))
                .collect::<Vec<_>>()
        });
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
                    self.notice = Some(format!("{mime} is too large (over 5 MB passthrough cap)"));
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
            thought: true,
            fold,
            ..
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

    /// Pin one aggregated tool-run card open or shut. Records the MANUAL
    /// verdict only — from here on the run stops following its own state
    /// (the TS ToolRunCard's `manual`).
    fn set_tool_run_open(
        &mut self,
        row_id: &str,
        start_index: usize,
        open_now: bool,
        cx: &mut Context<Self>,
    ) {
        self.tool_run_open
            .insert((row_id.to_string(), start_index), open_now);
        cx.notify();
    }

    /// A row copy landed (E4 — ActionBarPrimitive.Copy): hold the row's
    /// checkmark for a beat, then clear it. The clipboard write itself
    /// happens at the click site (`App::write_to_clipboard`).
    fn mark_copied(&mut self, row_id: &str, cx: &mut Context<Self>) {
        self.copied_row = Some(row_id.to_string());
        cx.notify();
        let row_id = row_id.to_string();
        let clear = cx.background_executor().timer(Duration::from_millis(1400));
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
        // note. The hub address left the bar — it lives in the settings
        // drawer's server book, where it can be acted on.
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
                            .text_size(ui_text(11.))
                            .text_color(theme.foreground)
                            .child(bridge.unwrap_or_else(|| label.to_string())),
                    )
                    .when_some(activity, |this, text| {
                        this.child(
                            div()
                                .id("status-activity")
                                .test_support()
                                .flex_none()
                                .text_size(ui_text(11.))
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
                                .text_size(ui_text(11.))
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
                    .text_size(ui_text(11.))
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
                    .text_size(ui_text(11.))
                    .font_family(theme.mono_font_family.clone())
                    .text_color(theme.muted_foreground)
                    .child(dock),
            );
        }
        bar = bar.child(right);
        bar.into_any_element()
    }

    // -- dock ----------------------------------------------------------------

    /// The left dock (ADR-0009's dock-style window): a rail of toggles plus
    /// the docked sessions panel, resizable against the chat area through
    /// the splitter (`ResizableState` keeps the dragged width across
    /// collapse/expand and config-page round trips). Collapsed, the rail
    /// alone remains. Settings and quota are NOT docked panels — they open
    /// the right overlay drawer (`Self::render_drawer`), leaving this layout
    /// untouched underneath.
    fn render_dock(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();

        // The rail: the sessions dock toggle, then the two drawer toggles.
        let mut rail = div()
            .w(px(RAIL_WIDTH))
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
            "sessions",
            KitIconName::MessageSquare,
            "Sessions — live sessions, history, and the ⌘K search",
            cx,
        ));
        rail = rail.child(self.render_rail_drawer(
            DrawerSurface::Settings,
            IconName::Settings2,
            "Settings — servers, language, models, usage, system status",
            cx,
        ));
        rail = rail.child(self.render_rail_drawer(
            DrawerSurface::Quota,
            KitIconName::Gauge,
            "Plan quota — allowance windows and reset cards",
            cx,
        ));
        // The terminal's visible door: the ⌃` binding alone left the dock
        // undiscoverable. Painted as selected while the ACTIVE tab's
        // terminal is open (the dock is per-tab, the door is one slot).
        rail = rail.child(self.render_rail_terminal(cx));

        if !self.dock_open {
            // Collapsed: the rail plus the chat area at full width.
            return div()
                .flex()
                .flex_1()
                .min_h_0()
                // min_w_0: without it the row lays out at its min-content
                // (a wide dock + the chat floor) instead of the window —
                // the whole point of the shrink bug.
                .min_w_0()
                .child(rail)
                .child(self.render_chat_area(cx))
                .into_any_element();
        }
        let panel = self.render_sidebar(cx);
        div()
            .flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(rail)
            .child(
                div().flex().flex_1().min_w_0().min_h_0().child(
                    h_resizable("dock-split")
                        .with_state(&self.dock_sizes)
                        .child(
                            resizable_panel()
                                .size(px(280.))
                                .size_range(px(200.)..px(520.))
                                // flex_none (shrink 0) kept the dragged
                                // width while the window shrank, pushing
                                // the chat column past the right edge
                                // (2026-10-21): the dock still never
                                // GROWS, but under pressure it yields
                                // down to its 200px floor and the chat
                                // keeps a usable width.
                                .flex_grow_0()
                                .flex_shrink_1()
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
                            // Same pressure rule as the dock split's panel.
                            .flex_grow_0()
                            .flex_shrink_1()
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
        // The chat stack (everything above the terminal dock): tab strip,
        // plan bar, scroller, toast, composer — the v_resizable split's top
        // panel while the terminal is open.
        let chat_stack = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            // The hard width guarantee (2026-10-21): narrow windows CLIP the
            // stack's wide min-contents instead of pushing the column past
            // the window's right edge.
            .overflow_hidden()
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
            .child(self.render_composer(cx));
        // The active tab's terminal dock: per-tab open flag, per-tab shell
        // (a dock exists while hidden too — the flag only paints).
        let open_terminal = self
            .snap
            .active_tab
            .filter(|id| self.tab_terminal_open.contains(id));
        if open_terminal.is_none() {
            // The plain column: the stack fills it as before.
            return div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .overflow_hidden()
                .child(chat_stack)
                .into_any_element();
        }
        // The terminal dock: a v_resizable split under the chat stack (the
        // left dock's h_resizable pattern turned vertical). Sizes are
        // view-local and reset on a viewport-width change like the others.
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(
                v_resizable("terminal-split")
                    .with_state(&self.terminal_sizes)
                    .child(
                        resizable_panel()
                            .size(px(420.))
                            .size_range(px(160.)..px(6000.))
                            .flex_shrink_1()
                            .child(chat_stack),
                    )
                    .child(
                        resizable_panel()
                            .size(px(240.))
                            .size_range(px(80.)..px(2000.))
                            .child(terminal_panel::render_terminal_panel(
                                open_terminal
                                    .and_then(|id| self.tab_terminals.get(&id))
                                    .map(|panes| panes.as_slice()),
                                open_terminal,
                                self.active_workspace()
                                    .as_deref()
                                    .map(crate::file_browser::root_name)
                                    .as_deref(),
                                cx,
                            )),
                    ),
            )
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
                    .text_size(ui_text(15.))
                    .text_color(theme.foreground)
                    .child("Enable remote access"),
            )
            .child(
                div()
                    .text_size(ui_text(13.))
                    .text_color(theme.muted_foreground)
                    .child(
                        "No hub is running and the config has no access token. Confirming \
                         writes the token below into your zcode-acp config, then starts the hub.",
                    ),
            )
            .child(
                div()
                    .text_size(ui_text(13.))
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
                    .text_size(ui_text(12.))
                    .font_family(theme.mono_font_family.clone())
                    .text_color(theme.muted_foreground)
                    .child(preview.to_string()),
            );
        if let Some(err) = error {
            card = card.child(
                div()
                    .text_size(ui_text(13.))
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
        key: &'static str,
        icon: impl Into<gpui_kit::component::Icon> + Clone,
        tooltip: &str,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let wb = cx.entity();
        let active = self.dock_open;
        let color = if active {
            theme.foreground
        } else {
            theme.muted_foreground
        };
        let label = tooltip.to_string();
        div()
            .id(SharedString::from(format!("dock-rail-{key}")))
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
                wb.update(cx, |wb, cx| wb.toggle_dock(cx));
            })
    }

    /// A rail toggle for the ACTIVE tab's terminal dock —
    /// `render_rail_button`'s shape with the terminal's own state and
    /// action (the dock is a chat-column surface, not the sessions dock).
    fn render_rail_terminal(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let wb = cx.entity();
        let active = self
            .snap
            .active_tab
            .is_some_and(|id| self.tab_terminal_open.contains(&id));
        let color = if active {
            theme.foreground
        } else {
            theme.muted_foreground
        };
        let label = t!("terminal.toggle").into_owned();
        div()
            .id("dock-rail-terminal")
            .flex()
            .items_center()
            .justify_center()
            .size_8()
            .rounded_md()
            .cursor_pointer()
            .when(active, |this| this.bg(theme.secondary))
            .hover(|this| this.bg(theme.secondary.opacity(0.6)))
            .child(Icon::new(KitIconName::Terminal).size_4().text_color(color))
            .tooltip(move |window, cx| Tooltip::new(label.clone()).build(window, cx))
            .test_support()
            .on_click(move |_, window, cx| {
                wb.update(cx, |wb, cx| wb.toggle_terminal(window, cx));
            })
    }

    /// A rail toggle that opens the overlay DRAWER rather than docking a
    /// panel: painted as selected while its surface is the open drawer (the
    /// drawer rides above this rail, so the toggle doubles as the close
    /// affordance).
    fn render_rail_drawer(
        &self,
        surface: DrawerSurface,
        icon: impl Into<gpui_kit::component::Icon> + Clone,
        tooltip: &str,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let wb = cx.entity();
        let active = self.drawer == Some(surface);
        let color = if active {
            theme.foreground
        } else {
            theme.muted_foreground
        };
        let label = tooltip.to_string();
        div()
            .id(SharedString::from(format!("dock-rail-{}", surface.key())))
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
                wb.update(cx, |wb, cx| wb.toggle_drawer(surface, cx));
            })
    }

    /// The Servers section of the settings drawer — the Connection Profile book
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
                            .text_size(ui_text(12.))
                            .text_color(theme.muted_foreground)
                            .child(t!("connect.savedServers").into_owned()),
                    )
                    .child(
                        div().ml_auto().child(
                            Button::new("server-add")
                                .ghost()
                                .xsmall()
                                .icon(IconName::Plus)
                                .tooltip(t!("connect.addServer").into_owned())
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
                            .text_size(ui_text(13.))
                            .text_color(theme.foreground)
                            .child("Local hub"),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(ui_text(12.))
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
                                .text_size(ui_text(13.))
                                .text_color(theme.foreground)
                                .child(server.name.clone()),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(ui_text(12.))
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
                            runtime.send(UiCommand::SwitchProfile {
                                id: Some(id.clone()),
                            })
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
            let remove =
                Button::new(SharedString::from(format!("server-remove-{}", server.id))).xsmall();
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
                    runtime_remove.send(UiCommand::RemoveServer {
                        id: remove_id.clone(),
                    });
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
                    .text_size(ui_text(12.))
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
    /// `saveFontSize`), publish the chrome factor, and repaint — every text
    /// role reads the ladder at paint time (chat px pairs direct, the rest
    /// through `ui_prefs::ui_text`), so one notify rescales the window.
    fn set_font_scale(&mut self, scale: crate::ui_prefs::FontScale, cx: &mut Context<Self>) {
        if self.font_scale == scale {
            return;
        }
        self.font_scale = scale;
        crate::ui_prefs::store_font_size(scale.key());
        crate::ui_prefs::set_current_ui_scale(scale);
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
                    .text_size(ui_text(13.))
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

    /// The settings drawer's body (formerly the config dock panel): the
    /// entry list for the high-frequency config pages (requirements §4 —
    /// the TS app reaches these from the settings panel), plus the server
    /// book, appearance picks, the hub upgrade check and the workflow gate
    /// switch. The account quota has its own rail toggle and drawer
    /// (`DrawerSurface::Quota`) plus a page entry in the list below, so it
    /// does not repeat here. The drawer frame (`render_drawer`) provides
    /// the chrome — header, close, border; scrolling lives here.
    fn render_config_panel(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let runtime = self.runtime.clone();
        // No caption row: the drawer frame's header already names the panel
        // (the old dock panel needed its own — it had no frame).
        let mut column = div().flex().flex_col().gap_2().p_2();

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
                        .text_size(ui_text(12.))
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
        let mut entries: Vec<(KitIconName, String, String, fn() -> UiCommand)> = vec![
            (
                KitIconName::Boxes,
                t!("zconfig.entryModels").into_owned(),
                t!("zconfig.entryModelsDesc").into_owned(),
                || UiCommand::OpenModels,
            ),
            (
                KitIconName::ChartColumn,
                t!("zconfig.entryUsage").into_owned(),
                t!("zconfig.entryUsageDesc").into_owned(),
                || UiCommand::OpenUsage,
            ),
            (
                KitIconName::Gauge,
                t!("zconfig.entryQuota").into_owned(),
                t!("zconfig.entryQuotaDesc").into_owned(),
                || UiCommand::OpenQuota,
            ),
            (
                KitIconName::Activity,
                t!("zconfig.entrySystem").into_owned(),
                t!("zconfig.entrySystemDesc").into_owned(),
                || UiCommand::OpenSystemStatus,
            ),
            (
                KitIconName::Puzzle,
                t!("zconfig.entrySkills").into_owned(),
                t!("zconfig.entrySkillsDesc").into_owned(),
                || UiCommand::OpenConfigPage {
                    page: crate::runtime::ConfigPageKind::Skills,
                },
            ),
            (
                KitIconName::PlugZap,
                t!("zconfig.entryMcp").into_owned(),
                t!("zconfig.entryMcpDesc").into_owned(),
                || UiCommand::OpenConfigPage {
                    page: crate::runtime::ConfigPageKind::Mcp,
                },
            ),
            (
                KitIconName::Webhook,
                t!("zconfig.entryHooks").into_owned(),
                t!("zconfig.entryHooksDesc").into_owned(),
                || UiCommand::OpenConfigPage {
                    page: crate::runtime::ConfigPageKind::Hooks,
                },
            ),
            (
                KitIconName::Bot,
                t!("zconfig.entryAgents").into_owned(),
                t!("zconfig.entryAgentsDesc").into_owned(),
                || UiCommand::OpenConfigPage {
                    page: crate::runtime::ConfigPageKind::Agents,
                },
            ),
            (
                KitIconName::ArchiveRestore,
                t!("zconfig.entryBackups").into_owned(),
                t!("zconfig.entryBackupsDesc").into_owned(),
                || UiCommand::OpenConfigPage {
                    page: crate::runtime::ConfigPageKind::Backups,
                },
            ),
            (
                KitIconName::Download,
                t!("zconfig.entryAppUpdate").into_owned(),
                t!("zconfig.entryAppUpdateDesc").into_owned(),
                || UiCommand::OpenConfigPage {
                    page: crate::runtime::ConfigPageKind::AppUpdate,
                },
            ),
        ];
        if self.snap.workflow_gate.as_ref().is_some_and(|g| g.enabled) {
            entries.push((
                KitIconName::Workflow,
                t!("zconfig.entryWorkflows").into_owned(),
                t!("zconfig.entryWorkflowsDesc").into_owned(),
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
                                    .text_size(ui_text(13.))
                                    .text_color(theme.foreground)
                                    .child(title.to_string()),
                            )
                            .child(
                                div()
                                    .text_size(ui_text(10.))
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
                                .text_size(ui_text(13.))
                                .text_color(theme.foreground)
                                .child(t!("zconfig.workflowGate").into_owned()),
                        )
                        .child(
                            div()
                                .text_size(ui_text(10.))
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

        div()
            .id("config-panel")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_y_scroll()
            .overflow_x_hidden()
            .child(column)
            .into_any_element()
    }

    // -- session panel (right) ----------------------------------------------

    /// The right panel IS the active session's file browser: a header strip
    /// (dotfiles / Finder / refresh) plus the breadcrumb and one directory
    /// level. Session config, turn stats, the workflow entry and the quota
    /// card all live elsewhere (composer chips, tab strip, quota drawer,
    /// status bar), so this panel carries nothing else.
    fn render_session_panel(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let wb_close = cx.entity();
        let files_ctx = FilesPanelCtx {
            runtime: self.runtime.clone(),
            wb: cx.entity(),
            show_hidden: self.files_show_hidden,
            hub_addr: self.snap.hub_addr.as_deref(),
            fs_capable: self.tab.as_ref().is_some_and(|t| t.fs_capable),
        };
        div()
            .id("session-panel")
            .test_support()
            .h_full()
            .w_full()
            .min_w_0()
            .flex_none()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(theme.border)
            .bg(theme.title_bar)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .h_9()
                    .px_2()
                    .flex_none()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(render_files_header(&self.snap.files, &files_ctx, cx))
                    .child(
                        Button::new("session-panel-close")
                            .ghost()
                            .xsmall()
                            .icon(KitIconName::X)
                            .tooltip("Close the files panel")
                            .on_click(move |_, _, cx| {
                                wb_close.update(cx, |this, cx| this.toggle_session_panel(cx));
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .when(
                        // No session, nothing to browse: the capability note
                        // would otherwise read as "bridge too old".
                        self.snap
                            .active
                            .as_ref()
                            .is_none_or(|t| t.session_id.is_none()),
                        |this| {
                            this.child(
                                div()
                                    .id("session-panel-empty")
                                    .test_support()
                                    .p_3()
                                    .text_size(ui_text(13.))
                                    .text_color(theme.muted_foreground)
                                    .child(t!("chat.noSession").into_owned()),
                            )
                        },
                    )
                    .when(
                        self.snap
                            .active
                            .as_ref()
                            .is_some_and(|t| t.session_id.is_some()),
                        |this| this.child(render_files_panel(&self.snap.files, &files_ctx, cx)),
                    ),
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
                        .text_size(ui_text(12.))
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
                // The heading is the project's LEAF name, not its path — a
                // deep workspace made the heading run past the panel's width
                // and ellipsize mid-directory (user feedback, 2026-10-21).
                // The full path stays one hover away.
                rows.push(
                    div()
                        .id(SharedString::from(format!(
                            "session-group-{}",
                            sess.workspace
                        )))
                        .test_support()
                        .mt_1()
                        .text_size(ui_text(10.))
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(if sess.workspace.is_empty() {
                            "(no workspace)".to_string()
                        } else {
                            project_name(&sess.workspace)
                        })
                        .tooltip({
                            let full = sess.workspace.clone();
                            move |window, cx| Tooltip::new(full.clone()).build(window, cx)
                        })
                        .into_any_element(),
                );
            }
            rows.push(self.render_session_row(sess, cx).into_any_element());
        }
        if rows.is_empty() {
            rows.push(
                div()
                    .text_size(ui_text(12.))
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

        // Session history lives in its own overlay dialog (the TS
        // ProjectHistoryDialog shape): inline, it drowned between the live
        // sessions AND shared this panel's single scroller — the sessions
        // scrolled away before any history row moved. The sidebar keeps
        // only the entry point.
        let runtime_hist = self.runtime.clone();
        column = column.child(
            div().mt_2().child(
                Button::new("history-open")
                    .ghost()
                    .xsmall()
                    .label(t!("historyDialog.title").into_owned())
                    .tooltip("Browse a project's past sessions")
                    .on_click(move |_, _, _| runtime_hist.send(UiCommand::HistoryToggle)),
            ),
        );

        div()
            .id("sessions-panel")
            .test_support()
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
                Button::new(SharedString::from(format!(
                    "session-menu-{}",
                    sess.session_id
                )))
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
                    m = m.item(PopupMenuItem::new("Rename…").on_click(move |_, _, cx| {
                        wb.update(cx, |wb, cx| {
                            wb.open_rename(inst.clone(), sid.clone(), prefill.clone(), cx)
                        });
                    }));
                    let (runtime, inst, sid) = (
                        menu_runtime.clone(),
                        menu_instance.clone(),
                        menu_session.clone(),
                    );
                    m = m.item(PopupMenuItem::new("End session").on_click(move |_, _, _| {
                        runtime.send(UiCommand::CloseSession {
                            instance_id: inst.clone(),
                            session_id: sid.clone(),
                        });
                    }));
                    m
                }),
            );
        // The hover carries the full title — the row ellipsizes it away. The
        // workspace path is deliberately OUT: the heading above already names
        // the project (its leaf), and a deep path made the tooltip wider than
        // the panel (user feedback, 2026-10-21).
        let hover_label = title.clone();
        div()
            .id(SharedString::from(format!(
                "session-row-{}",
                sess.session_id
            )))
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
                    .id(SharedString::from(format!(
                        "session-row-title-{}",
                        sess.session_id
                    )))
                    .test_support()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(ui_text(13.))
                    .text_color(theme.foreground)
                    .child(title)
                    .tooltip(move |window, cx| Tooltip::new(hover_label.clone()).build(window, cx)),
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
                        .text_size(ui_text(10.))
                        .text_color(theme.accent)
                        .child(t!("chat.activityFinished").into_owned()),
                )
            })
            .when(sess.open_tab.is_some(), |this| {
                this.child(
                    div()
                        .text_size(ui_text(10.))
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

    /// The session-history overlay dialog (TS ProjectHistoryDialog port): a
    /// centered card with its OWN scroller, opened from the sidebar entry.
    /// Mounted as a late root sibling — above the dock's sessions panel in
    /// paint order, so this scroller takes the wheel before the panel's
    /// does; the backdrop closes, the card swallows its own clicks.
    fn render_history_dialog(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let runtime = self.runtime.clone();

        // Header: back (a project is open), title/path, close.
        let selected = self.snap.history.selected.as_ref();
        let mut header = div()
            .flex()
            .items_center()
            .gap_1()
            .h_9()
            .px_2()
            .flex_none()
            .border_b_1()
            .border_color(theme.border);
        if selected.is_some() {
            let runtime_back = runtime.clone();
            header = header.child(
                Button::new("history-back")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ArrowLeft)
                    .tooltip("All projects")
                    .on_click(move |_, _, _| runtime_back.send(UiCommand::HistoryBack)),
            );
        }
        let title = match selected {
            Some(sel) => sel.workspace_path.clone(),
            None => t!("historyDialog.title").into_owned(),
        };
        header = header
            .child(
                div()
                    .id("history-title")
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .text_size(ui_text(13.))
                    .text_color(theme.foreground)
                    .child(title),
            )
            .child(
                Button::new("history-close")
                    .ghost()
                    .xsmall()
                    .icon(KitIconName::X)
                    .tooltip("Close the dialog")
                    .on_click(move |_, _, _| runtime.send(UiCommand::HistoryToggle)),
            );

        let card = div()
            .id("history-dialog")
            .test_support()
            .flex()
            .flex_col()
            .max_w(px(760.))
            .w(relative(0.8))
            .max_h(relative(0.8))
            .overflow_hidden()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.title_bar)
            .shadow_lg()
            // The card swallows clicks inside it — the backdrop click below
            // closes; neither may fall through to the surface beneath. The
            // wheel gets the same containment, and the card OCCLUDES: gpui's
            // scroll containers never stop propagation, so a wheel at the
            // list's end used to chain into the sessions panel behind. A
            // BlockMouse hitbox ends the hit test at itself, so every surface
            // behind reports should_handle_scroll() == false; the card's own
            // deeper hitboxes are collected first and keep scrolling.
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_scroll_wheel(|_, _, cx| {
                cx.stop_propagation();
            })
            .occlude()
            .child(header)
            .child(
                div()
                    .id("history-dialog-body")
                    .test_support()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .overflow_x_hidden()
                    .p_3()
                    .child(self.render_history_content(cx)),
            );

        let runtime_bg = self.runtime.clone();
        div()
            .id("history-backdrop")
            .test_support()
            .flex()
            .items_start()
            .justify_center()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .pt(px(48.))
            .bg(gpui::black().opacity(0.5))
            // Outside the card the backdrop swallows the wheel too: nothing
            // behind a modal answers a scroll through it (same reasoning as
            // the card; parent hitboxes are inserted before their children,
            // so the card's own scrollers stay live).
            .occlude()
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                cx.stop_propagation();
                runtime_bg.send(UiCommand::HistoryToggle);
            })
            .child(card)
            .into_any_element()
    }

    /// The history dialog's body column: the project chooser, or the
    /// selected project's paged session rows.
    fn render_history_content(&self, cx: &Context<Self>) -> gpui::Div {
        let theme = cx.theme();
        let mut column = div().flex().flex_col().gap_1();

        let Some(selected) = self.snap.history.selected.as_ref() else {
            // Project chooser.
            match (
                &self.snap.history.projects,
                &self.snap.history.projects_error,
            ) {
                (None, None) => {
                    column = column.child(
                        div()
                            .text_size(ui_text(12.))
                            .text_color(theme.muted_foreground)
                            .child(t!("historyDialog.loading").into_owned()),
                    );
                }
                (None, Some(err)) => {
                    column = column.child(
                        div()
                            .text_size(ui_text(12.))
                            .text_color(theme.danger)
                            .child(err.clone()),
                    );
                }
                (Some(list), _) => {
                    // The project-layer filter (TS ProjectHistoryDialog
                    // :413-421): case-insensitive path match, the same
                    // shape as the chooser's.
                    column =
                        column.child(div().px_1().child(Input::new(&self.history_filter).small()));
                    let q = self.history_filter.read(cx).value().trim().to_lowercase();
                    let shown: Vec<_> = list
                        .iter()
                        .filter(|p| q.is_empty() || p.workspace_path.to_lowercase().contains(&q))
                        .collect();
                    if shown.is_empty() {
                        column = column.child(
                            div()
                                .text_size(ui_text(12.))
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
                        let hover_path = workspace.clone();
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
                                        .text_size(ui_text(13.))
                                        .text_color(theme.foreground)
                                        .child(project_name(&p.workspace_path)),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .text_size(ui_text(10.))
                                        .text_color(theme.muted_foreground)
                                        // TS ProjectHistoryDialog:529 — count
                                        // plus fmtRelative(lastActive).
                                        .child(project_meta_line(p.sessions, p.last_active)),
                                )
                                // The chooser shows only the leaf name — the
                                // hover disambiguates same-named projects with
                                // the full workspace path.
                                .tooltip(move |window, cx| {
                                    Tooltip::new(hover_path.clone()).build(window, cx)
                                })
                                .on_click(move |_, _, _| {
                                    runtime.send(UiCommand::HistoryOpenProject {
                                        workspace_path: workspace.clone(),
                                    });
                                }),
                        );
                    }
                }
            }
            return column;
        };

        // The selected project's paged rows.
        self.render_history_list(selected, cx, column)
    }

    fn render_history_list(
        &self,
        selected: &HistorySelected,
        cx: &Context<Self>,
        mut column: gpui::Div,
    ) -> gpui::Div {
        let theme = cx.theme();
        if let Some(err) = &selected.error {
            column = column.child(
                div()
                    .text_size(ui_text(12.))
                    .text_color(theme.danger)
                    .child(err.clone()),
            );
        }
        if selected.loading_page {
            column = column.child(
                div()
                    .text_size(ui_text(12.))
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
                    .text_size(ui_text(10.))
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
                Button::new(SharedString::from(format!(
                    "history-menu-{}",
                    row.session_id
                )))
                .ghost()
                .xsmall()
                .icon(IconName::Ellipsis)
                .tooltip("Session actions")
                .dropdown_menu(move |menu, _, _| {
                    let mut m = menu;
                    let (runtime, sid) = (menu_runtime.clone(), menu_session.clone());
                    m = m.item(PopupMenuItem::new("Delete from history").on_click(
                        move |_, _, _| {
                            runtime.send(UiCommand::DeleteHistorySession {
                                session_id: sid.clone(),
                            });
                        },
                    ));
                    m
                }),
            );
        div()
            .id(SharedString::from(format!(
                "history-row-{}",
                row.session_id
            )))
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
                    .id(SharedString::from(format!(
                        "history-row-title-{}",
                        row.session_id
                    )))
                    .test_support()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(ui_text(13.))
                    .text_color(theme.foreground)
                    .child(title.clone())
                    // The row ellipsizes; the hover carries the full title.
                    .tooltip(move |window, cx| Tooltip::new(title.clone()).build(window, cx)),
            )
            .child(
                div()
                    .text_size(ui_text(10.))
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
            // A wide tab plus the trailing buttons floor ~275px of
            // min-content — clip instead of pushing the chat column past a
            // narrowed window.
            .min_w_0()
            .overflow_hidden()
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
                    .text_size(ui_text(13.))
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

        // The strip's drop tail: releasing a dragged tab past the last slot
        // (over the `+`'s trailing space) moves it to the END of the strip.
        let tail_runtime = self.runtime.clone();
        strip = strip.child(
            div()
                .id("tab-strip-drop-tail")
                .flex_none()
                .w_8()
                .h_full()
                .drag_over::<TabDragId>(|this, _, _, cx| this.rounded_md().bg(cx.theme().secondary))
                .on_drop(move |drag: &TabDragId, _, _| {
                    tail_runtime.send(UiCommand::MoveTab {
                        id: drag.0,
                        before_id: None,
                    });
                }),
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
        // The right side panel's toggle (TS ChatScreen's files button): the
        // panel is the session's file browser, so it needs a session.
        let wb_toggle = cx.entity();
        let toggle = Button::new("session-panel-toggle")
            .ghost()
            .xsmall()
            .icon(KitIconName::PanelRight)
            .selected(self.session_panel_open)
            .tooltip(t!("files.title").into_owned())
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
                        .text_size(ui_text(12.))
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
                                    .text_size(ui_text(12.))
                                    .text_color(color)
                                    .child(glyph),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .text_size(ui_text(13.))
                                    .when(status == Some("completed"), |this| {
                                        this.line_through().text_color(theme.muted_foreground)
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
            (Some(_), _) => tab.session_title.clone().unwrap_or_else(|| {
                short_id(tab.session_id.as_deref().unwrap_or("session"), "session")
            }),
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
        // Hover carries the FULL title — it ellipsizes on the strip — over
        // the project's LEAF name, not its path. A deep workspace path made
        // the tooltip a line of directories (user feedback, 2026-10-21); the
        // tab already shows that same leaf as its prefix.
        let hover_label = tab
            .workspace
            .as_deref()
            .filter(|w| !w.is_empty() && *w != "/")
            .map(|w| format!("{title} · {}", project_name(w)))
            .unwrap_or_else(|| title.clone());

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
            .text_size(ui_text(13.))
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

        // Drag-reorder (strip): press-and-move starts the drag — gpui
        // suppresses the click when one starts — and the dragged id rides
        // the drag payload. Releasing over a TAB inserts before it (the
        // accent left edge marks the slot); the strip's tail zone takes
        // the end slot.
        let drag_title = title.clone();
        tab_view = tab_view.on_drag(TabDragId(tab.id), move |_, _, _, cx| {
            cx.stop_propagation();
            cx.new(|_| TabDragPreview(drag_title.clone().into()))
        });
        let drop_runtime = self.runtime.clone();
        let target_id = tab.id;
        tab_view = tab_view
            .drag_over::<TabDragId>(|this, _, _, cx| {
                this.border_l_2().border_color(cx.theme().accent)
            })
            .on_drop(move |drag: &TabDragId, _, _| {
                drop_runtime.send(UiCommand::MoveTab {
                    id: drag.0,
                    before_id: Some(target_id),
                });
            });

        // Session Activity badges: running dot / awaiting ring / pending
        // count; a non-connected phase shows its state instead.
        match phase_label {
            Some(label) => {
                tab_view = tab_view.child(
                    div()
                        .text_size(ui_text(10.))
                        .text_color(theme.muted_foreground)
                        .child(label),
                );
            }
            None if matches!(tab.phase, TabPhase::Failed(_)) => {
                tab_view = tab_view.child(
                    div()
                        .text_size(ui_text(10.))
                        .text_color(theme.danger)
                        .child("error"),
                );
            }
            None if tab.running => {
                tab_view = tab_view.child(
                    div()
                        .size(px(6.))
                        .flex_none()
                        .rounded_full()
                        .bg(theme.accent),
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
                .flex()
                .items_center()
                .min_w_0()
                // A long session name must not stretch its tab unboundedly:
                // the cap bounds the pair so the ellipsis engages (and
                // crowded strips share the slack — the tab itself min_w_0s).
                .max_w(px(240.))
                // The CLI composer-cap shape, on a tab: the project name
                // first (a same-titled session in another repo is the
                // misread this must prevent), then the session title —
                // which yields all the slack to the ellipsis. A root-only
                // fallback workspace ("/") paints no prefix.
                .when_some(
                    tab.workspace
                        .as_deref()
                        .filter(|w| !w.is_empty() && *w != "/")
                        .map(project_name),
                    |this, project| {
                        this.child(
                            div()
                                .flex_none()
                                .max_w(px(120.))
                                .truncate()
                                .text_color(theme.muted_foreground)
                                .child(project),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_color(theme.muted_foreground)
                                .child(" · "),
                        )
                    },
                )
                .child(div().min_w_0().truncate().child(title))
                .tooltip(move |window, cx| Tooltip::new(hover_label.clone()).build(window, cx)),
        );
        if tab.pending > 0 {
            tab_view = tab_view.child(
                div()
                    .text_size(ui_text(10.))
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

        // Right-click (the tab's context menu): the standard trio. The
        // disabled flags are read at render and captured fresh — the menu
        // builder runs per open, and every open follows a render.
        let menu_runtime = self.runtime.clone();
        let has_others = self.snap.tabs.iter().any(|t| t.id != tab.id);
        let has_right = self
            .snap
            .tabs
            .iter()
            .position(|t| t.id == tab.id)
            .is_some_and(|at| at + 1 < self.snap.tabs.len());
        let menu_id = tab.id;
        let with_menu = tab_view.context_menu(move |menu, _, _| {
            let mut m = menu;
            let (runtime, id) = (menu_runtime.clone(), menu_id);
            m = m.item(
                PopupMenuItem::new("Close tab")
                    .on_click(move |_, _, _| runtime.send(UiCommand::CloseTab { id })),
            );
            let (runtime, id) = (menu_runtime.clone(), menu_id);
            m = m.item(
                PopupMenuItem::new("Close other tabs")
                    .disabled(!has_others)
                    .on_click(move |_, _, _| runtime.send(UiCommand::CloseOtherTabs { id })),
            );
            let (runtime, id) = (menu_runtime.clone(), menu_id);
            m = m.item(
                PopupMenuItem::new("Close tabs to the right")
                    .disabled(!has_right)
                    .on_click(move |_, _, _| runtime.send(UiCommand::CloseRightTabs { id })),
            );
            m
        });
        with_menu.into_any_element()
    }
    fn render_chat(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let mut column = div().flex().flex_col().flex_1().min_h_0().min_w_0();

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
                    Some(n) if n > 0 => fill(&t!("chat.loadEarlier"), &[("n", &n.to_string())]),
                    _ => t!("chat.loadEarlierGeneric").into_owned(),
                }
            };
            column = column.child(
                div().flex().justify_center().flex_none().py_2().child(
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
                            .text_size(ui_text(13.))
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
        let tool_run_open = self.tool_run_open.clone();
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
                            &tool_run_open,
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
                .min_h_0()
                // min_w_0: a flex item's automatic minimum is its content's
                // min-content width — one unwrappable row (long code line)
                // otherwise floors the WHOLE chat area and shrinking the
                // window just pushes it off-screen instead of reflowing.
                .min_w_0(),
            )
            .into_any_element()
    }

    fn render_composer(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        // flex_none: the message list shrinks, never the composer — the
        // input stays visible whatever the stream above is doing.
        // min_w_0: the column may be narrower than its widest row.
        let mut column = div().flex().flex_col().gap_1().flex_none().min_w_0();
        // The pending queue, one row per draft (ChatView's pending bubbles):
        // semi-transparent, text + image count, with the three affordances —
        // Send now (hidden while replaying/disconnected), Edit (back into
        // the composer), and discard.
        if let Some(t) = self.tab.as_ref() {
            // TS ChatView:932 — Send now needs connState "open" AND the
            // replay finished; the phase flips Connected before load_session
            // runs, so loading_session alone would leave a dead button up
            // through the whole replay (force_send refuses while loading).
            let connected = matches!(t.phase, TabPhase::Connected) && !t.loading_session;
            for (index, draft) in t.pending.iter().enumerate() {
                let runtime = self.runtime.clone();
                let mut row = div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(theme.secondary)
                    .opacity(0.85);
                row = row.child(
                    div()
                        .flex_none()
                        .text_size(ui_text(9.))
                        .text_color(theme.muted_foreground)
                        .child(t!("chat.pending").into_owned()),
                );
                row = row.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(ui_text(13.))
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
                            .text_size(ui_text(10.))
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
                        .on_click(move |_, _, _| runtime.send(UiCommand::DiscardPending { index })),
                );
                column = column.child(row);
            }
            if !t.pending.is_empty() {
                column = column.child(
                    div()
                        .text_size(ui_text(10.))
                        .text_color(theme.muted_foreground)
                        .child("queued — the next one sends when the running turn settles"),
                );
            }
        }
        if let Some(notice) = &self.notice {
            column = column.child(
                div()
                    .text_size(ui_text(12.))
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
                        .text_size(ui_text(9.))
                        .text_color(theme.muted_foreground)
                        .child("svg")
                        .into_any_element(),
                };
                strip = strip.child(
                    div().relative().flex_none().child(thumb).child(
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
                    .text_size(ui_text(10.))
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
        // The clip zeroes the row's min-content contribution: without it the
        // textarea's intrinsic width floors the composer, the composer floors
        // the chat panel, and a shrunken window leaves the chat at its old
        // width running past the right edge (2026-10-21). The buttons keep
        // their size; the textarea narrows and scrolls internally.
        let mut controls = div().flex().items_end().gap_2().min_w_0().overflow_hidden();
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
                            let _ = cx.update_entity(&wb, |wb, cx| wb.stage_inputs(inputs, cx));
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
                // min_w_0: the area's intrinsic width otherwise floors the
                // controls row (and with it the composer) in a narrow chat.
                .min_w_0()
                // Desktop paste: clipboard rasters and copied files ride
                // the same pipeline as picks (ChatView onPaste).
                .on_paste({
                    let wb = cx.entity();
                    move |item: &ClipboardItem, _window, cx| {
                        paste_inputs(item)
                            .map(|inputs| {
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
        let can_send = !self.staged.is_empty() || !self.composer.read(cx).value().trim().is_empty();
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
                    .text_size(ui_text(12.))
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
            // min_w_0 + clip: without them the composer's content floor
            // (chips, buttons) became the chat column's floor — a window
            // shrink pushed the whole composer past the right edge instead
            // of narrowing it.
            .min_w_0()
            .overflow_hidden()
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
            composer = composer.relative().child(
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
    let is_ws = |b: u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c');
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
fn slash_matches(
    token: &str,
    commands: &[protocol::acp::SlashCommand],
) -> Vec<protocol::acp::SlashCommand> {
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

/// A drop's split (the TS app bounces non-images; the desktop keeps them as
/// references): files that decode stage as image attachments, while a
/// non-image file or a directory becomes its path, appended to the composer
/// text so the prompt can point the agent at it — the drop always lands.
fn read_dropped_paths(paths: Vec<std::path::PathBuf>) -> (Vec<StageInput>, Option<String>) {
    let mut inputs = Vec::new();
    let mut refs = Vec::new();
    for path in paths {
        if path.is_dir() {
            refs.push(quoted_drop_path(&path));
            continue;
        }
        match std::fs::read(&path)
            .ok()
            .and_then(|bytes| sniff_mime(&bytes).map(|mime| (bytes, mime)))
        {
            Some((bytes, mime)) => inputs.push(StageInput {
                bytes,
                mime: mime.to_string(),
            }),
            None => refs.push(quoted_drop_path(&path)),
        }
    }
    let tail = (!refs.is_empty()).then(|| refs.join(" "));
    (inputs, tail)
}

/// The drop reference form: the absolute path verbatim, debug-quoted only
/// when it carries whitespace (a bare space would split the reference).
fn quoted_drop_path(path: &std::path::Path) -> String {
    let text = path.display().to_string();
    if text.chars().any(char::is_whitespace) {
        format!("{text:?}")
    } else {
        text
    }
}

#[cfg(test)]
mod drop_ref_tests {
    use super::read_dropped_paths;

    /// A dropped plain file, a directory, and an unreadable path never
    /// bounce: each lands in the composer tail as a path reference
    /// (whitespace-carrying paths quoted), and nothing stages as an image.
    #[test]
    fn non_images_and_directories_become_path_references() {
        let dir = std::env::temp_dir().join(format!("zcode-drop-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("notes.txt"), b"plain text").unwrap();
        std::fs::write(dir.join("with space.md"), b"markdown").unwrap();

        let (inputs, tail) = read_dropped_paths(vec![
            dir.join("notes.txt"),
            dir.join("nested"),
            dir.join("with space.md"),
            dir.join("missing.bin"),
        ]);
        assert!(inputs.is_empty(), "nothing here decodes as an image");
        let tail = tail.expect("the non-images land as references");
        assert!(tail.contains("notes.txt"), "tail: {tail}");
        assert!(tail.contains("nested"), "tail: {tail}");
        assert!(
            tail.contains("missing.bin"),
            "an unreadable file still references: {tail}"
        );
        assert!(
            tail.contains(&format!(
                "{:?}",
                dir.join("with space.md").display().to_string()
            )),
            "a whitespace path stays one quoted reference: {tail}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
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
            let _ = wb.update(cx, |this, cx| {
                this.open_lightbox(open_src.clone(), None, cx)
            });
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
                    let _ = wb.update(cx, |this, cx| this.stage_slash_select(&name, cx));
                }
            })
            .child(
                div()
                    .flex_none()
                    .text_size(ui_text(13.))
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
                    .text_size(ui_text(12.))
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
    tool_run_open: &std::collections::HashMap<(String, usize), bool>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    match row.role {
        ChatRole::User => render_user_row(row, copied, wb, chat_font, theme).into_any_element(),
        ChatRole::Assistant => render_assistant_row(
            row,
            is_last_row,
            running,
            copied,
            wb,
            chat_font,
            thought_full,
            tool_run_open,
            theme,
        )
        .into_any_element(),
    }
}

fn render_user_row(
    row: &RowModel,
    copied: Option<&str>,
    wb: &WeakEntity<Workbench>,
    chat_font: (f32, f32),
    theme: &component::Theme,
) -> AnyElement {
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
            .text_size(ui_text(12.))
            .text_color(theme.muted_foreground)
            .child(
                Icon::new(KitIconName::Bell)
                    .small()
                    .flex_none()
                    .text_color(theme.muted_foreground),
            )
            .child(
                div().min_w_0().truncate().child(
                    summary
                        .is_empty()
                        .then(|| t!("chat.taskNotice").into_owned())
                        .unwrap_or(summary),
                ),
            );
        if !status.is_empty() {
            chip = chip.child(
                div()
                    .flex_none()
                    .child(format!("· {}", status.to_uppercase())),
            );
        }
        return div()
            .flex()
            .min_w_0()
            .justify_center()
            .child(chip.test_support())
            .into_any_element();
    }
    if row.collapsed {
        // Replay harness fold (`_meta.zcode.collapsed`): the wall of context
        // text collapses to a chip, like ChatView's collapsed ids.
        return div()
            .flex()
            .gap_3()
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(theme.secondary)
                    .text_size(ui_text(12.))
                    .text_color(theme.muted_foreground)
                    .child(t!("chat.contextHandoff").into_owned()),
            )
            .into_any_element();
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
                    .text_size(ui_text(12.))
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
    // The user side hangs RIGHT in a tinted, bordered bubble; the assistant
    // side below stays full-width, borderless and unpainted. Alignment ALONE
    // was not enough (user feedback, 2026-10-21): a message long enough to
    // fill its 85% cap starts at almost the same x as the agent's row, so
    // the two read alike. The fill and the hairline edge differ on every
    // line, however wide the message gets — `secondary` alone (0x1e1e22) sits
    // too close to the canvas (0x0d0d0f) to carry that on its own, so the
    // accent tint and the border do the work.
    // (ChatView.tsx UserMessage `items-end max-w-[85%]` vs AssistantMessage.)
    let mut side = div()
        .flex()
        .flex_col()
        .items_end()
        .flex_none()
        .max_w(relative(0.85));
    side = side.child(
        div()
            .id(SharedString::from(format!("user-bubble-{}", row.id)))
            .test_support()
            .min_w_0()
            .px_3()
            .py_2()
            .rounded_lg()
            .bg(theme.accent.opacity(0.18))
            .border_1()
            .border_color(theme.accent.opacity(0.45))
            .child(bubble),
    );
    if !text.trim().is_empty() {
        side = side.child(copy_button(
            row,
            text.clone(),
            copied == Some(row.id.as_str()),
            wb,
        ));
    }
    div()
        .id(SharedString::from(format!("user-row-{}", row.id)))
        .test_support()
        .flex()
        .justify_end()
        .min_w_0()
        .child(side)
        .context_menu(chat_row_menu(text))
        .into_any_element()
}

fn render_assistant_row(
    row: &RowModel,
    is_last_row: bool,
    running: bool,
    copied: Option<&str>,
    wb: &WeakEntity<Workbench>,
    chat_font: (f32, f32),
    thought_full: &std::collections::HashSet<(String, usize)>,
    tool_run_open: &std::collections::HashMap<(String, usize), bool>,
    theme: &component::Theme,
) -> AnyElement {
    let mut column = div().flex().flex_col().gap_2().min_w_0();
    let last_part = row.parts.len().saturating_sub(1);
    let mut part_index = 0;
    while part_index < row.parts.len() {
        let part = &row.parts[part_index];
        // Adjacent tool calls with no text between them are one unit of agent
        // work. A long turn produces dozens of context-free cards (name +
        // output head, nothing to read them against), so the run folds into
        // ONE collapsed summary card — the TS `group-tools` case. A lone call
        // keeps its own card: a one-item wrapper would be pure chrome.
        if matches!(part, PartView::ToolCall { .. }) {
            let start = part_index;
            while part_index < row.parts.len()
                && matches!(row.parts[part_index], PartView::ToolCall { .. })
            {
                part_index += 1;
            }
            let run = &row.parts[start..part_index];
            column = column.child(if run.len() > 1 {
                // Absent = never toggled: a live run stays open so its calls
                // are watchable, a settled one folds away. While the turn is
                // running, the live row's TAIL run stays latched open as
                // well: the desktop renders every protocol update on its
                // own, so in the gap between one call's result and the next
                // call's request "any call in flight" briefly reads false
                // and the card flaps shut then reopens (the mobile app
                // batches those updates and never shows the gap).
                let live_tail = running && is_last_row && part_index == row.parts.len();
                let open = tool_run_open
                    .get(&(row.id.clone(), start))
                    .copied()
                    .unwrap_or_else(|| tool_run_summary(run).1 || live_tail);
                render_tool_run(&row.id, start, run, open, wb, theme)
            } else {
                render_tool_card(&row.id, start, &run[0], wb, theme)
            });
            continue;
        }
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
                                .text_size(ui_text(12.))
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
            PartView::Markdown { state, .. } => {
                column.child(div().text_size(px(chat_font.1)).child(TextView::new(state)))
            }
            PartView::Plain { text } => column.child(
                div()
                    .text_size(px(chat_font.1))
                    .line_height(relative(1.55))
                    .text_color(theme.foreground)
                    .child(text.clone()),
            ),
            // Tool calls are handled by the run branch above.
            PartView::ToolCall { .. } => column,
            PartView::ImageStub => column.child(
                div()
                    .text_size(ui_text(12.))
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
        part_index += 1;
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
        .id(SharedString::from(format!("agent-row-{}", row.id)))
        .test_support()
        .flex()
        .gap_3()
        .child(div().flex_1().min_w_0().child(column))
        .when(!text.trim().is_empty(), |this| {
            this.child(copy_button(
                row,
                text.clone(),
                copied == Some(row.id.as_str()),
                wb,
            ))
        })
        .context_menu(chat_row_menu(text))
        .into_any_element()
}

/// One tool call's card (the TS ToolCard `<details>`): the summary row
/// (glyph, name, detail head) is the toggle; the body is the FULL detail in
/// a capped, scrollable window. Replay folds (`context-handoff` /
/// `tool-transcript`) keep the summary clean — a handoff shows the localized
/// label (the wire title is English-only plumbing), the body behind expand.
fn render_tool_card(
    row_id: &str,
    part_index: usize,
    view: &PartView,
    wb: &WeakEntity<Workbench>,
    theme: &component::Theme,
) -> AnyElement {
    let PartView::ToolCall {
        label,
        status,
        detail_head,
        detail,
        fold_kind,
        expanded,
        diff_src: _,
        diffs,
    } = view
    else {
        return div().into_any_element();
    };
    let done = status == "completed";
    let glyph = if done { "✓" } else { "…" };
    let color = if done {
        theme.muted_foreground
    } else if status == "failed" {
        theme.danger
    } else {
        theme.accent
    };
    // The wire title is the raw command: a heredoc/`&&` chain carries real
    // newlines, which rendered as-is turned the one-line summary into a wall.
    let display: SharedString = match fold_kind.as_deref() {
        Some("context-handoff") => t!("chat.contextHandoff").into_owned().into(),
        _ => tool_display_name(label).into(),
    };
    let open_now = *expanded;
    let row_id_owned = row_id.to_string();
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
            .id(SharedString::from(format!(
                "tool-{}-{}",
                row_id, part_index
            )))
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .cursor_pointer()
            .overflow_hidden()
            .hover(|h| h.bg(theme.title_bar))
            .on_click({
                let wb = wb.clone();
                move |_, _window, cx| {
                    if let Some(wb) = wb.upgrade() {
                        wb.update(cx, |wb, cx| {
                            wb.set_tool_expanded(&row_id_owned, part_index, open_now, cx)
                        });
                    }
                }
            })
            .child(
                div()
                    .flex_none()
                    .font_family(theme.mono_font_family.clone())
                    .text_size(ui_text(12.))
                    .text_color(color)
                    .child(glyph),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .font_family(theme.mono_font_family.clone())
                    .text_size(ui_text(12.))
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
                        .text_size(ui_text(12.))
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
        // TS ToolCard body order: the diff blocks first, then the plain
        // detail under them (ChatView.tsx DiffBlock → detail).
        let mut body = div()
            .id(SharedString::from(format!(
                "tool-detail-{}-{}",
                row_id, part_index
            )))
            .max_h(px(256.))
            .overflow_y_scroll()
            .min_w_0()
            .border_t_1()
            .border_color(theme.border)
            .font_family(theme.mono_font_family.clone())
            .text_size(ui_text(12.))
            .line_height(relative(1.45));
        for (bi, block) in diffs.iter().enumerate() {
            body = body.child(render_diff_block(block, row_id, part_index, bi, theme));
        }
        body = body.child(
            div()
                .px_2()
                .py(px(6.))
                .text_color(theme.muted_foreground)
                .whitespace_normal()
                .child(if detail.is_empty() {
                    "…".to_string()
                } else {
                    detail.clone()
                }),
        );
        card = card.child(body.test_support());
    }
    // Right-click on the card (run members included): copy the raw command
    // line, or the full output behind the fold. The right-mouse stop keeps
    // the enclosing run/row menus out — bubble dispatch is reverse paint
    // order, so this card's own menu listener has already run.
    let menu_command = label.to_string();
    let menu_output = detail.clone();
    card.id(SharedString::from(format!(
        "tool-card-{}-{}",
        row_id, part_index
    )))
    .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
    .context_menu(move |menu, _, _| {
        let mut m = menu;
        let command = menu_command.clone();
        m = m.item(
            PopupMenuItem::new("Copy command").on_click(move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(command.clone()));
            }),
        );
        let empty = menu_output.trim().is_empty();
        let output = menu_output.clone();
        m = m.item(
            PopupMenuItem::new("Copy output")
                .disabled(empty)
                .on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(output.clone()));
                }),
        );
        m
    })
    .into_any_element()
}

/// The aggregated summary of an adjacent tool run (the TS `summarizeToolRun`):
/// distinct short names in first-seen order, plus the running/failed fold. A
/// call still pending or in flight keeps the whole run live; a failed call
/// marks it failed without pretending it is live.
fn tool_run_summary(run: &[PartView]) -> (Vec<String>, bool, bool) {
    let mut names: Vec<String> = Vec::new();
    let mut running = false;
    let mut failed = false;
    for view in run {
        if let PartView::ToolCall { label, status, .. } = view {
            let name = tool_short_name(label);
            if !name.is_empty() && !names.contains(&name) {
                names.push(name);
            }
            if status == "pending" || status == "in_progress" {
                running = true;
            } else if status == "failed" {
                failed = true;
            }
        }
    }
    (names, running, failed)
}

/// A run of ADJACENT tool calls folds into one collapsed card (the TS
/// ToolRunCard): without surrounding text each row is context-free noise,
/// and a long turn produces dozens — the wall this card replaces. The
/// individual cards stay inside, each still expandable.
fn render_tool_run(
    row_id: &str,
    start_index: usize,
    run: &[PartView],
    open: bool,
    wb: &WeakEntity<Workbench>,
    theme: &component::Theme,
) -> AnyElement {
    let (names, running, failed) = tool_run_summary(run);
    // Same palette as the single card's glyph: accent while live, danger on
    // failure, muted once settled.
    let color = if running {
        theme.accent
    } else if failed {
        theme.danger
    } else {
        theme.muted_foreground
    };
    let label = fill(
        &t_count("chat.toolRun", run.len() as u64),
        &[("count", &run.len().to_string())],
    );
    let row_id_owned = row_id.to_string();
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
            .id(SharedString::from(format!(
                "tool-run-{}-{}",
                row_id, start_index
            )))
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .cursor_pointer()
            .overflow_hidden()
            .hover(|h| h.bg(theme.title_bar))
            .on_click({
                let wb = wb.clone();
                move |_, _window, cx| {
                    if let Some(wb) = wb.upgrade() {
                        wb.update(cx, |wb, cx| {
                            wb.set_tool_run_open(&row_id_owned, start_index, !open, cx)
                        });
                    }
                }
            })
            .child(
                div()
                    .flex_none()
                    .text_size(ui_text(12.))
                    .text_color(color)
                    .child(if running { "◆" } else { "◇" }),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(ui_text(12.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.foreground)
                    .child(label),
            )
            .when(!names.is_empty(), |this| {
                this.child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_size(ui_text(12.))
                        .text_color(theme.muted_foreground)
                        .child(names.join(" · ")),
                )
            })
            .child(
                div().ml_auto().flex_none().child(
                    Icon::new(IconName::ChevronDown)
                        .small()
                        .flex_none()
                        .when(!open, |icon| {
                            icon.rotate(radians(-std::f32::consts::FRAC_PI_2))
                        })
                        .text_color(theme.muted_foreground),
                ),
            )
            .test_support(),
    );
    if open {
        let mut body = div()
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .py_1()
            .border_t_1()
            .border_color(theme.border);
        for (i, view) in run.iter().enumerate() {
            body = body.child(render_tool_card(row_id, start_index + i, view, wb, theme));
        }
        card = card.child(body);
    }
    // Right-click on the run's chrome (summary row, the gaps between member
    // cards — the members themselves carry their own menus): copy every
    // command of the run, or every output joined. The same reverse-paint
    // stop as the member cards keeps the row menu out.
    let commands = run
        .iter()
        .filter_map(|p| match p {
            PartView::ToolCall { label, .. } => Some(label.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let outputs = run
        .iter()
        .filter_map(|p| match p {
            PartView::ToolCall { detail, .. } => Some(detail.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    card.id(SharedString::from(format!(
        "tool-run-card-{}-{}",
        row_id, start_index
    )))
    .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
    .context_menu(move |menu, _, _| {
        let mut m = menu;
        let commands = commands.clone();
        m = m.item(
            PopupMenuItem::new("Copy commands").on_click(move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(commands.clone()));
            }),
        );
        let empty = outputs.trim().is_empty();
        let outputs = outputs.clone();
        m = m.item(
            PopupMenuItem::new("Copy output")
                .disabled(empty)
                .on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(outputs.clone()));
                }),
        );
        m
    })
    .into_any_element()
}

impl Render for Workbench {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A window resize leaves both resizable splits holding their stored
        // PIXEL sizes from the old width — taffy's shrink can recover some
        // of it, but the chat column then overflows a narrowed window until
        // a dock toggle happens to re-lay the tree (observed 2026-10-21).
        // Reset both dock states whenever the viewport width changes so the
        // panels re-derive from the current window; the user's drag sizes
        // are the price of correctness here.
        let viewport_w = window.viewport_size().width;
        if self.last_viewport_w != Some(viewport_w) {
            self.last_viewport_w = Some(viewport_w);
            let dock = self.dock_sizes.clone();
            let session = self.session_dock_sizes.clone();
            window.defer(cx, move |_, cx| {
                dock.update(cx, |state, cx| {
                    state.clear();
                    cx.notify();
                });
                session.update(cx, |state, cx| {
                    state.clear();
                    cx.notify();
                });
            });
        }
        if self.clear_composer {
            self.clear_composer = false;
            let composer = self.composer.clone();
            window.defer(cx, move |window, cx| {
                composer.update(cx, |state, cx| state.set_value("", window, cx));
            });
        }
        // The incoming tab's adopted draft (a switch parked the outgoing
        // one — see on_runtime_event): text and staged attachments re-enter
        // together, after any pending clear so a just-sent draft cannot
        // resurrect. Same parked-flag discipline as the prefill below.
        if let Some((text, staged)) = self.composer_restore.take() {
            let composer = self.composer.clone();
            window.defer(cx, move |window, cx| {
                composer.update(cx, |state, cx| state.set_value(&text, window, cx));
            });
            self.staged = staged;
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
        // A dropped non-image file or directory appends its path reference
        // to the composer text — read at apply time, so typing since the
        // drop survives.
        if let Some(tail) = self.composer_append.take() {
            let composer = self.composer.clone();
            window.defer(cx, move |window, cx| {
                composer.update(cx, |state, cx| {
                    let mut text = state.value().to_string();
                    if !text.is_empty() && !text.ends_with(char::is_whitespace) {
                        text.push(' ');
                    }
                    text.push_str(&tail);
                    state.set_value(&text, window, cx);
                });
            });
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
        // A snap page (driver-owned) replaces the workbench surfaces below
        // the status bar. The drawer rides ABOVE the current surface — the
        // rail's quota drawer never replaces anything.
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
            // after the attach dialog and clipboard paste); what does NOT
            // decode — a plain file, a directory — appends its path to the
            // composer as a reference instead of bouncing.
            .on_drop(cx.listener(|_this, paths: &ExternalPaths, _window, cx| {
                let paths = paths.paths().to_vec();
                let load = cx
                    .background_executor()
                    .spawn(async move { read_dropped_paths(paths) });
                cx.spawn(async move |this, cx| {
                    let (inputs, tail) = load.await;
                    if let Some(entity) = this.upgrade() {
                        let _ = cx.update_entity(&entity, |this, cx| {
                            this.stage_inputs(inputs, cx);
                            if tail.is_some() {
                                this.composer_append = tail;
                                cx.notify();
                            }
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
            .on_action(cx.listener(Self::on_toggle_terminal))
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
            // The content region: the current surface — the dock and chat, or
            // an open configuration page replacing them (the TS configuration
            // screens replace the chat/picker screens wholesale) — plus the
            // overlay drawer riding ON TOP of it. `relative` anchors the
            // drawer; later root siblings (sheets, viewer, dialogs) still
            // paint above it.
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .when_some(
                        if !snap_page_open {
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
                            this.child(if let Some(_form) = &self.provider_form {
                                self.render_provider_form(cx)
                            } else if self.models_form.is_some() {
                                self.render_models_form(cx)
                            } else {
                                self.render_models_page(cx)
                            })
                        },
                    )
                    .when_some(self.snap.system_status.clone(), |this, page| {
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
                    })
                    .when_some(
                        self.snap.quota_page.clone(),
                        // The plan-quota page (settings entry): a full page
                        // like every config section. The RAIL quota drawer is
                        // the separate all-providers surface.
                        |this, _page| this.child(self.render_quota_page(cx)),
                    )
                    .when_some(self.snap.usage_page.clone(), |this, page| {
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
                    })
                    // The ZCode-config section pages (batch I): the open form
                    // replaces the page while open, exactly like the Models form.
                    .when_some(self.snap.config_section_page.clone(), |this, _page| {
                        this.child(if self.zconfig_form.is_some() {
                            self.render_zconfig_form(cx)
                        } else {
                            crate::zconfig_pages::render_config_section_page(self, cx)
                        })
                    })
                    // The Workflows page (batch J): the same contract — the open
                    // start/amend/meta form replaces the page while open.
                    .when_some(self.snap.workflows_page.clone(), |this, _page| {
                        this.child(if self.workflow_start_form.is_some() {
                            self.render_workflow_start_form(cx)
                        } else if self.workflow_amend_form.is_some() {
                            self.render_workflow_amend_form(cx)
                        } else if self.workflow_meta_form.is_some() {
                            self.render_workflow_meta_form(cx)
                        } else {
                            crate::workflows_page::render_workflows_page(self, cx)
                        })
                    })
                    // The overlay drawer floats above whichever surface is
                    // current — settings or the all-providers quota display.
                    .when_some(self.drawer, |this, surface| {
                        this.child(self.render_drawer(surface, cx))
                    }),
            )
            // The WorkflowGate sheet (J2) sits above the config surfaces —
            // the TS bottom sheet is the topmost layer while open.
            .when(self.workflow_gate_sheet, |this| {
                this.child(self.render_workflow_gate_sheet(cx))
            })
            // The new-session chooser rides ON TOP of whatever surface is
            // below (the TS dialog is a fixed full-screen overlay).
            .when_some(self.snap.new_session.clone(), |this, view| {
                this.child(self.render_new_session(&view, cx))
            })
            // The rename sheet (G1) sits above the chooser — the TS action
            // sheet is the topmost layer while open.
            .when(self.rename_target.is_some(), |this| {
                this.child(self.render_rename_sheet(cx))
            })
            // The session-history dialog (its entry lives in the sessions
            // panel): above the dock and drawers; the file viewer, lightbox
            // and quit dialog stay above it.
            .when(self.snap.history.open, |this| {
                this.child(self.render_history_dialog(cx))
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
                let md_source = matches!(&self.file_md_source, Some((p, true)) if *p == view.path);
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
            // The quit dialog outranks even the lightbox (priority 3): it is
            // app-level, and ⌘Q can fire over any open layer.
            .when(self.quit_dialog_open || self.quit_stopping, |this| {
                this.child(self.render_quit_dialog(window, cx))
            })
    }
}

impl Workbench {
    /// The overlay drawer: slides out from the LEFT, flush against the rail
    /// (its toggle stays clickable), floating over whatever surface is
    /// current WITHOUT reflowing or replacing it — the rail buttons sit
    /// beside the surface they open, so the drawer opens on their side.
    /// Non-modal by design — no backdrop, so the UI beside it stays fully
    /// interactive; the X, Esc, and the rail toggle close it. Later root
    /// siblings (sheets, file viewer) and the deferred layers (lightbox,
    /// quit dialog) paint above.
    fn render_drawer(&self, surface: DrawerSurface, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let wb = cx.entity();
        let (panel_id, title, header_trail, body) = match surface {
            DrawerSurface::Settings => (
                "drawer-settings",
                t!("panel.title").into_owned(),
                None,
                self.render_config_panel(cx),
            ),
            // The all-providers quota display (the mobile side panels' card
            // at drawer scale). The refresh rides the header; the body is
            // pure content from the shared card renderer.
            DrawerSurface::Quota => {
                let refresh = self.runtime.clone();
                (
                    "drawer-quota",
                    t!("quota.title").into_owned(),
                    Some(
                        Button::new("quota-refresh")
                            .ghost()
                            .xsmall()
                            .icon(IconName::RefreshCw)
                            .tooltip(t!("quota.refresh").into_owned())
                            .on_click(move |_, _, _| refresh.send(UiCommand::RefreshQuota))
                            .into_any_element(),
                    ),
                    crate::quota_card::render_quota_drawer_body(&self.snap.quota_card, &theme),
                )
            }
        };
        let close_surface = surface;
        div()
            .id(SharedString::from(panel_id))
            .test_support()
            .flex()
            .flex_col()
            .absolute()
            .top_0()
            .left(px(RAIL_WIDTH))
            .bottom_0()
            .w(px(DRAWER_WIDTH))
            .flex_none()
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.title_bar)
            .shadow_lg()
            // The panel swallows clicks inside it — nothing beneath reacts
            // through the drawer. Same containment for the wheel, and it
            // OCCLUDES: a scroll container never stops propagation, so a
            // wheel at the drawer list's end chained into the sessions panel
            // underneath (2026-10-21). A BlockMouse hitbox ends the hit test
            // at itself, so the panels behind stop answering scroll; the
            // drawer's own deeper scrollers are collected first and keep
            // scrolling.
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_scroll_wheel(|_, _, cx| {
                cx.stop_propagation();
            })
            .occlude()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .h_9()
                    .px_2()
                    .flex_none()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_size(ui_text(13.))
                            .text_color(theme.foreground)
                            .child(title),
                    )
                    .when_some(header_trail, |this, trail| this.child(trail))
                    .child(
                        Button::new("drawer-close")
                            .ghost()
                            .xsmall()
                            .icon(KitIconName::X)
                            .tooltip("Close the drawer")
                            .on_click(move |_, _, cx| {
                                wb.update(cx, |wb, cx| wb.toggle_drawer(close_surface, cx));
                            }),
                    ),
            )
            .child(body)
            .into_any_element()
    }

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
                        .text_size(ui_text(12.))
                        .text_color(theme.muted_foreground)
                        .child(caption),
                )
            });
        let backdrop = div()
            .id("lightbox-backdrop")
            .on_scroll_wheel(|_, _, cx| {
                cx.stop_propagation();
            })
            // A modal: nothing behind it answers a scroll through it. The
            // card's own hitboxes are inserted later (deeper), so they stay
            // live; the BlockMouse hitbox only cuts the surfaces behind.
            .occlude()
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

    /// The quit dialog (requirements §4): opens when turns THIS app started
    /// are still running — Quit Only closes the app and leaves every turn
    /// running (the default), Stop Turns & Quit cancels the own turns first
    /// and closes once the cancels flushed. Turns other clients started are
    /// never touched either way; Esc and the backdrop dismiss the dialog and
    /// keep the window. Deferred like the lightbox — above every layer, one
    /// priority step higher still.
    fn render_quit_dialog(&self, window: &mut Window, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let wb = cx.entity();
        let n = self.snap.own_running_session_count;
        let viewport = window.viewport_size();
        let stopping = self.quit_stopping;

        let mut card = div()
            .id("quit-dialog")
            .test_support()
            .flex()
            .flex_col()
            .gap_2()
            .w_full()
            .max_w(px(440.))
            .p_4()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.title_bar)
            // The card swallows backdrop clicks (its own close).
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .child(
                div()
                    .text_size(ui_text(13.))
                    .text_color(theme.foreground)
                    .child("Quit with your turns still running"),
            )
            .child(
                div()
                    .min_w_0()
                    .text_size(ui_text(12.))
                    .text_color(theme.muted_foreground)
                    .child(format!(
                        "{n} turn{s} you started from this app {is} still running on the hub — \
                 they keep running after it closes. Turns started from other clients \
                 are never touched.",
                        s = if n == 1 { "" } else { "s" },
                        is = if n == 1 { "is" } else { "are" },
                    )),
            );
        if stopping {
            card = card.child(
                div()
                    .id("quit-stopping")
                    .test_support()
                    .text_size(ui_text(12.))
                    .text_color(theme.muted_foreground)
                    .child("Stopping the running turns…"),
            );
        } else {
            card = card.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("quit-stop")
                            .secondary()
                            .small()
                            .label("Stop Turns & Quit")
                            .on_click({
                                let wb = wb.clone();
                                move |_, window, cx| {
                                    wb.update(cx, |wb, cx| {
                                        wb.stop_own_turns_and_quit(window, cx);
                                    });
                                }
                            }),
                    )
                    // The default choice (requirements §4): quit as-is.
                    .child(
                        Button::new("quit-only")
                            .primary()
                            .small()
                            .label("Quit Only")
                            .on_click({
                                let wb = wb.clone();
                                move |_, window, cx| {
                                    wb.update(cx, |wb, _| {
                                        wb.quit_confirmed = true;
                                        wb.quit_dialog_open = false;
                                    });
                                    // The confirmed close: the last window
                                    // closing quits the app
                                    // (QuitMode::LastWindowClosed, main.rs).
                                    window.remove_window();
                                }
                            }),
                    ),
            );
        }
        let backdrop = div()
            .id("quit-backdrop")
            .on_scroll_wheel(|_, _, cx| {
                cx.stop_propagation();
            })
            // A modal — see the lightbox backdrop: occlude cuts every surface
            // behind it out of the wheel's hit test, the card stays live.
            .occlude()
            .test_support()
            .flex()
            .items_center()
            .justify_center()
            .absolute()
            .top_0()
            .left_0()
            .w(viewport.width)
            .h(viewport.height)
            .bg(gpui::black().opacity(0.55))
            .when(!stopping, |this| {
                this.on_mouse_down(MouseButton::Left, {
                    let wb = wb.clone();
                    move |_, _, cx| {
                        wb.update(cx, |this, cx| {
                            this.quit_dialog_open = false;
                            cx.notify();
                        });
                    }
                })
            })
            .child(card);
        deferred(
            anchored()
                .anchor(Anchor::TopLeft)
                .position(point(px(0.), px(0.)))
                .child(backdrop),
        )
        // Priority 3: above the lightbox's 2 — quitting outranks every layer.
        .with_priority(3)
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
                    .text_size(ui_text(13.))
                    .text_color(theme.foreground)
                    .child("Rename session"),
            )
            .child(
                div()
                    .min_w_0()
                    .text_size(ui_text(12.))
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
                                move |_, _, cx| wb.update(cx, |wb, cx| wb.submit_rename(cx))
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
            // A modal — see the history dialog: the wheel must not reach the
            // surfaces behind it once the sheet's own list ends, and a
            // BlockMouse hitbox ends the hit test here (the card's deeper
            // hitboxes are collected first and stay live).
            .occlude()
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
                        .text_size(ui_text(14.))
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
                div()
                    .px_1()
                    .child(Input::new(&self.new_session_filter).small()),
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
                        .text_size(ui_text(12.))
                        .text_color(theme.warning_foreground)
                        .child(err.clone()),
                );
            }
            match &view.projects {
                None => {
                    list = list.child(
                        div()
                            .text_size(ui_text(13.))
                            .text_color(theme.muted_foreground)
                            .child("loading projects…"),
                    );
                }
                Some(projects) if projects.is_empty() => {
                    list = list.child(
                        div()
                            .text_size(ui_text(13.))
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
                                .text_size(ui_text(13.))
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
                                        .text_size(ui_text(13.))
                                        .text_color(theme.foreground)
                                        .child(name),
                                )
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(ui_text(10.))
                                        .font_family(theme.mono_font_family.clone())
                                        .text_color(theme.muted_foreground)
                                        .child(workspace_path.clone()),
                                ),
                        );
                        row = row.child(
                            div()
                                .flex_none()
                                .text_size(ui_text(10.))
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
            .id("new-session-overlay")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .p_6()
            .bg(theme.background.opacity(0.65))
            // Wheel containment, and OCCLUDE: gpui's scroll containers never
            // stop propagation, so a wheel at the card's list end used to
            // chain into the sessions panel below the overlay. The BlockMouse
            // hitbox ends the hit test at the overlay, so the surfaces behind
            // report should_handle_scroll() == false; the card's own deeper
            // scrollers are collected first and keep scrolling.
            .on_scroll_wheel(|_, _, cx| {
                cx.stop_propagation();
            })
            .occlude()
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
                .text_size(ui_text(10.))
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
                    .text_size(ui_text(12.))
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
                            .text_size(ui_text(12.))
                            .text_color(theme.accent)
                            .child("creating…"),
                    )
                })
                .when(!browse.creatable, |this| {
                    this.child(
                        div()
                            .text_size(ui_text(10.))
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
                            .text_size(ui_text(12.))
                            .text_color(theme.muted_foreground)
                            .child("↑"),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(ui_text(12.))
                            .font_family(theme.mono_font_family.clone())
                            .text_color(theme.muted_foreground)
                            .child(parent_label),
                    ),
            );
        }
        if !browse.loading && browse.entries.is_empty() && browse.error.is_none() {
            body = body.child(
                div()
                    .text_size(ui_text(12.))
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
                            .text_size(ui_text(12.))
                            .text_color(theme.muted_foreground)
                            .child("▸"),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(ui_text(13.))
                            .text_color(theme.foreground)
                            .child(name.clone()),
                    ),
            );
        }
        if browse.truncated {
            body = body.child(
                div()
                    .text_size(ui_text(10.))
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
    use super::{
        context_pct, fmt_context, fmt_tokens, parse_task_notification, PartView, RowModel,
    };
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

    #[test]
    fn tool_display_name_flattens_newlines_and_caps() {
        // A heredoc command's title carries real newlines — the summary row
        // is one line, so whitespace runs collapse.
        let flat = super::tool_display_name("Bash: cat <<'EOF'\nimport json\nEOF");
        assert!(!flat.contains('\n'));
        assert_eq!(flat, "Bash: cat <<'EOF' import json EOF");
        // Long titles cap with an ellipsis instead of measuring/rendering
        // thousands of chars every frame.
        let long = "x".repeat(400);
        let capped = super::tool_display_name(&long);
        assert!(capped.chars().count() <= 241);
        assert!(capped.ends_with('…'));
    }

    #[test]
    fn tool_run_summary_dedupes_names_and_folds_status() {
        let part = |label: &str, status: &str| PartView::ToolCall {
            label: label.into(),
            status: status.to_string(),
            detail_head: String::new(),
            detail: String::new(),
            fold_kind: None,
            expanded: false,
            diff_src: Vec::new(),
            diffs: Vec::new(),
        };
        let run = vec![
            part("Bash: npm test", "completed"),
            part("Read: /tmp/a.ts", "completed"),
            part("Bash: ls", "completed"),
        ];
        let (names, running, failed) = super::tool_run_summary(&run);
        assert_eq!(names, vec!["Bash", "Read"]);
        assert!(!running && !failed);

        // A pending or in-flight call keeps the run live.
        let live = vec![part("Bash: x", "completed"), part("Read: y", "in_progress")];
        let (_, running, failed) = super::tool_run_summary(&live);
        assert!(running && !failed);

        // A failed call marks the run failed without faking liveness.
        let broken = vec![part("Bash: x", "completed"), part("Read: y", "failed")];
        let (_, running, failed) = super::tool_run_summary(&broken);
        assert!(!running && failed);
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
        Phase, ProtocolRuntime, RuntimeEvent, TabPhase, TabSummary, UiCommand, WorkbenchSnapshot,
    };
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::InputEvent as _;
    use gpui_kit::{
        px, AnyWindowHandle, App, AppContext as _, Bounds, Entity, Point, Size, TestAppContext,
        WindowBounds, WindowOptions,
    };
    use protocol::chat::ChatRole;

    fn connected_snapshot() -> WorkbenchSnapshot {
        let mut snap = WorkbenchSnapshot::initial();
        snap.phase = Phase::Connected;
        snap.tabs = vec![TabSummary {
            id: 7,
            session_id: Some("sess_1".to_string()),
            workspace: None,
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

    /// Fenced code blocks in the chat's markdown colour through the kit's
    /// tree-sitter grammars. The highlighter installs with the kit theme
    /// (`Theme::sync_base`) only while the `tree-sitter-*` features are on
    /// in src-gpui/Cargo.toml — dropping them would silently return every
    /// code block to plain text, so the install and the fence tags an agent
    /// reply actually uses are pinned here.
    #[gpui_kit::test]
    fn chat_code_blocks_have_a_syntax_highlighter(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            assert!(
                gpui_kit::base::TextViewDefaults::global(cx).has_code_block_highlighter(),
                "the kit's code-block highlighter must install with the theme"
            );
        });
        let registry = gpui_kit::component::highlighter::LanguageRegistry::singleton();
        for lang in [
            "rust",
            "typescript",
            "tsx",
            "javascript",
            "python",
            "go",
            "bash",
            "json",
            "yaml",
            "toml",
            "diff",
            "markdown",
        ] {
            assert!(
                registry.language(lang).is_some(),
                "fence tag `{lang}` must resolve to a compiled grammar"
            );
        }
    }

    /// The embedded terminal dock toggles under the chat column: hidden by
    /// default, the toggle mounts the panel (the test build's placeholder —
    /// `cfg!(test)` never spawns a real shell), toggling again unmounts it.
    #[gpui_kit::test]
    fn the_terminal_panel_toggles_under_the_chat(cx: &mut TestAppContext) {
        let _windows = super::window_test_lock();
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let snap = connected_snapshot();
        let (handle, wb, _rt) = cx.update(|cx| open(cx, snap, 1000.));
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            assert!(
                window.try_find("terminal-panel").is_none(),
                "the dock starts hidden"
            );
            wb.update(cx, |wb, cx| wb.toggle_terminal(window, cx));
            window.draw(cx).clear(cx);
            assert!(window.find("terminal-panel").visible());
            assert!(
                window.find("terminal-placeholder").visible(),
                "the test build paints the placeholder, not a live shell"
            );
            wb.update(cx, |wb, cx| wb.toggle_terminal(window, cx));
            window.draw(cx).clear(cx);
            assert!(
                window.try_find("terminal-panel").is_none(),
                "the second toggle unmounts the panel"
            );
        })
        .unwrap();
    }

    /// The rail carries the terminal's visible door, and each tab owns its
    /// OWN terminal: opening on one tab never opens another's, a switch
    /// shows the incoming tab's state, and closing a tab drops its terminal
    /// (the dock — the shell — goes with it).
    #[gpui_kit::test]
    fn each_tab_owns_its_terminal(cx: &mut TestAppContext) {
        let _windows = super::window_test_lock();
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let two_tabs = |active: Option<u64>, keep_7: bool| {
            let mut snap = WorkbenchSnapshot::initial();
            snap.phase = Phase::Connected;
            if keep_7 {
                snap.tabs.push(TabSummary {
                    id: 7,
                    session_id: Some("sess_1".to_string()),
                    workspace: Some("/repo-a".to_string()),
                    session_title: None,
                    phase: TabPhase::Connected,
                    running: false,
                    pending: 0,
                    awaiting: false,
                });
            }
            snap.tabs.push(TabSummary {
                id: 8,
                session_id: Some("sess_8".to_string()),
                workspace: Some("/repo-b".to_string()),
                session_title: None,
                phase: TabPhase::Connected,
                running: false,
                pending: 0,
                awaiting: false,
            });
            snap.active_tab = active;
            snap
        };
        let (handle, wb, _rt) = cx.update(|cx| open(cx, two_tabs(Some(7), true), 1000.));
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            window.click("dock-rail-terminal", cx);
            window.draw(cx).clear(cx);
            assert!(
                window.find("terminal-panel").visible(),
                "the rail click opens the active tab's terminal"
            );
            // Switch to tab 8: its terminal was never opened — the dock
            // stays with tab 7, parked.
            wb.update(cx, |wb, cx| {
                wb.on_runtime_event(RuntimeEvent::Snapshot(two_tabs(Some(8), true)), cx)
            });
            window.draw(cx).clear(cx);
            assert!(
                window.try_find("terminal-panel").is_none(),
                "the incoming tab's terminal is its own, unopened"
            );
            let mut open: Vec<u64> =
                wb.update(cx, |wb, _| wb.tab_terminal_open.iter().copied().collect());
            open.sort_unstable();
            assert_eq!(open, [7u64]);
        })
        .unwrap();
        cx.update_window(handle.into(), |_, window, cx| {
            // Tab 8 opens its own; both flags now live side by side.
            window.click("dock-rail-terminal", cx);
            window.draw(cx).clear(cx);
            assert!(window.find("terminal-panel").visible());
            let mut open: Vec<u64> =
                wb.update(cx, |wb, _| wb.tab_terminal_open.iter().copied().collect());
            open.sort_unstable();
            assert_eq!(open, [7u64, 8u64]);
            // Tab 7 closes: its terminal state goes with it — the shell
            // would be dropped (killed) with the dock.
            wb.update(cx, |wb, cx| {
                wb.on_runtime_event(RuntimeEvent::Snapshot(two_tabs(Some(8), false)), cx)
            });
            window.draw(cx).clear(cx);
            assert!(window.find("terminal-panel").visible());
            let mut open: Vec<u64> =
                wb.update(cx, |wb, _| wb.tab_terminal_open.iter().copied().collect());
            open.sort_unstable();
            assert_eq!(open, [8u64], "a closed tab's terminal state is pruned");
        })
        .unwrap();
    }

    /// The chat's code surfaces are neutral grays and the syntax tokens are
    /// the kit's own Default Dark set. The glare the user kept reporting as
    /// "the highlight" was never a token colour: the kit maps the
    /// inline-code chip onto the theme accent — a solid emerald rectangle
    /// behind every `code` span. `apply_quiet_dark` re-styles the installed
    /// TextViewDefaults (chip on the hairline gray, fenced panels on the
    /// muted surface) and leaves the highlight theme untouched.
    #[gpui_kit::test]
    fn quiet_dark_paints_code_surfaces_neutral(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
            crate::apply_quiet_dark(cx);
            let theme = gpui_kit::component::Theme::global(cx);
            let style = crate::chat_text_view_style(theme);
            assert_ne!(
                style.inline_code().background_color,
                Some(theme.accent),
                "no accent chip behind `code` spans"
            );
            assert_eq!(
                style.inline_code().background_color,
                Some(theme.border),
                "the inline-code chip is the hairline gray"
            );
            assert_eq!(
                style.code_background(),
                theme.muted,
                "fenced panels keep the theme's muted surface"
            );
            assert_eq!(
                *theme.highlight_theme,
                *gpui_kit::component::highlighter::HighlightTheme::default_dark(),
                "the syntax tokens are the kit defaults again"
            );
        });
    }

    /// The split button grows the ACTIVE tab's pane row — real panes even
    /// under test (split_terminal spawns; only the rail toggle keeps the
    /// placeholder) — up to the cap. A pane's close takes just that pane;
    /// the last one takes the panel with it (the exit callback rides the
    /// same close path).
    #[gpui_kit::test]
    fn splitting_the_terminal_adds_side_by_side_panes(cx: &mut TestAppContext) {
        let _windows = super::window_test_lock();
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let (handle, wb, _rt) = cx.update(|cx| open(cx, connected_snapshot(), 1400.));
        cx.update_window(handle.into(), |_, window, cx| {
            fn pane_count(wb: &Entity<Workbench>, cx: &mut App) -> usize {
                wb.update(cx, |wb, _| {
                    wb.tab_terminals.get(&7).map(|p| p.len()).unwrap_or(0)
                })
            }
            // Open via the rail (test: placeholder), then split — the
            // button spawns for real.
            window.click("dock-rail-terminal", cx);
            window.draw(cx).clear(cx);
            assert!(window.find("terminal-placeholder").visible());
            window.click("terminal-split-add", cx);
            window.draw(cx).clear(cx);
            assert_eq!(
                pane_count(&wb, cx),
                1,
                "the first split spawns the first pane"
            );
            assert!(
                window.try_find("terminal-placeholder").is_none(),
                "a real pane replaces the placeholder"
            );
            // The second pane shows the per-pane strips.
            window.click("terminal-split-add", cx);
            window.draw(cx).clear(cx);
            assert_eq!(pane_count(&wb, cx), 2);
            assert!(window.find(("terminal-pane-close", 0usize)).visible());
            assert!(window.find(("terminal-pane-close", 1usize)).visible());
            // The cap: a third pane fits, a fourth does not.
            window.click("terminal-split-add", cx);
            window.draw(cx).clear(cx);
            assert_eq!(pane_count(&wb, cx), 3);
            window.click("terminal-split-add", cx);
            window.draw(cx).clear(cx);
            assert_eq!(
                pane_count(&wb, cx),
                3,
                "the row caps at MAX_TERMINAL_PANES"
            );
            // Closing one pane keeps the panel and its siblings; closing
            // them all takes the panel down.
            let second = wb.update(cx, |wb, _| wb.tab_terminals[&7][1].view.entity_id());
            wb.update(cx, |wb, cx| wb.close_terminal(second, cx));
            window.draw(cx).clear(cx);
            assert_eq!(pane_count(&wb, cx), 2);
            assert!(window.find("terminal-panel").visible());
            let all: Vec<_> = wb.update(cx, |wb, _| {
                wb.tab_terminals[&7]
                    .iter()
                    .map(|p| p.view.entity_id())
                    .collect()
            });
            for pane in all {
                wb.update(cx, |wb, cx| wb.close_terminal(pane, cx));
            }
            window.draw(cx).clear(cx);
            assert!(
                window.try_find("terminal-panel").is_none(),
                "the last pane takes the panel with it"
            );
            assert!(
                !wb.update(cx, |wb, _| wb.tab_terminal_open.contains(&7)),
                "and clears the open flag"
            );
        })
        .unwrap();
    }

    /// ⌃` is a GLOBAL binding (main.rs, no action context): it must toggle
    /// the terminal no matter what owns the keyboard — from the rail button
    /// (a generic non-composer focus) AND from INSIDE a focused terminal
    /// pane, the strictest case (the view writes most keys to its shell).
    /// The raw dispatch_keystroke keeps the harness's observed-focus guard
    /// out of the way for the unobserved terminal focus.
    #[gpui_kit::test]
    fn ctrl_backquote_toggles_the_terminal_from_any_focus(cx: &mut TestAppContext) {
        let _windows = super::window_test_lock();
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
            crate::install_keybindings(cx);
        });
        let (handle, _wb, _rt) = cx.update(|cx| open(cx, connected_snapshot(), 1000.));
        let ctrl_backquote =
            || gpui_kit::Keystroke::parse("ctrl-`").expect("the binding's own spelling parses");
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            // Focus 1: the rail button (a non-composer focus path).
            window.click("dock-rail-terminal", cx);
            window.draw(cx).clear(cx);
            assert!(
                window.find("terminal-panel").visible(),
                "the rail click opens the terminal"
            );
            window.press("ctrl-`", cx);
            window.draw(cx).clear(cx);
            assert!(
                window.try_find("terminal-panel").is_none(),
                "⌃` closes the terminal from a non-composer focus"
            );
            // Focus 2: a REAL pane's shell view — the deepest focus there
            // is, and the one whose on_key_down writes to the PTY.
            _wb.update(cx, |wb, cx| {
                let dock = crate::terminal_panel::spawn_dock(None, cx.weak_entity(), cx)
                    .expect("spawn");
                wb.tab_terminals.insert(7, vec![dock]);
                wb.tab_terminal_open.insert(7);
                cx.notify();
            });
            window.draw(cx).clear(cx);
            window.draw(cx).clear(cx);
            let focus = _wb.update(cx, |wb, cx| {
                wb.tab_terminals[&7][0].focus.clone()
            });
            window.focus(&focus, cx);
            window.draw(cx).clear(cx);
            assert!(
                window.find("terminal-panel").visible(),
                "⌃` reopened the terminal while focus sat in the composer"
            );
            window.dispatch_keystroke(ctrl_backquote(), cx);
            window.draw(cx).clear(cx);
            assert!(
                window.try_find("terminal-panel").is_none(),
                "⌃` closes the terminal even while the shell view owns focus"
            );
            // The close returned focus to the composer — the reopen must
            // work from there too (the lingering-hidden-focus trap).
            window.dispatch_keystroke(ctrl_backquote(), cx);
            window.draw(cx).clear(cx);
            assert!(
                window.find("terminal-panel").visible(),
                "⌃` reopens after the close returned focus to the composer"
            );
        })
        .unwrap();
    }

    /// A REAL dock (live PTY and view, not the test placeholder) must fit
    /// its grid to the panel it sits in: the panel root carries `flex_1`
    /// because the resizable panel's root is a ROW-direction flex box — a
    /// child without it shrinks to its min-content width (the header's
    /// ~150px) and the shell wraps at the spawn-time 80 columns.
    #[gpui_kit::test]
    fn the_terminal_grid_fits_the_panel_width(cx: &mut TestAppContext) {
        let _windows = super::window_test_lock();
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let (handle, wb, _rt) = cx.update(|cx| open(cx, connected_snapshot(), 1400.));
        cx.update_window(handle.into(), |_, window, cx| {
            // The test-mode toggle mounts the placeholder only; mount the
            // real dock by hand (the production spawn path).
            wb.update(cx, |wb, cx| {
                let dock =
                    crate::terminal_panel::spawn_dock(None, cx.weak_entity(), cx).expect("spawn");
                wb.tab_terminals.insert(7, vec![dock]);
                wb.tab_terminal_open.insert(7);
                cx.notify();
            });
            window.draw(cx).clear(cx);
            window.draw(cx).clear(cx);
            let panel = window.find("terminal-panel");
            let panel_w = f32::from(panel.bounds().size.width);
            let (cols, rows) = wb.update(cx, |wb, cx| {
                let pane = wb
                    .tab_terminals
                    .get(&7)
                    .expect("the dock stays mounted")
                    .first()
                    .expect("the tab keeps its pane");
                pane.view.read(cx).dimensions()
            });
            // 1400px window minus the ~280px dock + rail leaves the chat
            // column ~1080px wide — far more than the 80-column default.
            assert!(
                panel_w > 900.,
                "the panel must span the chat column, not shrink to its content (w={panel_w})"
            );
            assert!(
                cols > 100,
                "the grid must follow the panel width, not the spawn default (cols={cols})"
            );
            assert!(rows > 1, "the grid must follow the panel height too");
        })
        .unwrap();
    }

    /// The quit dialog gates on turns THIS app started only (§4): other
    /// clients' running turns never open it; an own turn opens the
    /// two-choice dialog, Esc dismisses it (keep window), and the stop-all
    /// path holds the close while its cancels flush.
    #[gpui_kit::test]
    fn quit_dialog_gates_on_own_turns_only(cx: &mut TestAppContext) {
        let _windows = super::window_test_lock();
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });

        // Other clients' turns alone: no dialog, the window may close.
        let mut snap = connected_snapshot();
        snap.running_session_count = 3;
        let (handle, wb, _rt) = cx.update(|cx| open(cx, snap, 1000.));
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            assert!(
                wb.update(cx, |wb, cx| wb.request_quit(window, cx)),
                "other clients' turns never gate quitting"
            );
        })
        .unwrap();

        // An own turn: the dialog opens, vetoes the close, and Esc
        // dismisses it without quitting.
        let mut snap = connected_snapshot();
        snap.own_running_session_count = 1;
        let (handle, wb, _rt) = cx.update(|cx| open(cx, snap, 1000.));
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            assert!(
                !wb.update(cx, |wb, cx| wb.request_quit(window, cx)),
                "an own running turn vetoes the close"
            );
        })
        .unwrap();
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            assert!(window.find("quit-dialog").visible());
            assert!(window.find("quit-only").visible());
            assert!(window.find("quit-stop").visible());
            wb.update(cx, |wb, cx| wb.on_stop_turn(&super::StopTurn, window, cx));
        })
        .unwrap();
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            assert!(
                window.try_find("quit-dialog").is_none(),
                "Esc dismisses the quit dialog"
            );
            assert!(
                !wb.update(cx, |wb, cx| wb.request_quit(window, cx)),
                "still gated — the own turn keeps running"
            );
        })
        .unwrap();

        // Stop Turns & Quit: the cancels flush behind a stopping card and
        // the close stays vetoed until they did.
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            assert!(window.find("quit-dialog").visible(), "the reopened dialog");
            window.click("quit-stop", cx);
        })
        .unwrap();
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            assert!(
                window.find("quit-stopping").visible(),
                "the flush window paints"
            );
            assert!(
                !wb.update(cx, |wb, cx| wb.request_quit(window, cx)),
                "the close stays vetoed while the cancels flush"
            );
        })
        .unwrap();

        // Quit Only (the default): confirms the close — the window either
        // already went away or passes the next request through unasked.
        let mut snap = connected_snapshot();
        snap.own_running_session_count = 2;
        let (handle, wb, _rt) = cx.update(|cx| open(cx, snap, 1000.));
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            assert!(!wb.update(cx, |wb, cx| wb.request_quit(window, cx)));
            window.click("quit-only", cx);
        })
        .unwrap();
        match cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            window.try_find("quit-dialog").is_none()
                && wb.update(cx, |wb, cx| wb.request_quit(window, cx))
        }) {
            Ok(confirmed) => assert!(confirmed, "Quit Only confirms the close"),
            Err(_) => {} // the window already closed — the stronger outcome
        }
    }

    /// The chat column must follow the window's width: shrinking the window
    /// leaves the dock panel's stored pixels and the chat's measured content
    /// minimum fighting for space, and the loser used to be the window —
    /// the composer ran past the right edge until a dock toggle re-laid the
    /// split (observed 2026-10-21).
    #[gpui_kit::test]
    fn chat_column_follows_window_shrink(cx: &mut TestAppContext) {
        let _windows = super::window_test_lock();
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let (handle, _wb, _rt) = cx.update(|cx| open(cx, connected_snapshot(), 1200.));
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            window.draw(cx).clear(cx);
            let wide = window.find("composer").bounds();
            assert!(
                wide.right() <= px(1200.) && wide.size.width > px(700.),
                "the composer fills the wide window (got {wide:?})"
            );
        })
        .unwrap();

        cx.simulate_window_resize(handle.into(), gpui_kit::size(px(560.), px(700.)));
        cx.update_window(handle.into(), |_, window, cx| {
            // The resizable's container adjustment lands a frame after the
            // resize; give it the settling draw.
            for _ in 0..5 {
                window.draw(cx).clear(cx);
            }
            let narrow = window.find("composer").bounds();
            assert!(
                narrow.right() <= px(560.),
                "the composer fits the 560px window (got {narrow:?})"
            );
            assert!(
                narrow.size.width >= px(200.),
                "the chat keeps a sane minimum (got {narrow:?})"
            );
        })
        .unwrap();

        // A second, harder shrink: at 480px the rail (44) + dock floor (200)
        // + the composer's clipped button row must still fit inside the
        // window — the textarea's intrinsic width no longer floors the chat.
        cx.simulate_window_resize(handle.into(), gpui_kit::size(px(480.), px(700.)));
        cx.update_window(handle.into(), |_, window, cx| {
            for _ in 0..5 {
                window.draw(cx).clear(cx);
            }
            let tiny = window.find("composer").bounds();
            assert!(
                tiny.right() <= px(480.),
                "the composer fits the 480px window (got {tiny:?})"
            );
        })
        .unwrap();
    }

    /// The overlay drawer (settings + the all-providers quota) floats above
    /// the chat WITHOUT replacing it: the rail toggles open it over a
    /// still-mounted composer (and seed the quota read), the drawer is
    /// anchored flush right of the rail at full height, Esc closes, and the
    /// plan-quota PAGE (the settings entry) is the separate full-page
    /// surface that also dismisses the drawer.
    #[gpui_kit::test]
    fn drawers_overlay_the_chat_without_replacing_it(cx: &mut TestAppContext) {
        let _windows = super::window_test_lock();
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let (handle, wb, runtime) = cx.update(|cx| open(cx, connected_snapshot(), 1000.));

        // Settings: the rail toggle opens the drawer over an intact chat.
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            assert!(window.try_find("drawer-settings").is_none());
            assert!(window.find("composer").visible());
            window.click("dock-rail-config", cx);
            window.draw(cx).clear(cx);
            assert!(window.find("drawer-settings").visible());
            // The drawer's body is up (the server book's Add button rides
            // its top section — the scroll container itself paints nothing).
            assert!(window.find("server-add").visible());
            // The chat below stays mounted — the drawer replaced nothing.
            assert!(window.find("composer").visible());
            // Left-anchored flush against the rail, full height below the
            // status bar (36px): the inset positioning resolves on the FIRST
            // layout, not only after a resize.
            let drawer = window.find("drawer-settings").bounds();
            assert_eq!(drawer.origin.x, px(super::RAIL_WIDTH));
            assert_eq!(drawer.size.width, px(super::DRAWER_WIDTH));
            // Below the h_9 (36px) status bar: the content region's top.
            assert_eq!(drawer.origin.y, px(36.));
            assert!(
                drawer.size.height > px(600.),
                "the drawer spans the content region's height"
            );
            assert!(
                runtime
                    .recorded_commands()
                    .iter()
                    .any(|c| matches!(c, UiCommand::RefreshQuota)),
                "opening the drawer seeds the quota read"
            );
            wb.update(cx, |wb, cx| wb.on_stop_turn(&super::StopTurn, window, cx));
        })
        .unwrap();
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            assert!(
                window.try_find("drawer-settings").is_none(),
                "Esc closes the settings drawer"
            );
        })
        .unwrap();

        // The rail's quota drawer: the all-providers display, purely
        // view-local — no driver page opens under it.
        cx.update_window(handle.into(), |_, window, cx| {
            window.click("dock-rail-quota", cx);
            window.draw(cx).clear(cx);
            assert!(window.find("drawer-quota").visible());
            assert!(window.find("quota-drawer-body").visible());
            assert!(
                window.find("composer").visible(),
                "still no surface replaced"
            );
            assert!(window.try_find("quota-back").is_none(), "not the plan page");
            wb.update(cx, |wb, cx| wb.on_stop_turn(&super::StopTurn, window, cx));
        })
        .unwrap();
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            assert!(
                window.try_find("drawer-quota").is_none(),
                "Esc closes the quota drawer"
            );
            assert!(
                !runtime
                    .recorded_commands()
                    .iter()
                    .any(|c| matches!(c, UiCommand::CloseQuota | UiCommand::OpenQuota)),
                "the drawer quota is view-local — no driver page state touched"
            );
        })
        .unwrap();

        // The plan-quota PAGE (settings entry → OpenQuota): a full page that
        // replaces the surfaces and dismisses any open drawer.
        cx.update_window(handle.into(), |_, window, cx| {
            window.click("dock-rail-quota", cx);
            let mut snap = connected_snapshot();
            snap.quota_page = Some(crate::runtime::QuotaPageView::default());
            wb.update(cx, |wb, cx| {
                wb.on_runtime_event(RuntimeEvent::Snapshot(snap), cx)
            });
            window.draw(cx).clear(cx);
            assert!(window.find("quota-back").visible(), "the plan page mounted");
            assert!(
                window.try_find("composer").is_none(),
                "a page replaces the chat"
            );
            assert!(
                window.try_find("drawer-quota").is_none(),
                "the page dismisses the drawer"
            );
            // The page fills the content region on the FIRST layout (the
            // wrapper must not clip it until a resize): the body's bottom
            // edge is the window's (700px in this harness).
            let page_body = window.find("quota-body").bounds();
            assert!(
                page_body.bottom() >= px(695.),
                "the page body reaches the region's bottom, not a truncated slice"
            );
        })
        .unwrap();
    }

    /// The composer is bound to the ACTIVE session: a tab switch parks the
    /// outgoing tab's draft and adopts the incoming one's — text typed for
    /// one conversation can never ride into another (send targets the
    /// active tab), and switching back restores what was parked.
    #[gpui_kit::test]
    fn switching_tabs_parks_and_adopts_the_composer_draft(cx: &mut TestAppContext) {
        let _windows = super::window_test_lock();
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });

        let second = TabSummary {
            id: 8,
            session_id: Some("sess_2".to_string()),
            workspace: None,
            session_title: None,
            phase: TabPhase::Connected,
            running: false,
            pending: 0,
            awaiting: false,
        };
        let with_active = |id: u64| {
            let mut snap = connected_snapshot();
            snap.tabs.push(second.clone());
            snap.active_tab = Some(id);
            snap
        };
        let (handle, wb, _rt) = cx.update(|cx| open(cx, with_active(7), 1000.));
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            wb.update(cx, |wb, cx| {
                wb.composer
                    .update(cx, |state, cx| state.set_value("for sess_1", window, cx));
            });
        })
        .unwrap();

        // Switch to tab 8: the draft parks under tab 7 and the composer
        // adopts tab 8's (empty — a fresh session's draft).
        cx.update_window(handle.into(), |_, _, cx| {
            wb.update(cx, |wb, cx| {
                wb.on_runtime_event(RuntimeEvent::Snapshot(with_active(8)), cx)
            });
        })
        .unwrap();
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            assert_eq!(
                wb.read(cx).composer.read(cx).value().to_string(),
                "",
                "the incoming tab's draft starts empty"
            );
        })
        .unwrap();

        // Type for sess_2 and switch back: sess_1's parked draft returns,
        // and sess_2's parks under tab 8.
        cx.update_window(handle.into(), |_, window, cx| {
            wb.update(cx, |wb, cx| {
                wb.composer
                    .update(cx, |state, cx| state.set_value("for sess_2", window, cx));
            });
        })
        .unwrap();
        cx.update_window(handle.into(), |_, _, cx| {
            wb.update(cx, |wb, cx| {
                wb.on_runtime_event(RuntimeEvent::Snapshot(with_active(7)), cx)
            });
        })
        .unwrap();
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            assert_eq!(
                wb.read(cx).composer.read(cx).value().to_string(),
                "for sess_1",
                "the parked draft returns on the switch back"
            );
            assert_eq!(
                wb.read(cx)
                    .tab_drafts
                    .get(&8)
                    .map(|(text, _)| text.as_str()),
                Some("for sess_2"),
                "the other tab's draft is parked, not lost"
            );
        })
        .unwrap();
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
            "GLM coding plan · 5h 42% ↻ 2h13m · week 12% ↻ 3d 4h · month 61% · $3.50".to_string(),
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
            assert!(
                bridge.right() <= dock.left(),
                "the bridge label keeps its place"
            );
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
            assert!(
                window.try_find("server-form").is_none(),
                "no form until asked"
            );

            window.click("server-edit-srv-1", cx);
            window.draw(cx).clear(cx);
            assert!(
                window.find("server-form").visible(),
                "Edit opens the dialog"
            );
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

    /// Wheel containment for modal overlays (observed 2026-10-21): gpui's
    /// scroll containers never stop propagation, so a wheel that reaches the
    /// end of the dialog's own list used to CHAIN into the sessions panel
    /// behind it — the user scrolled the popup, hit its end, and the sessions
    /// list under it started moving. The fix is the overlay's BlockMouse
    /// hitbox (`occlude`), which ends the hit test at the overlay: every
    /// surface behind reports `should_handle_scroll() == false`, while the
    /// dialog's own (deeper) scrollers keep scrolling.
    ///
    /// The assertion is positional: the sessions panel's first row must not
    /// move. Without `occlude` the wheel past the list's end scrolls it and
    /// the row's bounds shift.
    #[gpui_kit::test]
    fn a_wheel_at_the_history_dialogs_end_never_scrolls_the_sessions_panel(
        cx: &mut TestAppContext,
    ) {
        let _windows = super::window_test_lock();
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let mut snap = connected_snapshot();
        // Enough sessions for the panel itself to be scrollable — the bug is
        // invisible when the list behind cannot move.
        snap.sessions = (0..30)
            .map(|i| crate::runtime::SessionRow {
                instance_id: "i-1".to_string(),
                workspace: "/w".to_string(),
                session_id: format!("sess_{i}"),
                title: Some(format!("Session {i}")),
                updated_at: Some(1_000 + i as u64),
                running: false,
                awaiting: false,
                finished_at: None,
                rest_running: false,
                activity_known: false,
                open_tab: None,
            })
            .collect();
        // The dialog, open on a project with a list long enough to scroll.
        snap.history.open = true;
        snap.history.projects = Some(vec![crate::runtime::HistoryProjectRow {
            workspace_path: "/w".to_string(),
            sessions: 40,
            last_active: 1_000,
        }]);
        snap.history.selected = Some(crate::runtime::HistorySelected {
            workspace_path: "/w".to_string(),
            rows: (0..40)
                .map(|i| crate::runtime::HistoryRow {
                    session_id: format!("hist_{i}"),
                    title: Some(format!("Past session {i}")),
                    updated_at: Some("2026-10-21T00:00:00Z".to_string()),
                    live: false,
                })
                .collect(),
            next_cursor: None,
            loading_page: false,
            loading_more: false,
            error: None,
            resuming: None,
            deleting: None,
        });

        // Control: with the dialog CLOSED the panel must be scrollable at all,
        // otherwise the containment assertion below proves nothing.
        let mut closed = snap.clone();
        closed.history.open = false;
        let (ctl_handle, _ctl_wb, _ctl_rt) = cx.update(|cx| open(cx, closed, 1200.));
        cx.update_window(ctl_handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            // The dock opens by default — the sessions panel is mounted.
            let b0 = window.find("session-row-sess_0").bounds();
            for _ in 0..8 {
                window.scroll(
                    "sessions-panel",
                    gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.), gpui::px(-200.))),
                    cx,
                );
            }
            let moved = window
                .try_find("session-row-sess_0")
                .map(|row| row.bounds().origin.y != b0.origin.y)
                .unwrap_or(true);
            assert!(
                moved,
                "control: the sessions panel scrolls when nothing covers it"
            );
        })
        .unwrap();

        let (handle, _wb, _rt) = cx.update(|cx| open(cx, snap, 1200.));
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            let before = window.find("session-row-sess_0").bounds();
            assert!(
                window.find("history-dialog-body").visible(),
                "the dialog's own list is on screen"
            );

            // The wheel goes over the strip where the dialog and the sessions
            // panel OVERLAP (the card starts at x≈220, the panel ends at
            // x≈320). The card's centre sits clear of the panel, where a wheel
            // has nothing to chain to and the bug could never show.
            let overlap = gpui::point(gpui::px(270.), gpui::px(300.));
            for _ in 0..8 {
                window.render_frame(cx);
                window.dispatch_event(
                    gpui::ScrollWheelEvent {
                        position: overlap,
                        delta: gpui::ScrollDelta::Pixels(gpui::point(
                            gpui::px(0.),
                            gpui::px(-200.),
                        )),
                        ..Default::default()
                    }
                    .to_platform_input(),
                    cx,
                );
            }
            window.render_frame(cx);
            let after = window.find("session-row-sess_0").bounds();
            assert_eq!(
                before.origin.y, after.origin.y,
                "the sessions panel under the dialog must not move ({before:?} -> {after:?})"
            );

            // The dialog's own list DID move (the containment is not a dead
            // zone), and its first row is gone while a later one shows.
            assert!(
                !window.find("history-row-hist_0").visible(),
                "the dialog's list scrolled off its first row"
            );
        })
        .unwrap();
    }

    /// The role split is positional, not a label (2026-10-21): the user's
    /// message hangs right in a capped bubble, the agent's reply runs
    /// full-width and borderless. This replaced a "You"/"Agent" gutter —
    /// one agent per conversation made the text meaningless, and the usual
    /// chat convention (ChatView.tsx `items-end max-w-[85%]` vs a plain
    /// full-width assistant block) carries the same information for free.
    #[gpui_kit::test]
    fn the_user_bubble_hangs_right_while_the_agent_reply_runs_full_width(cx: &mut TestAppContext) {
        let _windows = super::window_test_lock();
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let mut snap = connected_snapshot();
        let mut active = crate::runtime::TabView {
            session_id: Some("sess_1".to_string()),
            agent_version: None,
            phase: TabPhase::Connected,
            running: false,
            loading_session: false,
            messages: vec![
                protocol::chat::ChatMessage {
                    id: "m_user".to_string(),
                    role: ChatRole::User,
                    parts: vec![protocol::chat::ChatPart::Text {
                        text: "a question".to_string(),
                    }],
                    created_at: 1,
                    collapsed: None,
                },
                protocol::chat::ChatMessage {
                    id: "m_agent".to_string(),
                    role: ChatRole::Assistant,
                    parts: vec![protocol::chat::ChatPart::Text {
                        text: "an answer".to_string(),
                    }],
                    created_at: 2,
                    collapsed: None,
                },
            ],
            pending: Vec::new(),
            has_more: false,
            total_messages: None,
            loading_earlier: false,
            approval: None,
            config_options: Vec::new(),
            usage: None,
            cache_hit: None,
            turn_stats: None,
            plan_entries: None,
            fs_capable: false,
            available_commands: Vec::new(),
        };
        active.session_id = Some("sess_1".to_string());
        snap.active = Some(active);

        let (handle, _wb, _rt) = cx.update(|cx| open(cx, snap, 1000.));
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            let bubble = window.find("user-bubble-m_user").bounds();
            let agent = window.find("agent-row-m_agent").bounds();
            // The bubble hangs RIGHT on the agent row's right edge. The
            // 85% cap is asserted as a bound, not as a left-edge gap: a
            // message long enough to fill the cap starts close to the
            // agent's x, which is exactly why the tint and border (checked
            // in the source below) carry the role, not the offset.
            assert!(
                bubble.size.width <= agent.size.width * 0.85 + px(1.),
                "the bubble respects the 85% cap ({bubble:?} vs agent {agent:?})"
            );
            let bubble_right = bubble.origin.x + bubble.size.width;
            let agent_right = agent.origin.x + agent.size.width;
            assert!(
                (bubble_right - agent_right).abs() < px(2.),
                "the bubble is flush with the right edge ({bubble_right:?} vs {agent_right:?})"
            );
            assert!(
                bubble.origin.x > agent.origin.x,
                "the bubble does not start left of the agent row ({bubble:?} vs {agent:?})"
            );
        })
        .unwrap();
    }

    /// Regression (the 14:51 SIGABRT, 2026-10-21): the config pages' delete
    /// controls read their armed flag with
    /// `cx.entity().read_with(cx, ..)` from INSIDE `render`. gpui leases the
    /// entity for the render, so that second lease panics with "cannot read
    /// … while it is already being updated" the moment a deletable row paints
    /// — the app died as soon as the Models page showed a model. The flag is
    /// now resolved by the caller, which already holds `&Workbench`.
    #[gpui_kit::test]
    fn a_deletable_row_renders_without_re_leasing_the_workbench(cx: &mut TestAppContext) {
        use protocol::config_decode::{ModelRef, ModelsBlock, ModelsPayload};

        let _windows = super::window_test_lock();
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });

        let mut snap = connected_snapshot();
        snap.models_page = Some(crate::runtime::ModelsPageView {
            supported: Some(true),
            loading: false,
            error: None,
            models: Some(ModelsPayload {
                ok: Some(true),
                models: Some(ModelsBlock {
                    available: Some(vec![ModelRef {
                        provider_id: Some("prov_1".to_string()),
                        provider_name: Some("Provider One".to_string()),
                        model_id: Some("model_1".to_string()),
                    }]),
                    providers: None,
                    model_rules: None,
                }),
            }),
            busy: false,
        });

        let (handle, _wb, _rt) = cx.update(|cx| open(cx, snap, 1000.));
        cx.update_window(handle.into(), |_, window, cx| {
            // Painting the page is the regression: before the fix this frame
            // aborted the process on the double lease.
            window.draw(cx).clear(cx);
            assert!(
                window.find("model-delete-0").visible(),
                "the deletable row's control rendered (its presence is what \
                 used to trigger the panic)"
            );
        })
        .unwrap();
    }

    /// Regression (the edit-form crash, 2026-10-21): opening the model edit
    /// form and painting it. The form reads its own field entities
    /// (`form.model_id.read(cx)`, `form.context_window.read(cx)`) while the
    /// WORKBENCH is leased for this render — legal, because those are
    /// different entities — but any helper that reached back for the
    /// workbench itself (`cx.entity().read_with(cx, ..)`, as the delete
    /// control used to) aborts the process here. Painting is the assertion.
    #[gpui_kit::test]
    fn the_model_edit_form_paints_without_re_leasing_the_workbench(cx: &mut TestAppContext) {
        use protocol::config_decode::{ModelRef, ModelsBlock, ModelsPayload};

        let _windows = super::window_test_lock();
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });

        let mut snap = connected_snapshot();
        snap.models_page = Some(crate::runtime::ModelsPageView {
            supported: Some(true),
            loading: false,
            error: None,
            models: Some(ModelsPayload {
                ok: Some(true),
                models: Some(ModelsBlock {
                    available: Some(vec![ModelRef {
                        provider_id: Some("prov_1".to_string()),
                        provider_name: Some("Provider One".to_string()),
                        model_id: Some("model_1".to_string()),
                    }]),
                    providers: None,
                    model_rules: None,
                }),
            }),
            busy: false,
        });

        let (handle, wb, _rt) = cx.update(|cx| open(cx, snap, 1000.));
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            // The row's edit control — the gesture that crashed.
            let target = ModelRef {
                provider_id: Some("prov_1".to_string()),
                provider_name: Some("Provider One".to_string()),
                model_id: Some("model_1".to_string()),
            };
            wb.update(cx, |this, cx| {
                this.open_models_form(Some(target), window, cx)
            });
            // Painting the OPEN form is the regression: the context window
            // and model id inputs are read during render, and the save
            // button's canSubmit check reads them again.
            window.draw(cx).clear(cx);
            assert!(
                window.find("model-form-body").visible(),
                "the edit form rendered with its fields"
            );
            assert!(
                window.find("model-save").visible(),
                "the save control rendered (its disabled state reads the form)"
            );
        })
        .unwrap();
    }
}
