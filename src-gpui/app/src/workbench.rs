//! The workbench window view (requirements §6): connection status bar
//! (bootstrap phase + hub address), the left dock (ADR-0009's dock-style
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
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{
    Input, InputEvent, InputState, Textarea, TextareaState,
};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::text::{TextView, TextViewMotion, TextViewState};
use gpui_kit::component::{
    h_resizable, resizable_panel, ActiveTheme as _, Disableable as _, Icon, IconName,
    ResizableState, Sizable as _, WindowExt as _,
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
use crate::i18n::fill;
use crate::models_page::ModelsForm;
use crate::runtime::{
    FsBrowseView, HistoryRow, HistorySelected, NewSessionView, Phase, ProtocolRuntime,
    RuntimeEvent, SessionRow, TabPhase, TabSummary, TabView, UiCommand, WorkbenchSnapshot,
};
use crate::quota_page::SpendStage;
use crate::system_status::render_system_status_page;
use crate::usage_page::render_usage_page;

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

/// Which panel the left dock shows (ADR-0009: sessions/files/settings as
/// dockable panels around the chat area — files stay deferred P1).
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
    /// The sessions panel's search field — the ⌘K target (TS SessionList's
    /// "Search sessions": title or workspace base name, substring).
    session_search: Entity<InputState>,
    /// The new-session chooser's known-projects filter (TS ProjectCreateDialog):
    /// matches the workspace path case-insensitively (the name is its last
    /// segment, so a name hit is a path hit).
    new_session_filter: Entity<InputState>,
    _ns_filter_sub: Subscription,
    _search_sub: Subscription,
    /// The Servers section's add-server form (Connection Profile book,
    /// CONTEXT.md): name / hub URL / bearer token.
    add_server_name: Entity<InputState>,
    add_server_url: Entity<InputState>,
    add_server_token: Entity<InputState>,
    /// The Models config page's add/edit form (TS ModelForm local state):
    /// `None` = closed. Rendered INSTEAD of the page while open (the TS
    /// ConfigFormSheet replaces the section page outright).
    pub(crate) models_form: Option<ModelsForm>,
    /// Keeps the form inputs' change subscriptions alive.
    pub(crate) models_form_subs: Vec<Subscription>,
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
    _composer_sub: Subscription,
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
    /// collapsed by default like the TS `<details>`.
    ToolCall {
        label: SharedString,
        status: String,
        detail_head: String,
        detail: String,
        fold_kind: Option<String>,
        expanded: bool,
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

/// Context gauge as the TUI prints it (ChatScreen.tsx `fmtContext`) —
/// `17.5%/1.0M`, a one-decimal percent of the model window, clamped at 100.
fn fmt_context(used: u64, size: u64) -> String {
    let pct = if size == 0 {
        100.0
    } else {
        ((used as f64 / size as f64) * 100.0).min(100.0)
    };
    format!("{pct:.1}%/{}", fmt_tokens(size))
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
                ..
            },
            ChatPart::ToolCall(tc),
        ) => {
            let head = tool_detail_head(&tc.detail);
            if *status == tc.status
                && *detail_head == head
                && label.as_ref() == tc.tool_name
                && *detail == tc.detail
                && *fold_kind == tc.fold_kind
            {
                return false;
            }
            *label = tc.tool_name.clone().into();
            *status = tc.status.clone();
            *detail_head = head;
            *detail = tc.detail.clone();
            *fold_kind = tc.fold_kind.clone();
            true
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Workbench
// ---------------------------------------------------------------------------

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
                this.submit_composer(cx);
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
        let add_server_name =
            cx.new(|cx| InputState::new(window, cx).placeholder("Name (e.g. dev box)"));
        let add_server_url =
            cx.new(|cx| InputState::new(window, cx).placeholder("https://hub.example:8377"));
        let add_server_token =
            cx.new(|cx| InputState::new(window, cx).placeholder("Bearer token"));
        let dock_sizes = cx.new(|_| ResizableState::default());
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
            session_search,
            _search_sub: search_sub,
            new_session_filter,
            _ns_filter_sub: ns_filter_sub,
            add_server_name,
            add_server_url,
            add_server_token,
            models_form: None,
            models_form_subs: Vec::new(),
            quota_confirm: None,
            copied_row: None,
            plan_open: false,
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
        }
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

    /// Esc: close the topmost layer first — the new-session sheet swallows
    /// it (the TS back-handler stack, `creating` guard included via the
    /// command's no-op-while-busy); otherwise it is ChatView's Cancel —
    /// `session/cancel` on the running turn.
    fn on_stop_turn(&mut self, _: &StopTurn, _window: &mut Window, cx: &mut Context<Self>) {
        if self.snap.new_session.is_some() {
            self.runtime.send(UiCommand::NewSessionToggle);
        } else if self.tab.as_ref().is_some_and(|t| t.running) {
            self.runtime.send(UiCommand::CancelTurn);
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
                // The spend gesture is page-local state (TS useState); a
                // page closed under an open confirm dialog drops it.
                if self.snap.quota_page.is_none() {
                    self.quota_confirm = None;
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
            // long address/note can never push the right cluster off-screen.
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(theme.foreground)
                            .child(label),
                    )
                    .when_some(self.snap.hub_addr.clone(), |this, addr| {
                        this.child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(11.))
                                .font_family(theme.mono_font_family.clone())
                                .text_color(theme.muted_foreground)
                                .child(addr),
                        )
                    })
                    .when_some(self.snap.bootstrap_note.clone(), |this, note| {
                        this.child(
                            div()
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

        // Right cluster: the active tab's title-bar stats (E1 — ChatScreen's
        // header line: context `17.5%/1.0M`, bare cache percent, `tok/s`),
        // running-session count (the quit guard's number), the turn state,
        // and its bridge agent.
        let mut right = div().flex().items_center().gap_3().ml_auto().flex_none();
        if let Some(tab) = self.tab.as_ref() {
            if let Some(usage) = tab.usage.filter(|u| u.size > 0) {
                right = right.child(
                    div()
                        .id("stat-context")
                        .flex_none()
                        .text_size(px(11.))
                        .font_family(theme.mono_font_family.clone())
                        .text_color(theme.muted_foreground)
                        .child(format!("ctx {}", fmt_context(usage.used, usage.size)))
                        .test_support(),
                );
            }
            if let Some(hit) = tab.cache_hit {
                right = right.child(
                    div()
                        .id("stat-cache")
                        .flex_none()
                        .text_size(px(11.))
                        .font_family(theme.mono_font_family.clone())
                        .text_color(theme.muted_foreground)
                        .child(format!("cache {hit}%"))
                        .test_support(),
                );
            }
            if let Some(stats) = tab.turn_stats {
                right = right.child(
                    div()
                        .id("stat-tps")
                        .flex_none()
                        .text_size(px(11.))
                        .font_family(theme.mono_font_family.clone())
                        .text_color(theme.muted_foreground)
                        .child(format!("{} tok/s", stats.tps))
                        .test_support(),
                );
            }
        }
        if self.snap.running_session_count > 0 {
            right = right.child(
                div()
                    .flex_none()
                    .text_size(px(11.))
                    .text_color(theme.accent)
                    .child(format!(
                        "{} session{} running",
                        self.snap.running_session_count,
                        if self.snap.running_session_count == 1 {
                            ""
                        } else {
                            "s"
                        }
                    )),
            );
        } else if self.tab.as_ref().is_some_and(|t| t.running) {
            right = right.child(
                div()
                    .flex_none()
                    .text_size(px(11.))
                    .text_color(theme.accent)
                    .child("turn running"),
            );
        }
        if let Some(agent) = self.tab.as_ref().and_then(|t| t.agent_version.as_deref()) {
            right = right.child(
                div()
                    .flex_none()
                    .truncate()
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child(format!("bridge {agent}")),
            );
        }
        bar = bar.child(right);
        // Context progress hairline (ChatScreen's header-bottom gauge): the
        // fill sits on the bar's bottom edge, clamped at 100%. Inside the
        // bar's own bounds, so no later sibling can overpaint it.
        if let Some(usage) = self
            .tab
            .as_ref()
            .and_then(|t| t.usage)
            .filter(|u| u.size > 0)
        {
            let pct = (((usage.used as f32 / usage.size as f32) * 100.0).min(100.0) / 100.0) as f32;
            bar = bar.child(
                div()
                    .id("context-progress")
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .h(px(2.))
                    .w(relative(pct))
                    .bg(theme.accent)
                    .test_support(),
            );
        }
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
                .child(self.render_chat_column(cx))
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
                            .child(resizable_panel().child(self.render_chat_column(cx))),
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
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child(
                        "No hub is running and the config has no access token. Confirming \
                         writes the token below into your zcode-acp config, then starts the hub.",
                    ),
            )
            .child(
                div()
                    .text_size(px(12.))
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
                    .text_size(px(11.))
                    .font_family(theme.mono_font_family.clone())
                    .text_color(theme.muted_foreground)
                    .child(preview.to_string()),
            );
        if let Some(err) = error {
            card = card.child(
                div()
                    .text_size(px(12.))
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
            .on_click(move |_, _, cx| {
                wb.update(cx, |wb, cx| wb.toggle_dock_panel(panel, cx));
            })
    }

    /// The docked configuration panel: the entry list for the high-frequency
    /// config pages (requirements §4). The TS app reaches these from the
    /// settings panel; the workbench's equivalent home is this dock panel —
    /// the pages themselves stay full pages (ADR-0009).
    /// The Servers section of the config dock — the Connection Profile book
    /// (CONTEXT.md): the implicit local hub, every saved remote hub, and the
    /// add form. Switching restarts the dial on the new target.
    fn render_servers_section(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let runtime = self.runtime.clone();
        let mut column = div().flex().flex_col().gap_1();

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
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.foreground)
                            .child("Local hub"),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .child("Spawned or discovered on this machine"),
                    ),
            );
        if !local_active {
            let runtime = runtime.clone();
            local_row = local_row.child(
                Button::new("server-local-switch")
                    .secondary()
                    .small()
                    .label("Switch")
                    .on_click(move |_, _, _| runtime.send(UiCommand::SwitchProfile { id: None })),
            );
        }
        column = column.child(local_row);

        for server in &self.snap.servers {
            let active = self.snap.active_server_id.as_deref() == Some(server.id.as_str());
            let mut row = div()
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
                                .text_size(px(12.))
                                .text_color(theme.foreground)
                                .child(server.name.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
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
                        .secondary()
                        .small()
                        .label("Switch")
                        .on_click(move |_, _, _| {
                            runtime.send(UiCommand::SwitchProfile { id: Some(id.clone()) })
                        }),
                );
            }
            let runtime_remove = runtime.clone();
            let remove_id = server.id.clone();
            row = row.child(
                Button::new(SharedString::from(format!("server-remove-{}", server.id)))
                    .secondary()
                    .small()
                    .label("Remove")
                    .on_click(move |_, _, _| {
                        runtime_remove.send(UiCommand::RemoveServer { id: remove_id.clone() })
                    }),
            );
            column = column.child(row);
        }

        // The add form: name / hub URL / token; Add saves (upsert + activate)
        // and the driver re-dials the new profile.
        let name = self.add_server_name.clone();
        let url = self.add_server_url.clone();
        let token = self.add_server_token.clone();
        let add_runtime = runtime.clone();
        let can_add = !self.add_server_name.read(cx).value().trim().is_empty()
            && !self.add_server_url.read(cx).value().trim().is_empty();
        column = column.child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.muted_foreground)
                        .child("Add remote server"),
                )
                .child(Input::new(&self.add_server_name).small())
                .child(Input::new(&self.add_server_url).small())
                .child(Input::new(&self.add_server_token).small())
                .child(
                    Button::new("server-add")
                        .primary()
                        .small()
                        .label("Add and connect")
                        .disabled(!can_add)
                        .on_click(move |_, window, cx| {
                            let n = name.read(cx).value().trim().to_string();
                            let u = url.read(cx).value().trim().to_string();
                            let t = token.read(cx).value().trim().to_string();
                            name.update(cx, |s, cx| s.set_value("", window, cx));
                            url.update(cx, |s, cx| s.set_value("", window, cx));
                            token.update(cx, |s, cx| s.set_value("", window, cx));
                            add_runtime.send(UiCommand::AddServer {
                                name: n,
                                hub_url: u,
                                token: t,
                            });
                        }),
                ),
        );
        column.into_any_element()
    }

    fn render_config_panel(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let runtime = self.runtime.clone();
        let mut column = div().flex().flex_col().gap_2().p_2();

        column = column.child(
            div()
                .text_size(px(11.))
                .font_family(theme.mono_font_family.clone())
                .text_color(theme.muted_foreground)
                .child("CONFIGURATION"),
        );

        column = column.child(self.render_servers_section(cx));

        // (icon, title, description, entry command constructor). The command
        // is built inside the click closure — click listeners are `Fn`, so a
        // prebuilt command would have to be Clone for nothing.
        let entries: [(
            KitIconName,
            &str,
            &str,
            fn() -> UiCommand,
        ); 4] = [
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
        ];
        for (icon, title, description, open) in entries {
            let runtime = runtime.clone();
            column = column.child(
                div()
                    .id(SharedString::from(format!("config-entry-{title}")))
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
                                    .text_size(px(12.))
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

        // Section header: sessions + refresh.
        let runtime = self.runtime.clone();
        column = column.child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .text_size(px(11.))
                        .font_family(theme.mono_font_family.clone())
                        .text_color(theme.muted_foreground)
                        .child(t!("chat.sessions").to_uppercase()),
                )
                .child(div().flex_1())
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
                    .text_size(px(11.))
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
        let mut dot = div().size(px(6.)).flex_none().rounded_full();
        dot = if sess.running {
            dot.bg(theme.accent)
        } else if sess.awaiting {
            dot.border_1().border_color(theme.warning_foreground)
        } else {
            dot.bg(theme.muted_foreground.opacity(0.4))
        };
        let instance_id = sess.instance_id.clone();
        let session_id = sess.session_id.clone();
        div()
            .id(SharedString::from(format!("session-row-{}", sess.session_id)))
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
                    .text_size(px(12.))
                    .text_color(theme.foreground)
                    .child(title),
            )
            .when(sess.open_tab.is_some(), |this| {
                this.child(
                    div()
                        .text_size(px(10.))
                        .text_color(theme.muted_foreground)
                        .child("open"),
                )
            })
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
                        .text_size(px(11.))
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
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .child(t!("historyDialog.loading").into_owned()),
                    );
                }
                (None, Some(err)) => {
                    column = column.child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.danger)
                            .child(err.clone()),
                    );
                }
                (Some(list), _) => {
                    if list.is_empty() {
                        column = column.child(
                            div()
                                .text_size(px(11.))
                                .text_color(theme.muted_foreground)
                                .child("No known projects yet."),
                        );
                    }
                    for p in list {
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
                                        .text_size(px(12.))
                                        .text_color(theme.foreground)
                                        .child(project_name(&p.workspace_path)),
                                )
                                .child(
                                    div()
                                        .text_size(px(10.))
                                        .text_color(theme.muted_foreground)
                                        .child(format!("{}", p.sessions)),
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
                    .text_size(px(11.))
                    .text_color(theme.danger)
                    .child(err.clone()),
            );
        }
        if selected.loading_page {
            column = column.child(
                div()
                    .text_size(px(11.))
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
        let resuming = self
            .snap
            .history
            .selected
            .as_ref()
            .and_then(|s| s.resuming.as_ref())
            .map(|r| r == &row.session_id)
            .unwrap_or(false);
        let workspace = workspace_path.to_string();
        let session_id = row.session_id.clone();
        div()
            .id(SharedString::from(format!("history-row-{}", row.session_id)))
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
                    .text_size(px(12.))
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
                    } else if row.live {
                        "live".to_string()
                    } else {
                        row.updated_at.clone().unwrap_or_default()
                    }),
            )
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
                    .text_size(px(12.))
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

        // Config chips (the TS ConfigSheet at desktop distance): the active
        // tab's model / mode / thought picks, one dropdown each. Only with a
        // live session — the TS guard (`connState === "open"` + active id).
        if let Some(view) = self.snap.active.as_ref() {
            if matches!(view.phase, TabPhase::Connected) && !view.config_options.is_empty() {
                strip = strip.child(
                    div()
                        .ml_auto()
                        .flex()
                        .items_center()
                        .gap_1()
                        .min_w_0()
                        .pr_1()
                        .children(
                            view.config_options
                                .iter()
                                .map(|opt| self.render_config_chip(opt, cx)),
                        ),
                );
            }
        }
        strip.into_any_element()
    }

    /// One model/mode/thought chip (the TS ConfigSheet as a toolbar
    /// dropdown): the current value's display name labels the button, the
    /// menu lists every choice with a check on the active row; picking a
    /// non-active row dispatches `session/set_config_option` and the menu
    /// dismisses itself (the popover follows its DismissEvent).
    fn render_config_chip(
        &self,
        opt: &protocol::acp::ConfigOption,
        _cx: &Context<Self>,
    ) -> AnyElement {
        let runtime = self.runtime.clone();
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
        let title = opt.name.clone().unwrap_or_else(|| opt.id.clone());
        // The builder runs on every open, so hand each run a fresh copy.
        let config_id = opt.id.clone();
        let choices: Vec<(String, String, bool)> = opt
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
        Button::new(SharedString::from(format!("cfg-{}", opt.id)))
            .ghost()
            .xsmall()
            .label(current)
            .tooltip(format!("{title}: pick the session's {title}"))
            .dropdown_menu(move |menu, _, _| {
                let mut m = menu;
                for (value, name, active) in choices.clone() {
                    let runtime = runtime.clone();
                    let config_id = config_id.clone();
                    // `new` (clickable Item), NOT `label` (a disabled caption).
                    m = m.item(
                        PopupMenuItem::new(name).checked(active).on_click(
                            move |_, _, _| {
                                if !active {
                                    runtime.send(UiCommand::SetConfigOption {
                                        config_id: config_id.clone(),
                                        value: value.clone(),
                                    })
                                }
                            },
                        ),
                    );
                }
                m
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
                .py_1_5()
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
                        .text_size(px(11.))
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
                                    .text_size(px(11.))
                                    .text_color(color)
                                    .child(glyph),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .text_size(px(12.))
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
        let title = match (&tab.session_id, &tab.phase) {
            (Some(id), _) => short_id(id, "session"),
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
            .px_3()
            .pt_1()
            .pb_1()
            .rounded_t_md()
            .text_size(px(12.))
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
        tab_view = tab_view.child(div().min_w_0().truncate().child(title));
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
        column
            .child(
                MessageScroller::new("chat", scroller, move |index, _window, cx| {
                    let is_last_row = index + 1 == rows.len();
                    if let Some(row) = rows.get(index) {
                        render_row(row, is_last_row, running, copied.as_deref(), &weak_rows, cx)
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
            let connected = matches!(t.phase, TabPhase::Connected);
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
                        .child("pending"),
                );
                row = row.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.))
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
                            .label("Send now")
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
                    .text_size(px(11.))
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
        controls = controls.child(
            Textarea::new(&self.composer)
                .h(px(84.))
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
        column = column.child(
            div()
                .text_size(px(10.))
                .text_color(theme.muted_foreground)
                .child(
                    "Enter send · Shift+Enter newline · follow-ups queue while running · \
                     paste/drop up to 4 images",
                ),
        );

        div()
            .flex_none()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.title_bar)
            .p_3()
            .child(column)
            .into_any_element()
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
fn render_attachment_echo(src: &Arc<gpui::Image>, theme: &component::Theme) -> gpui::Div {
    div()
        .p_1()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .child(
            img(src.clone())
                .w(px(240.))
                .h(px(240.))
                .rounded_sm()
                .object_fit(gpui::ObjectFit::Contain),
        )
}

fn short_id(id: &str, fallback: &str) -> String {
    if id.is_empty() {
        fallback.to_string()
    } else {
        format!("{}…", &id[..id.len().min(8)])
    }
}

/// ProjectHistoryDialog's `projectName`: the last path segment.
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
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    match row.role {
        ChatRole::User => render_user_row(row, copied, wb, theme).into_any_element(),
        ChatRole::Assistant => {
            render_assistant_row(row, is_last_row, running, copied, wb, theme).into_any_element()
        }
    }
}

fn gutter(label: &str, color: gpui::Hsla) -> gpui::Div {
    div()
        .w_12()
        .flex_none()
        .text_size(px(11.))
        .text_color(color)
        .child(label.to_string())
}

fn render_user_row(
    row: &RowModel,
    copied: Option<&str>,
    wb: &WeakEntity<Workbench>,
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
            .text_size(px(11.))
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
                .text_size(px(11.))
                .text_color(theme.muted_foreground)
                .child(t!("chat.contextHandoff").into_owned()),
        );
    }
    let mut bubble = div().flex().flex_col().gap_2().min_w_0();
    for part in &row.parts {
        bubble = match part {
            PartView::Plain { text } => bubble.child(
                div()
                    .text_size(px(13.))
                    .line_height(relative(1.45))
                    .text_color(theme.foreground)
                    .child(text.clone()),
            ),
            PartView::ImageStub => bubble.child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child("[image attachment]"),
            ),
            PartView::Image { src } => bubble.child(render_attachment_echo(src, theme)),
            PartView::Markdown { state, .. } => bubble.child(
                div()
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
                        .flex()
                        .items_center()
                        .gap_1()
                        .px_3()
                        .py_1_5()
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
                                .text_size(px(11.))
                                .text_color(theme.muted_foreground)
                                .child(label),
                        ),
                );
                if open {
                    card = card.child(
                        div()
                            .id(SharedString::from(format!("thought-body-{}", row.id)))
                            .max_h(px(208.))
                            .overflow_y_scroll()
                            .px_3()
                            .pb_2()
                            .text_color(theme.muted_foreground)
                            .child(TextView::new(state)),
                    );
                }
                column.child(card)
            }
            PartView::Markdown { state, .. } => column.child(TextView::new(state)),
            PartView::Plain { text } => column.child(
                div()
                    .text_size(px(13.))
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
                                .text_size(px(11.))
                                .text_color(color)
                                .child(glyph),
                        )
                        .child(
                            div()
                                .flex_none()
                                .font_family(theme.mono_font_family.clone())
                                .text_size(px(11.))
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
                                    .text_size(px(11.))
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
                    card = card.child(
                        div()
                            .id(SharedString::from(format!(
                                "tool-detail-{}-{}",
                                row.id, part_index
                            )))
                            .max_h(px(256.))
                            .overflow_y_scroll()
                            .min_w_0()
                            .border_t_1()
                            .border_color(theme.border)
                            .px_2()
                            .py_1_5()
                            .font_family(theme.mono_font_family.clone())
                            .text_size(px(11.))
                            .line_height(relative(1.45))
                            .text_color(theme.muted_foreground)
                            .child(if detail.is_empty() {
                                "…".to_string()
                            } else {
                                detail.clone()
                            })
                            .test_support(),
                    );
                }
                column.child(card)
            }
            PartView::ImageStub => column.child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child("[image attachment]"),
            ),
            PartView::Image { src } => column.child(render_attachment_echo(src, theme)),
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
        // The inline approval card reconciles here: its interactive state
        // entities need a window (see reconcile_card).
        self.reconcile_card(window, cx);
        let toast = self.snap.toast.as_ref().map(|t| t.text.clone());
        let runtime = self.runtime.clone();
        let theme = cx.theme();
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
                if self.snap.system_status.is_none()
                    && self.snap.models_page.is_none()
                    && self.snap.quota_page.is_none()
                    && self.snap.usage_page.is_none()
                {
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
            // The new-session chooser rides ON TOP of whatever surface is
            // below (the TS dialog is a fixed full-screen overlay).
            .when_some(
                self.snap.new_session.clone(),
                |this, view| this.child(self.render_new_session(&view, cx)),
            )
    }
}

impl Workbench {
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
                        .text_size(px(11.))
                        .text_color(theme.warning_foreground)
                        .child(err.clone()),
                );
            }
            match &view.projects {
                None => {
                    list = list.child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.muted_foreground)
                            .child("loading projects…"),
                    );
                }
                Some(projects) if projects.is_empty() => {
                    list = list.child(
                        div()
                            .text_size(px(12.))
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
                                .text_size(px(12.))
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
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py_1_5()
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
                                        .text_size(px(12.))
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
                                    format!("{} sessions", p.sessions)
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
                    .text_size(px(11.))
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
                            .text_size(px(11.))
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
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .child("↑"),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.))
                            .font_family(theme.mono_font_family.clone())
                            .text_color(theme.muted_foreground)
                            .child(parent_label),
                    ),
            );
        }
        if !browse.loading && browse.entries.is_empty() && browse.error.is_none() {
            body = body.child(
                div()
                    .text_size(px(11.))
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
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .child("▸"),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
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

#[cfg(test)]
mod chip_tests {
    // Explicit imports only — the file-root `use gpui_kit::*` glob would
    // shadow the built-in `#[test]` with gpui's test macro inside the
    // `#[gpui_kit::test]` expansion (the testing guide's shadowing note).
    use super::Workbench;
    use crate::runtime::{
        Phase, ProtocolRuntime, TabPhase, TabSummary, TabView, UiCommand, WorkbenchSnapshot,
    };
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{
        px, AppContext as _, Bounds, Point, Size, TestAppContext, WindowBounds, WindowOptions,
    };

    fn fixture_options() -> Vec<protocol::acp::ConfigOption> {
        use protocol::acp::{ConfigOption, ConfigOptionChoice};
        let choice = |v: &str, n: &str| ConfigOptionChoice {
            value: v.to_string(),
            name: n.to_string(),
        };
        vec![
            ConfigOption {
                id: "model".to_string(),
                name: Some("Model".to_string()),
                category: None,
                current_value: Some("glm-4.7".to_string()),
                options: Some(vec![
                    choice("glm-4.7", "GLM-4.7"),
                    choice("glm-4-air", "GLM-4-Air"),
                ]),
            },
            ConfigOption {
                id: "mode".to_string(),
                name: Some("Mode".to_string()),
                category: None,
                current_value: Some("code".to_string()),
                options: Some(vec![
                    choice("code", "Code"),
                    choice("plan", "Plan"),
                ]),
            },
            ConfigOption {
                id: "thought".to_string(),
                name: Some("Thinking".to_string()),
                category: None,
                current_value: None,
                options: None,
            },
        ]
    }

    fn connected_snapshot() -> WorkbenchSnapshot {
        let mut snap = WorkbenchSnapshot::initial();
        snap.phase = Phase::Connected;
        snap.tabs = vec![TabSummary {
            id: 7,
            session_id: Some("sess_1".to_string()),
            phase: TabPhase::Connected,
            running: false,
            pending: 0,
            awaiting: false,
        }];
        snap.active_tab = Some(7);
        snap.active = Some(TabView {
            session_id: Some("sess_1".to_string()),
            agent_version: None,
            phase: TabPhase::Connected,
            running: false,
            loading_session: false,
            messages: Vec::new(),
            pending: Vec::new(),
            has_more: false,
            total_messages: None,
            loading_earlier: false,
            approval: None,
            config_options: fixture_options(),
            usage: None,
            cache_hit: None,
            turn_stats: None,
            plan_entries: None,
        });
        snap
    }

    /// The tab toolbar's config chips (batch D): all three render right of
    /// the `+` button inside the strip, the model dropdown lists every
    /// choice in bridge order, and picking one dispatches the exact
    /// `SetConfigOption` command. The wire half (config_id/value spelling)
    /// lives in the protocol tests.
    #[gpui_kit::test]
    fn config_chips_render_and_pick(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            // The dropdown's 150ms entrance slides its surface (TESTING.md:
            // wait out the legacy entrance or disable it); reduced motion
            // adopts the final position on frame one, so clicks land.
            cx.set_reduce_motion(true);
        });
        let runtime = ProtocolRuntime::inert();
        let (handle, _content) = cx.update(|cx| {
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
                    cx.new(|cx| {
                        Workbench::new(window, cx, runtime.clone(), connected_snapshot())
                    })
                },
            )
            .expect("open test window")
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            let model = window.find("cfg-model");
            let mode = window.find("cfg-mode");
            let thought = window.find("cfg-thought");
            assert!(model.visible() && mode.visible() && thought.visible());
            // Cluster sits right of `+` and never spills past the window.
            assert!(window.find("new-tab").bounds().right() <= model.bounds().left());
            assert!(thought.bounds().right() <= px(800.));
            // The chip's label is the CURRENT value's display name.
            assert_eq!(window.find("cfg-model").label().as_deref(), Some("GLM-4.7"));
            // The model menu lists the choices in bridge order; picking the
            // second one dispatches the pick. (The popup's own dismissal is
            // the kit's deferred DismissEvent — not ours to assert.)
            window.click("cfg-model", cx);
            {
                let mut menu = window.within("popup-menu");
                assert_eq!(menu.find(0usize).label().as_deref(), Some("GLM-4.7"));
                assert_eq!(menu.find(1usize).label().as_deref(), Some("GLM-4-Air"));
                menu.click(1usize, cx);
            }
            let sent = runtime.recorded_commands();
            assert!(
                sent.iter().any(|c| matches!(
                    c,
                    UiCommand::SetConfigOption { config_id, value }
                        if config_id == "model" && value == "glm-4-air"
                )),
                "the pick must reach the runtime, got {sent:?}"
            );
        })
        .unwrap();
    }
}

/// Batch E (display details) headless UI integration tests — same paradigm
/// as `chip_tests`: explicit imports, an inert runtime, a real Workbench in
/// a test window, element-id assertions and real clicks.
#[cfg(test)]
mod display_tests {
    use super::Workbench;
    use crate::runtime::{
        Phase, ProtocolRuntime, TabPhase, TabSummary, TabView, WorkbenchSnapshot,
    };
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{
        px, AnyWindowHandle, App, AppContext as _, Bounds, Entity, Point, Size, TestAppContext,
        WindowBounds, WindowOptions,
    };

    fn base_snapshot() -> WorkbenchSnapshot {
        let mut snap = WorkbenchSnapshot::initial();
        snap.phase = Phase::Connected;
        snap.tabs = vec![TabSummary {
            id: 7,
            session_id: Some("sess_1".to_string()),
            phase: TabPhase::Connected,
            running: false,
            pending: 0,
            awaiting: false,
        }];
        snap.active_tab = Some(7);
        snap
    }

    fn messages_fixture() -> Vec<protocol::chat::ChatMessage> {
        use protocol::chat::{ChatMessage, ChatPart, ChatRole, ToolCallPart};
        vec![
            ChatMessage {
                id: "u1".to_string(),
                role: ChatRole::User,
                parts: vec![ChatPart::Text {
                    text: "hello desktop".to_string(),
                }],
                created_at: 1,
                collapsed: None,
            },
            ChatMessage {
                id: "a1".to_string(),
                role: ChatRole::Assistant,
                parts: vec![
                    ChatPart::ToolCall(ToolCallPart {
                        tool_call_id: "t1".to_string(),
                        tool_name: "Bash: npm test".to_string(),
                        detail: "first line of output\nsecond line of output\nthird line"
                            .to_string(),
                        status: "completed".to_string(),
                        kind: Some("execute".to_string()),
                        raw_name: Some("Bash".to_string()),
                        fold_kind: None,
                        diffs: None,
                    }),
                    ChatPart::Text {
                        text: "all green".to_string(),
                    },
                ],
                created_at: 2,
                collapsed: None,
            },
            // A harness task-notification user message (E5): renders as the
            // centered system chip, not a bubble.
            ChatMessage {
                id: "n1".to_string(),
                role: ChatRole::User,
                parts: vec![ChatPart::Text {
                    text: "<task-notification>\n  <status>completed</status>\n  \
                           <summary>nightly build ok</summary>\n</task-notification>"
                        .to_string(),
                }],
                created_at: 3,
                collapsed: None,
            },
        ]
    }

    /// E1: the status bar carries the active tab's context/cache/tps
    /// readouts and the context progress hairline (ChatScreen's header
    /// line, `17.5%/1.0M` formatting included).
    #[gpui_kit::test]
    fn title_stats_render_in_status_bar(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let mut snap = base_snapshot();
        snap.active = Some(TabView {
            session_id: Some("sess_1".to_string()),
            agent_version: None,
            phase: TabPhase::Connected,
            running: false,
            loading_session: false,
            messages: Vec::new(),
            pending: Vec::new(),
            has_more: false,
            total_messages: None,
            loading_earlier: false,
            approval: None,
            config_options: Vec::new(),
            usage: Some(protocol::acp::ContextUsage {
                used: 175_000,
                size: 1_000_000,
            }),
            cache_hit: Some(82),
            turn_stats: Some(protocol::chat::TurnStats {
                tps: 41.0,
                output_tokens: 1_234,
                seconds: 30.1,
            }),
            plan_entries: None,
        });
        let (handle, _content) = cx.update(|cx| open_test_window(cx, snap));
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            assert!(window.find("stat-context").visible());
            assert!(window.find("stat-cache").visible());
            assert!(window.find("stat-tps").visible());
            let progress = window.find("context-progress");
            assert!(progress.visible());
            // 17.5% of the 800px status bar ≈ 140px (clamped fill).
            let width = progress.bounds().size.width;
            assert!(
                (width - px(140.)).abs() < px(8.),
                "progress fill should be ~140px, got {width:?}"
            );
        })
        .unwrap();
    }

    /// E2: a tool-call row expands its full detail (max-h scroll window) on
    /// click and collapses again — the TS `<details>` ToolCard.
    #[gpui_kit::test]
    fn tool_call_expands_detail(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let mut snap = base_snapshot();
        let mut tab = tab_view();
        tab.messages = messages_fixture();
        snap.active = Some(tab);
        let (handle, _content) = cx.update(|cx| open_test_window(cx, snap));
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            let row = window.find("tool-a1-0");
            assert!(row.visible(), "the tool summary row must render");
            assert!(
                window.try_find("tool-detail-a1-0").is_none(),
                "detail starts collapsed"
            );
            window.click("tool-a1-0", cx);
            assert!(
                window.find("tool-detail-a1-0").visible(),
                "the click must expand the detail body"
            );
            window.click("tool-a1-0", cx);
            assert!(
                window.try_find("tool-detail-a1-0").is_none(),
                "a second click collapses it again"
            );
        })
        .unwrap();
    }

    /// E3: the plan/todo strip renders collapsed with its done/total summary
    /// and expands to the entry list on click.
    #[gpui_kit::test]
    fn plan_bar_collapses_and_lists_entries(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let mut snap = base_snapshot();
        let mut tab = tab_view();
        tab.plan_entries = Some(vec![
            protocol::replay::PlanEntry {
                content: "scan the code".to_string(),
                status: Some("completed".to_string()),
            },
            protocol::replay::PlanEntry {
                content: "write the fix".to_string(),
                status: Some("active".to_string()),
            },
        ]);
        snap.active = Some(tab);
        let (handle, _content) = cx.update(|cx| open_test_window(cx, snap));
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            assert!(window.find("plan-toggle").visible());
            assert!(
                window.try_find("plan-entry-0").is_none(),
                "entries start collapsed"
            );
            window.click("plan-toggle", cx);
            assert!(window.find("plan-entry-0").visible());
            assert!(window.find("plan-entry-1").visible());
        })
        .unwrap();
    }

    /// E4 + E5: the copy affordance writes the row's source text to the
    /// platform clipboard (user plain text, assistant markdown source), and
    /// a task-notification renders as the centered chip with no bubble copy
    /// button.
    #[gpui_kit::test]
    fn copy_buttons_write_clipboard_and_notice_chip_has_none(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_reduce_motion(true);
        });
        let mut snap = base_snapshot();
        let mut tab = tab_view();
        tab.messages = messages_fixture();
        snap.active = Some(tab);
        let (handle, _content) = cx.update(|cx| open_test_window(cx, snap));
        cx.update_window(handle.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            assert!(window.find("copy-u1").visible());
            assert!(window.find("copy-a1").visible());
            // The notice row is a chip, and chips have no copy affordance.
            assert!(window.find("notice-n1").visible());
            assert!(window.try_find("copy-n1").is_none());
            window.click("copy-u1", cx);
        })
        .unwrap();
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("hello desktop".to_string())
        );
        cx.update_window(handle.into(), |_, window, cx| {
            window.click("copy-a1", cx);
        })
        .unwrap();
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("all green".to_string())
        );
    }

    fn tab_view() -> TabView {
        TabView {
            session_id: Some("sess_1".to_string()),
            agent_version: None,
            phase: TabPhase::Connected,
            running: false,
            loading_session: false,
            messages: Vec::new(),
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
        }
    }

    fn open_test_window(
        cx: &mut App,
        snap: WorkbenchSnapshot,
    ) -> (AnyWindowHandle, Entity<Workbench>) {
        let runtime = ProtocolRuntime::inert();
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
                cx.new(|cx| Workbench::new(window, cx, runtime.clone(), snap))
            },
        )
        .expect("open test window")
    }
}

/// Pure-logic units of batch E: the TUI-parity formatters and the
/// task-notification parser.
#[cfg(test)]
mod display_unit_tests {
    // Explicit imports only (the file-root kit glob shadows #[test]).
    use super::{fmt_context, fmt_tokens, parse_task_notification, PartView, RowModel};
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
