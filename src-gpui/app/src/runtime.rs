//! The protocol driver: a dedicated tokio runtime thread that owns the hub
//! bootstrap, the discovery list, the Session History browser, and ONE TAB
//! PER SESSION — each tab with its own WebSocket connection and its own
//! [`SessionCore`] state machine (requirements §4: "Multi-session = one
//! WebSocket connection per tab, each with its own `session/load`; hub
//! clients are independent ACP contexts").
//!
//! Composition (requirements §5): every wire shape and semantic lives in the
//! `protocol` crate — Hub Bootstrap (`protocol::bootstrap`), the ACP
//! WebSocket connection with the Desktop Presence identity
//! (`protocol::acp_client`), discovery + history (`protocol::hub_client`),
//! and the session semantics (`protocol::session::SessionCore`, the ported
//! appStore session slice). This layer only ORCHESTRATES those pieces and
//! mirrors their state into [`RuntimeEvent::Snapshot`] for the view — it
//! invents no protocol behavior of its own.
//!
//! Session semantics follow `src/store/appStore.ts`:
//! - openSession (discovery row) → attach that session (`session/load`) on
//!   its instance — here as a new tab with a fresh connection.
//! - resumeProjectSession (history row) → `POST /api/instances` with the
//!   session id so the hub incubates a resume bridge, then attach the same
//!   id on the returned instance.
//! - loadEarlier → the stored replay cursor passed back VERBATIM
//!   (`session/load_earlier`, SessionCore::load_earlier).
//! The history browser's pagination is the ProjectHistoryDialog port:
//! accumulated pages + the server's composite `nextCursor` passed back
//! verbatim (`hub_client::project_sessions`).
//!
//! Deliberately NOT reused: `protocol::reconnect::Supervisor` (the
//! connection-slice supervisor). It owns `session/load` with its own
//! always-full-tail semantics, which would bypass `SessionCore`'s
//! cache-reconcile replay — and it models ONE connection, while the
//! workbench runs one per tab. Tabs keep a plain connect-again-on-close loop
//! instead (the per-tab reconnect below).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use protocol::acp::SessionUpdate;
use protocol::acp_client::{
    parse_session_new_result, session_load_params, session_new_params, AcpConnection,
    AcpConnectionState, AcpHandlers, AcpInitializeResult, ServerRequest, ServerResponder,
    SessionAttachInfo, TurnStateInfo,
};
use protocol::approval::{ApprovalCore, ElicitAnswers, IncomingAsk, PendingAsk};
use protocol::bootstrap::{assess_local_hub, dial_host, ensure_hub, BootstrapEnv, EnsureOptions};
use protocol::chat::ChatMessage;
use protocol::clock::{Clock, TokioClock};
use protocol::config_decode::ModelsPayload;
use protocol::config_store::{
    NOTICE_NEEDS_RESTART, NOTICE_RESET_SPEND_FAILED, NOTICE_RESET_SPENT, NOTICE_RESET_STALE,
    NOTICE_RESTARTED, NOTICE_RESTART_FAILED,
};
use protocol::quota::{AccountUsageStats, QuotaItem};
use protocol::settings::{
    Effect, ModelUpsert, ResetCardInventory, ResetCardStatus, ResetCardType, SettingsAll,
    SettingsPlatformUsage, SettingsUsage, SpendResetCard, WriteEffect,
};
use protocol::hub::{
    FsBrowseResult, FsEntryKind, FsListing, HubCreateInstanceResult, HubHistoryCursor,
    HubHistoryPage, HubHistorySession, HubInstance, HubProject,
};
use protocol::hub_client::{HubApiError, HubClient};
use protocol::session::{LoadEarlierRequest, PromptRequest, SessionCore, WireRequest};
use protocol::settings::BackendRestartResult;
use protocol::settings_client::{HubApiError as SettingsApiError, HubHttpClient};
use protocol::system_stats::SystemStatsResponse;
use protocol::toast::{Toast, ToastState};
use protocol::ws::{TungsteniteWsFactory, WsFactory};
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::file_browser::{file_kind_of, FileKind, VIEWER_LINE_BUDGET};
use crate::http_transport::{LocalHttpTransport, SettingsApiTransport};

/// The 120ms update-coalescing batch the TS store runs (appStore.ts
/// `updateQueue`); the driver's tick is its timer.
const UPDATE_BATCH_MS: u64 = 120;
/// Plain reconnect retry while a tab's socket is down.
const RECONNECT_DELAY_MS: u64 = 1_000;

/// Exponential reconnect backoff, mirroring appStore.ts `scheduleReconnect`:
/// 1s, 2s, 4s… capped at 30s. The shift is clamped so large attempt counts
/// cannot overflow; the cap makes the clamp's ceiling irrelevant.
fn reconnect_delay_ms(attempts: u32) -> u64 {
    RECONNECT_DELAY_MS
        .saturating_mul(1u64 << attempts.min(5))
        .min(30_000)
}
/// Sidebar discovery refresh cadence once the hub answers.
const DISCOVERY_REFRESH_MS: u64 = 15_000;
/// Re-probe cadence while the hub is up but no bridge instance exists yet.
const NO_INSTANCE_RETRY_MS: u64 = 5_000;
/// System-status poll cadence while the page is open (TS SystemStatusPage
/// `POLL_MS`): a live dashboard — and the hub caches one collect for 1s, so
/// clients must not cache.
const SYSTEM_STATS_POLL_MS: u64 = 5_000;

/// Env-gated stderr trace for the driver lifecycle (ZCODE_ACP_TRACE=1) —
/// the slice's connection story, observable without a debugger.
fn trace() -> bool {
    static TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *TRACE.get_or_init(|| std::env::var("ZCODE_ACP_TRACE").ok().as_deref() == Some("1"))
}

macro_rules! traceln {
    ($($arg:tt)*) => {
        if trace() {
            eprintln!($($arg)*);
        }
    };
}

// ---------------------------------------------------------------------------
// UI surface types
// ---------------------------------------------------------------------------

/// Commands from the window (composer, buttons, tab/session management).
/// Dropping the runtime handle (process exit) stands in for an explicit
/// shutdown.
#[derive(Clone, Debug)]
pub enum UiCommand {
    SendPrompt {
        text: String,
        /// Staged attachments (ADR 0007 pipeline, `protocol::image`); empty
        /// for plain text prompts.
        images: Vec<protocol::chat::AttachmentDraft>,
    },
    /// The composer's stop control: `session/cancel` on the active tab.
    CancelTurn,
    /// The tab toolbar's model/mode/thought pick (appStore `setConfigOption`):
    /// `session/set_config_option` on the active tab's session; the answer's
    /// `configOptions` plus the bridge's broadcasts refresh the chips.
    SetConfigOption { config_id: String, value: String },
    /// Manual retry from the status bar (re-runs the whole bootstrap).
    Retry,
    /// Confirms the guided token setup's previewed write (Phase::TokenGuide):
    /// persist the plan, then re-run the bootstrap.
    ConfirmTokenWrite,
    /// Connection Profile switch (CONTEXT.md): activate a saved remote
    /// server by id, or `None` for the implicit local hub.
    SwitchProfile { id: Option<String> },
    /// Save a remote server (upsert by id) and activate it (the TS upsert
    /// semantics: saving means switching to it).
    AddServer {
        name: String,
        hub_url: String,
        token: String,
    },
    /// Remove a saved server; removing the active one falls back to local.
    RemoveServer { id: String },
    /// Empty-composer Enter with a queued follow-up: interrupt the running
    /// turn and send the queue head (ChatView.tsx `forceSendPending`).
    ForceSendPending,
    /// The pending bubble's Send now (ChatView.tsx:934): interrupt the
    /// running turn and send THAT draft first, not the queue head.
    ForceSendPendingAt { index: usize },
    /// The pending bubble's discard (ChatView.tsx:949).
    DiscardPending { index: usize },
    /// The pending bubble's edit (ChatView.tsx:942): the draft leaves the
    /// queue and returns to the composer as a prefill (text + staged images).
    EditPending { index: usize },
    /// The load-earlier control: one page of older history for the active
    /// tab (cursor verbatim, SessionCore::load_earlier).
    LoadEarlier,
    SelectTab { id: u64 },
    CloseTab { id: u64 },
    /// A discovery session row: attach it — a new tab with its own
    /// connection unless one already holds it (appStore `openSession`).
    OpenSession {
        instance_id: String,
        session_id: String,
    },
    /// The tab strip's `+` opens the new-session chooser (the TS
    /// ProjectCreateDialog): known projects plus the fs/browse mode. The
    /// picked directory incubates a serve bridge (`createInstance`) and a
    /// fresh `session/new` tab attaches to it — the user always sees WHICH
    /// directory a new session belongs to.
    NewSessionToggle,
    /// Walk one directory level of the hub machine (the dialog's browse
    /// mode): `None` lists HOME, `Some(path)` uses the RESOLVED path from a
    /// previous answer.
    NewSessionBrowse { path: Option<String> },
    /// Leave browse mode back to the known-projects list.
    NewSessionCloseBrowse,
    /// Pick a workspace: `POST /api/instances` (re)incubates its serve
    /// bridge, then a fresh session/new tab opens in it (appStore
    /// `createProjectSession`).
    NewSessionCreate { workspace_path: String },
    /// A history row: `POST /api/instances` carrying the session id, then
    /// attach on the returned instance (appStore `resumeProjectSession`).
    OpenHistorySession {
        workspace_path: String,
        session_id: String,
    },
    /// Sidebar session-history browser: open/close the panel.
    HistoryToggle,
    /// Drill into one project's paged history.
    HistoryOpenProject { workspace_path: String },
    /// Back to the project chooser.
    HistoryBack,
    /// Next page of the selected project (cursor verbatim).
    HistoryLoadMore,
    /// Manual sidebar refresh (probe=1 — an honest list).
    RefreshSessions,
    /// The approval card's answer to a `session/request_permission` ask
    /// (Permission Race: the first answer wins).
    AnswerPermission {
        request_id: u64,
        option_id: String,
    },
    /// The approval card's answer to an `elicitation/create` form:
    /// `Some(answers)` accepts with content, `None` declines (Cancel).
    AnswerElicitation {
        request_id: u64,
        answers: Option<ElicitAnswers>,
    },
    /// Dismiss the ephemeral toast.
    DismissToast,
    /// Open the System Status config page. Seeds one read (the TS
    /// ZCodeConfigScreen mount effect: loadConfigSection("system") →
    /// loadSystemStats); the driver keeps polling every 5s while open.
    OpenSystemStatus,
    /// The page's back gesture — stop polling. The dashboard state survives
    /// (TS keeps `configSystemStats` in the store), so a reopen paints
    /// instantly and re-seeds.
    CloseSystemStatus,
    /// The page header's refresh control (TS `loadSystemStats`).
    RefreshSystemStatus,
    /// Dismiss the config notice banner (TS `dismissNotice`).
    DismissConfigNotice,
    /// The restart affordance (TS `restartConfigBackend`): POST
    /// `backend/restart` on the ACTIVE TAB's bridge — the desktop's
    /// "connected session" instance.
    RestartBackend,
    /// Open the Models config page. Every open re-runs the TS mount pair:
    /// the /settings/all capability probe (the config entry list) and the
    /// section read (ModelsPage's `loadConfigSection("models")`).
    OpenModels,
    /// The Models page's back gesture — close it.
    CloseModels,
    /// The Models page header's refresh control (the section read again).
    RefreshModels,
    /// The add/edit form's Save: POST /settings/models — an UPSERT — through
    /// the applyConfigWrite path (effect class surfaced, section reloaded).
    /// `adding` only names the failure label (TS `target ? "update model" :
    /// "add model"`).
    UpsertModel {
        adding: bool,
        body: ModelUpsert,
    },
    /// Open the Usage config page. Every open is a fresh mount
    /// (UsagePage.tsx): the platform tab first — the account-level view the
    /// page leads with — then the mount loads (capability probe once, the
    /// active tab's section read).
    OpenUsage,
    /// The Usage page's back gesture — close it. Payloads survive (the TS
    /// store keeps them), so a reopen paints instantly and re-reads.
    CloseUsage,
    /// Switch the Usage page's tab: the TS tab switch REMOUNTS the tab
    /// component, whose effect refetches that tab's section.
    UsageTab { platform: bool },
    /// A Usage range pill: the range controls the upstream query, not a
    /// client-side filter — a tap refetches.
    UsageRange { platform: bool, range: String },
    /// Open the plan-quota config page. Every open re-runs the TS mount
    /// pair: the /settings/all snapshot (capability + card eligibility) and
    /// the quota read (QuotaPage's `loadConfigSection("quota")` →
    /// `refreshUsageStats`).
    OpenQuota,
    /// The page's back gesture — close it. The payloads survive (the TS
    /// store keeps them), so a reopen paints instantly and re-seeds.
    CloseQuota,
    /// The page header's refresh control (TS `refreshUsageStats`).
    RefreshQuota,
    /// The provider picker: read that provider's card inventory (TS
    /// `loadResetCards`).
    QuotaPickProvider { provider_id: String },
    /// The spend confirmation's commit. Provider + nonce were captured at
    /// the gesture (TS `spend`) — re-reading them at send time would let a
    /// provider switch redirect the spend at another plan's card.
    QuotaSpendCard {
        provider_id: String,
        nonce: String,
        reset_type: ResetCardType,
    },
    // ---- Session Files (batch F) ----
    /// List one directory of the ACTIVE session's root (CONTEXT.md "Session
    /// Files"): `""` is the root itself. Every dock of the files panel sends
    /// one (the TS FileBrowser's mount effect); breadcrumb jumps, entering a
    /// subdirectory, and refresh land here too.
    FilesNavigate { path: String },
    /// Open the simplified text viewer for one file (F3). The driver resolves
    /// name/size/kind from the current listing — the row the view clicked.
    FilesView { path: String },
    /// Close the viewer, back to the file list.
    FilesCloseView,
}

/// What the driver surfaces to the window. One shape: a full snapshot —
/// cheap (a few dozen messages in the active tab) and impossible to
/// desynchronize — plus the per-write results a form needs to close
/// itself (TS `if (ok) onClose()`).
pub enum RuntimeEvent {
    Snapshot(WorkbenchSnapshot),
    /// One model upsert settled: `ok` closes the add/edit form; a failure
    /// keeps it open with the notice banner carrying the reason.
    ModelWriteResult { ok: bool },
    /// A queued draft returned to the composer (ChatView `editPending`):
    /// prefill the text and re-stage the images. One-shot — never part of
    /// the snapshot, so it cannot re-fire on a fingerprint change.
    ComposerPrefill(protocol::chat::PromptDraft),
}

/// Hub-level connection/bootstrap phase for the status bar (per-tab
/// connection state lives in [`TabPhase`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Reading config / probing the port / spawning the daemon.
    Bootstrapping,
    /// Hub is up; listing bridge instances.
    Discovering,
    Connected,
    /// Hub REST unreachable; retrying.
    Reconnecting,
    /// Terminal for now: hub bootstrap failed.
    Failed(String),
    /// Guided remote-access setup (requirements §3.1): no hub is listening
    /// and the config carries no token — the preview below is the exact
    /// write a confirmation performs, after which the lifecycle restarts.
    TokenGuide {
        /// Absolute config path the confirmed write targets.
        path: String,
        /// Byte-exact file body the confirmation writes.
        preview: String,
        /// Last apply failure, if any (the card stays up for another try).
        error: Option<String>,
    },
}

impl Phase {
    pub fn label(&self) -> &'static str {
        match self {
            Phase::Bootstrapping => "bootstrapping hub",
            Phase::Discovering => "discovering bridges",
            Phase::Connected => "connected",
            Phase::Reconnecting => "hub unreachable; retrying",
            Phase::Failed(_) => "error",
            Phase::TokenGuide { .. } => "remote setup",
        }
    }
}

/// One tab's own connection lifecycle (each tab owns its WebSocket).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabPhase {
    Connecting,
    Connected,
    /// Socket died or the handshake failed; retrying on the tick.
    Reconnecting,
    /// Terminal for the tab (e.g. session/new refused); close or retry it.
    Failed(String),
}

impl TabPhase {
    pub fn label(&self) -> &'static str {
        match self {
            TabPhase::Connecting => "connecting",
            TabPhase::Connected => "connected",
            TabPhase::Reconnecting => "reconnecting",
            TabPhase::Failed(_) => "error",
        }
    }
}

/// A live session row in the sidebar — discovery's flat list, joined with
/// the Session Activity the tabs' connections broadcast (CONTEXT.md:
/// bridge-wide turnState fan-out; unknown until the next broadcast).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub instance_id: String,
    pub workspace: String,
    pub session_id: String,
    pub title: Option<String>,
    pub updated_at: Option<u64>,
    /// Session Activity `running` badge.
    pub running: bool,
    /// Session Activity `awaiting confirmation` badge (a pending
    /// permission/elicitation ask on one of the tabs).
    pub awaiting: bool,
    /// The tab already holding this session, if any.
    pub open_tab: Option<u64>,
}

/// Tab-strip entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabSummary {
    pub id: u64,
    pub session_id: Option<String>,
    pub phase: TabPhase,
    pub running: bool,
    pub pending: usize,
    /// The tab's session has a pending approval ask (tab badge).
    pub awaiting: bool,
}

/// One queued draft of the active tab's pending queue (ChatView's pending
/// bubbles): what the composer strip paints per draft — the text and the
/// attachment count (thumbnails live only while editing, re-staged from the
/// drafts themselves).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDraftView {
    pub text: String,
    pub images: usize,
}

/// The new-session chooser sheet (the TS ProjectCreateDialog): the hub's
/// known-projects list, with the fs/browse mode alongside it. `None`
/// payloads mean loading; errors keep the last list (the TS dialog keeps
/// painting the cached list under the banner).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewSessionView {
    pub projects: Option<Vec<HistoryProjectRow>>,
    pub error: Option<String>,
    /// `None` = the known-projects list (no browse mode active).
    pub browse: Option<FsBrowseView>,
    /// The workspace a create is incubating for (row/busy state; navigates
    /// are swallowed while set — the TS `creating` guard).
    pub creating: Option<String>,
}

/// One walked directory of the browse mode (`GET /api/fs/list`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FsBrowseView {
    /// The RESOLVED path — navigate by this, not by what was sent.
    pub path: String,
    pub parent: Option<String>,
    /// Subdirectory names, hidden (`.`-prefixed) filtered client-side.
    pub entries: Vec<String>,
    /// A NEW session may start in the listed directory itself (known
    /// project, or under a remote.projectRoots root).
    pub creatable: bool,
    pub truncated: bool,
    pub loading: bool,
    pub error: Option<String>,
}

/// The active tab's session state (the old M1 `SessionSnapshot`).
#[derive(Debug, Clone)]
pub struct TabView {
    pub session_id: Option<String>,
    pub agent_version: Option<String>,
    pub phase: TabPhase,
    pub running: bool,
    pub loading_session: bool,
    pub messages: Vec<ChatMessage>,
    /// The pending queue's drafts, in send order (ChatView's pending
    /// bubbles render one row each).
    pub pending: Vec<PendingDraftView>,
    /// Pagination from the attach response's replayMeta — drives the
    /// load-earlier control (ChatView's "Show N earlier messages").
    pub has_more: bool,
    pub total_messages: Option<u64>,
    pub loading_earlier: bool,
    /// The session's pending approval ask — the inline Approval Card
    /// (CONTEXT.md; shown ONLY in the session that raised it).
    pub approval: Option<PendingAsk>,
    /// The bridge's configOptions (model / mode / thought) — the tab
    /// toolbar's chips and their dropdown lists.
    pub config_options: Vec<protocol::acp::ConfigOption>,
    /// Last-reported context window usage (`usage_update`) — the status
    /// bar's readout and progress hairline (ChatScreen.tsx `usage`).
    pub usage: Option<protocol::acp::ContextUsage>,
    /// Last turn's prompt-cache hit rate in percent (SessionCore
    /// `apply_turn_usage`) — the `cache {n}%` segment.
    pub cache_hit: Option<u32>,
    /// Last completed turn's speed readout — the `tok/s` segment.
    pub turn_stats: Option<protocol::chat::TurnStats>,
    /// The session's current plan/todo entries (`plan` session/update) —
    /// the collapsible strip above the chat (ChatScreen PlanPanel).
    pub plan_entries: Option<Vec<protocol::replay::PlanEntry>>,
    /// The bridge advertised the zcode fs capability (Session Files gating,
    /// TS `fsCapable`): per-session `/fs/list` exists on this bridge.
    pub fs_capable: bool,
}

/// One project row of the history chooser (`GET /api/projects`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryProjectRow {
    pub workspace_path: String,
    pub sessions: u64,
    pub last_active: u64,
}

/// One history-listing row (ISO `updated_at`, not discovery's epoch-ms).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRow {
    pub session_id: String,
    pub title: Option<String>,
    pub updated_at: Option<String>,
    pub live: bool,
}

/// The selected project's accumulated pages plus pagination state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HistorySelected {
    pub workspace_path: String,
    pub rows: Vec<HistoryRow>,
    /// The server's composite cursor — passed back VERBATIM on load-more;
    /// `None` = last page ("Beginning of history").
    pub next_cursor: Option<HubHistoryCursor>,
    pub loading_page: bool,
    pub loading_more: bool,
    pub error: Option<String>,
    /// The row whose resume POST is in flight.
    pub resuming: Option<String>,
}

/// The sidebar's Session History browser (ProjectHistoryDialog port).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HistoryPanelView {
    pub open: bool,
    /// `None` = projects still loading.
    pub projects: Option<Vec<HistoryProjectRow>>,
    pub projects_error: Option<String>,
    pub selected: Option<HistorySelected>,
}

/// The System Status config page's dashboard state (the TS configSlice
/// `configSystemStats*` slice). Rendered only while the page is open.
#[derive(Debug, Clone, Default)]
pub struct SystemStatusPageView {
    /// The last snapshot that landed (survives transient failures — the TS
    /// policy: never paint an error over working content).
    pub stats: Option<SystemStatsResponse>,
    /// First-load failure story (set only while no snapshot exists).
    pub error: Option<String>,
    /// `false` once a 404 retired this page alone (bridge < 0.62.0) — the
    /// standard "hub too old" note, never the global config verdict.
    pub supported: bool,
    /// A read is in flight (the refresh control's spinner).
    pub loading: bool,
}

/// The Models config page state (the TS configSlice fields ModelsPage
/// reads: `configModels`, `configSupported`, `configLoading`,
/// `configError` + the form's busy). Rendered only while the page is open.
#[derive(Debug, Clone, Default)]
pub struct ModelsPageView {
    /// `None` = not probed yet; `Some(false)` = an older hub (the
    /// /settings/all probe answered 404) — the capability card, not an
    /// error.
    pub supported: Option<bool>,
    pub loading: bool,
    pub error: Option<String>,
    /// GET /settings/models, decoded permissively through
    /// `protocol::config_decode` (the module the TS rule says the screens
    /// must call, never fork): an OBJECT with `available` + `modelRules`.
    pub models: Option<ModelsPayload>,
    /// A model write is in flight (the form's busy: Save disabled, label
    /// "Saving…").
    pub busy: bool,
}

/// The Usage page's two tabs (TS `UsageTab`): Platform — the account-level
/// monitor data the desktop app's Coding Plan usage tab shows; Local — what
/// this machine's agent database recorded. Platform is the default, the
/// account-level view the page leads with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UsageTab {
    #[default]
    Platform,
    Local,
}

/// The Usage page state (the TS configSlice fields UsagePage.tsx reads:
/// `configUsage`, `configUsageRange`, `configPlatformUsage`,
/// `configPlatformUsageRange`, `configPlatformUsageSupported`,
/// `configSupported`, `configLoading`). Rendered only while the page is
/// open.
#[derive(Debug, Clone)]
pub struct UsagePageView {
    pub tab: UsageTab,
    /// TS `configSupported`: `None` = not probed yet; `Some(false)` = an
    /// older hub — the whole-page capability card.
    pub supported: Option<bool>,
    /// The shared section-loading flag (TS `configLoading`): the frame's
    /// first-load spinner runs while it is set AND nothing has landed.
    pub loading: bool,
    /// TS `configUsage` (the Local tab's payload; seeded from the
    /// /settings/all snapshot like the TS entry-list load).
    pub local: Option<SettingsUsage>,
    /// TS `configUsageRange` — the LAST range asked for. The pills highlight
    /// from it (the store's record is the authority, not a local mirror).
    pub local_range: String,
    /// TS `configPlatformUsage` (the Platform tab's payload, decoded).
    pub platform: Option<SettingsPlatformUsage>,
    /// TS `configPlatformUsageRange`.
    pub platform_range: String,
    /// `false` = this hub predates the platform route (a 404) — the platform
    /// tab's own upgrade note, never the whole-page verdict.
    pub platform_supported: bool,
    /// A platform payload that failed to decode. No TS counterpart (the
    /// untyped cast cannot fail) — surfaced instead of hidden.
    pub platform_error: Option<String>,
}

/// The plan-quota config page state (the TS configSlice fields QuotaPage
/// reads). Rendered only while the page is open.
#[derive(Debug, Clone, Default)]
pub struct QuotaPageView {
    /// `None` = not probed yet; `Some(false)` = an older hub (the
    /// /settings/all snapshot answered 404) — the capability card, not an
    /// error.
    pub supported: Option<bool>,
    /// The /settings/all snapshot read is in flight.
    pub loading: bool,
    pub error: Option<String>,
    pub quota_unavailable: bool,
    /// The GLM section of the combined quota — the coding-plan surface this
    /// page is (kind + items; the other platforms' windows stay with the
    /// side panels' card).
    pub glm_kind: Option<String>,
    pub glm_items: Vec<QuotaItem>,
    /// Epoch-ms of the last successful quota refresh (the footer stamp).
    pub usage_stats_at: Option<u64>,
    /// The provider ids the hub says COULD own cards (a picker is the norm —
    /// the hub does not know which one this account has).
    pub eligible: Vec<String>,
    /// Whether the machine's credential store could be decrypted at all.
    pub credentials: bool,
    /// The user's provider choice; nothing is pre-selected.
    pub provider_id: Option<String>,
    pub cards: Option<ResetCardInventory>,
    /// The nonce a spend gesture captures alongside the provider — tied to
    /// the status read that minted it.
    pub nonce: Option<String>,
    /// A card read or spend is in flight.
    pub busy: bool,
    /// When a denied reset opportunity said to try again. TS parity: the
    /// store field exists, nothing arms it (the page spends straight off
    /// the status inventory — availability in /status is the only gate).
    pub next_try_at: Option<u64>,
    /// The driver clock at emit (the retry countdown's "future" test).
    pub now_ms: u64,
}

/// The Session Files browser's view (batch F, TS FileBrowser): one loaded
/// directory of the ACTIVE session, plus the open viewer. The listing
/// survives a panel close; a session switch resets it driver-side.
#[derive(Debug, Clone, Default)]
pub struct FilesPanelView {
    /// The relative directory the listing belongs to; `""` = session root.
    pub path: String,
    /// `None` while loading (the TS dialog drops the previous directory so
    /// stale rows cannot join against the new path on tap).
    pub listing: Option<FsListing>,
    pub loading: bool,
    pub error: Option<String>,
    /// The open simplified viewer (F3); `None` = closed.
    pub viewing: Option<FileViewerView>,
}

/// The simplified file viewer's view (F3): read-only, whole-file load.
#[derive(Debug, Clone)]
pub struct FileViewerView {
    pub path: String,
    pub name: String,
    pub size: u64,
    pub kind: FileKind,
    /// The X-Zcode-First-Line the server reported for the loaded window.
    pub first_line: Option<u64>,
    /// The loaded text (None while loading; always None for the deferred
    /// image/binary kinds).
    pub text: Option<String>,
    pub loading: bool,
    pub error: Option<String>,
}

/// The whole workbench state the view renders.
#[derive(Debug, Clone)]
pub struct WorkbenchSnapshot {
    pub phase: Phase,
    /// Dial address, e.g. `http://127.0.0.1:8377`.
    pub hub_addr: Option<String>,
    /// How the hub came to serve (already listening / spawned / race lost),
    /// or the guided-flow hint when remote access is not configured.
    pub bootstrap_note: Option<String>,
    /// Live discovery rows for the sidebar.
    pub sessions: Vec<SessionRow>,
    pub tabs: Vec<TabSummary>,
    pub active_tab: Option<u64>,
    pub active: Option<TabView>,
    pub history: HistoryPanelView,
    /// Distinct sessions whose Session Activity reads `running` across all
    /// tabs — the quit guard's number (requirements §4).
    pub running_session_count: usize,
    /// Ephemeral workbench toast (notify-toast semantics; e.g. an approval
    /// requested in a non-active session).
    pub toast: Option<Toast>,
    /// The System Status config page; `None` = closed (the page replaces the
    /// workbench content below the status bar).
    pub system_status: Option<SystemStatusPageView>,
    /// The Models config page; `None` = closed (a full page of its own, like
    /// every config section — ADR-0009).
    pub models_page: Option<ModelsPageView>,
    /// The Usage config page; `None` = closed (a full page of its own, like
    /// every config section — ADR-0009).
    pub usage_page: Option<UsagePageView>,
    /// The plan-quota config page; `None` = closed (a full page of its own,
    /// like every config section — ADR-0009).
    pub quota_page: Option<QuotaPageView>,
    /// Config-screen notice (TS `notice`): a `notice.*` i18n key or plain
    /// failure text, rendered by every config page's frame.
    pub config_notice: Option<String>,
    /// A needs-restart write has not been applied yet (TS `pendingRestart`)
    /// — drives the Backend Restart affordance on the config pages.
    pub pending_restart: bool,
    /// Connection Profile book (CONTEXT.md): saved remote hubs plus the
    /// active id; `None` active = the local hub (Hub Bootstrap path).
    pub servers: Vec<protocol::hub::SavedServer>,
    pub active_server_id: Option<String>,
    /// The new-session chooser (TS ProjectCreateDialog); `None` = closed.
    pub new_session: Option<NewSessionView>,
    /// The Session Files browser (batch F). Always present — the dock
    /// panel's visibility is view-local, like every dock panel.
    pub files: FilesPanelView,
}

impl WorkbenchSnapshot {
    /// What the window paints before the driver's first snapshot lands.
    pub fn initial() -> Self {
        Self {
            phase: Phase::Bootstrapping,
            hub_addr: None,
            bootstrap_note: None,
            sessions: Vec::new(),
            tabs: Vec::new(),
            active_tab: None,
            active: None,
            history: HistoryPanelView::default(),
            running_session_count: 0,
            toast: None,
            system_status: None,
            models_page: None,
            usage_page: None,
            quota_page: None,
            config_notice: None,
            pending_restart: false,
            servers: Vec::new(),
            active_server_id: None,
            new_session: None,
            files: FilesPanelView::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Loop plumbing
// ---------------------------------------------------------------------------

/// Events from the tabs' pump tasks, the tabs' connect tasks, and the hub
/// REST tasks back into the driver loop. Everything `SessionCore` consumes
/// arrives here, tagged with the owning tab.
enum LoopEvent {
    ConnHandle { tab: u64, conn: AcpConnection },
    ConnectResult { tab: u64, result: Result<AcpInitializeResult, String> },
    Update {
        tab: u64,
        session_id: String,
        update: SessionUpdate,
        meta: Option<Value>,
    },
    TurnState {
        tab: u64,
        session_id: String,
        running: bool,
        turn: Option<TurnStateInfo>,
    },
    /// The tab's socket died. Only meaningful while the tab is Connected —
    /// handshake failures also fire it (via close_core) but own their
    /// retry through [`LoopEvent::ConnectResult`].
    Closed { tab: u64 },
    NewSession { tab: u64, result: Result<SessionAttachInfo, String> },
    LoadResult {
        tab: u64,
        req: protocol::session::LoadRequest,
        result: Result<Value, String>,
    },
    LoadEarlierResult {
        tab: u64,
        session_id: String,
        result: Result<Value, String>,
    },
    PromptResult {
        tab: u64,
        req: PromptRequest,
        result: Result<Value, String>,
    },
    /// `session/set_config_option` settled on a tab.
    SetConfigResult {
        tab: u64,
        result: Result<Value, String>,
    },
    /// A server -> client interaction request (permission / elicitation) on
    /// a tab's connection, with its answer channel.
    ServerRequest {
        tab: u64,
        request: ServerRequest,
        responder: ServerResponder,
    },
    /// `$/cancel_request`: this tab lost the Permission Race for that id.
    CancelRequest { tab: u64, id: u64 },
    /// `$/zcode/ask_settled`: the ask was decided on another client (or the
    /// bridge's wait broke) — dismiss the losing copy of the card.
    AskSettled {
        tab: u64,
        session_id: String,
        kind: Option<String>,
        tool_call_id: Option<String>,
    },
    // ---- hub REST (no tab) ----
    Discovery(Result<Vec<HubInstance>, HubApiError>),
    HistoryProjects {
        seq: u64,
        result: Result<Vec<HubProject>, HubApiError>,
    },
    HistoryPage {
        seq: u64,
        more: bool,
        result: Result<HubHistoryPage, HubApiError>,
    },
    ResumeReady {
        workspace_path: String,
        session_id: String,
        result: Result<HubCreateInstanceResult, HubApiError>,
    },
    // ---- new-session chooser (no tab) ----
    NewSessionProjects {
        seq: u64,
        result: Result<Vec<HubProject>, HubApiError>,
    },
    NewSessionBrowse {
        seq: u64,
        result: Result<FsBrowseResult, HubApiError>,
    },
    NewSessionCreated {
        result: Result<(String, HubCreateInstanceResult), HubApiError>,
    },
    // ---- config pages (no tab) ----
    SystemStats(Result<SystemStatsResponse, HubApiError>),
    BackendRestart(Result<BackendRestartResult, HubApiError>),
    /// The /settings/all capability probe settled (the verdict half of TS
    /// `loadConfigAll`).
    ConfigProbe(Result<(), SettingsApiError>),
    /// GET /settings/models settled (TS `loadConfigSection("models")`).
    ModelsLoaded(Result<Value, SettingsApiError>),
    /// POST /settings/models settled (the applyConfigWrite outcome).
    ModelUpsert {
        adding: bool,
        result: Result<WriteEffect, SettingsApiError>,
    },
    /// GET /settings/all settled for the Usage page (the loadConfigAll
    /// verdict + the local-usage seed the TS entry-list load carries).
    UsageAll(Result<SettingsAll, SettingsApiError>),
    /// One usage section read settled (TS `loadConfigSection("usage"` /
    /// `"usagePlatform")`); `requested` is the range the ask carried (the
    /// stale-response guard's fallback when the answer omits its range).
    UsageSection {
        platform: bool,
        requested: String,
        result: Result<Value, SettingsApiError>,
    },
    /// GET /settings/all settled for the quota page (TS `loadConfigAll`:
    /// capability verdict + reset-card eligibility).
    QuotaSettingsAll(Result<SettingsAll, SettingsApiError>),
    /// GET /settings/quota settled (TS `refreshUsageStats`): `Ok(None)` = a
    /// 2xx body that did not fit the combined shape.
    QuotaUsage {
        result: Result<Option<AccountUsageStats>, SettingsApiError>,
    },
    /// GET /settings/reset-cards settled for one provider (TS
    /// `loadResetCards`).
    QuotaCards {
        provider_id: String,
        result: Result<ResetCardStatus, SettingsApiError>,
    },
    /// POST /settings/reset-cards/use settled (TS `spendResetCard`).
    QuotaSpent { result: Result<Value, SettingsApiError> },
    /// POST /settings/reset-cards/history-read settled (the unread-badge
    /// clear — cosmetic).
    QuotaHistoryRead {
        provider_id: String,
        result: Result<(), SettingsApiError>,
    },
    // ---- Session Files (batch F, hub REST like the browse mode) ----
    /// `/fs/list` settled for the files panel's current directory.
    FilesList {
        seq: u64,
        result: Result<FsListing, HubApiError>,
    },
    /// `/fs/file` settled for the open viewer's single line window.
    FilesText {
        seq: u64,
        result: Result<(u64, String), HubApiError>,
    },
}

/// `AcpHandlers` forwarding into the driver loop's channel, tagged with the
/// tab — including the interaction requests the approval cards answer.
struct DriverHandlers {
    tab: u64,
    tx: UnboundedSender<LoopEvent>,
}

impl AcpHandlers for DriverHandlers {
    fn on_state(&self, state: AcpConnectionState) {
        if state == AcpConnectionState::Closed {
            let _ = self.tx.send(LoopEvent::Closed { tab: self.tab });
        }
    }

    fn on_update(&self, session_id: &str, update: &SessionUpdate, meta: Option<&Value>) {
        let _ = self.tx.send(LoopEvent::Update {
            tab: self.tab,
            session_id: session_id.to_string(),
            update: update.clone(),
            meta: meta.cloned(),
        });
    }

    fn on_server_request(&self, req: ServerRequest, respond: ServerResponder) {
        let _ = self.tx.send(LoopEvent::ServerRequest {
            tab: self.tab,
            request: req,
            responder: respond,
        });
    }

    fn on_cancel_request(&self, id: u64) {
        let _ = self.tx.send(LoopEvent::CancelRequest { tab: self.tab, id });
    }

    fn on_ask_settled(&self, session_id: &str, kind: Option<&str>, tool_call_id: Option<&str>) {
        let _ = self.tx.send(LoopEvent::AskSettled {
            tab: self.tab,
            session_id: session_id.to_string(),
            kind: kind.map(str::to_string),
            tool_call_id: tool_call_id.map(str::to_string),
        });
    }

    fn on_turn_state(&self, session_id: &str, running: bool, turn: Option<TurnStateInfo>) {
        let _ = self.tx.send(LoopEvent::TurnState {
            tab: self.tab,
            session_id: session_id.to_string(),
            running,
            turn,
        });
    }
}

/// Handle for the window side; the driver thread keeps the other halves.
#[derive(Clone)]
pub struct ProtocolRuntime {
    cmd_tx: UnboundedSender<UiCommand>,
    /// Test-only echo of everything `send` accepted — the inert handle has
    /// no driver, so UI tests assert their picks landed here.
    #[cfg(test)]
    recorded: std::sync::Arc<std::sync::Mutex<Vec<UiCommand>>>,
}

impl ProtocolRuntime {
    /// Spawn the driver thread. Returns the handle plus the event stream the
    /// window pumps into its entity.
    pub fn start() -> (ProtocolRuntime, UnboundedReceiver<RuntimeEvent>) {
        let (cmd_tx, cmd_rx) = unbounded_channel();
        let (ev_tx, ev_rx) = unbounded_channel();
        std::thread::Builder::new()
            .name("protocol-driver".to_string())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("build tokio runtime for the protocol driver");
                rt.block_on(driver(cmd_rx, ev_tx));
            })
            .expect("spawn protocol driver thread");
        (ProtocolRuntime {
            cmd_tx,
            #[cfg(test)]
            recorded: Default::default(),
        }, ev_rx)
    }

    pub fn send(&self, cmd: UiCommand) {
        #[cfg(test)]
        self.recorded.lock().expect("recorded lock").push(cmd.clone());
        let _ = self.cmd_tx.send(cmd);
    }

    /// A driver-less handle for window-layer tests: commands vanish — the
    /// driver thread (and its hub bootstrap side effects) never starts.
    /// `recorded_commands` still shows every `send`.
    #[cfg(test)]
    pub fn inert() -> Self {
        let (cmd_tx, _rx) = unbounded_channel();
        Self {
            cmd_tx,
            recorded: Default::default(),
        }
    }

    /// Everything sent through this handle (tests only).
    #[cfg(test)]
    pub fn recorded_commands(&self) -> Vec<UiCommand> {
        self.recorded.lock().expect("recorded lock").clone()
    }
}

// ---------------------------------------------------------------------------
// Driver state
// ---------------------------------------------------------------------------

/// One tab: its own connection + its own SessionCore + its own pending
/// approval asks (the card shows ONLY in the session that raised it — bridge
/// 0.17.0 per-session semantics, CONTEXT.md "Approval Card"). The attach
/// target is remembered so a reconnect re-attaches the same session (replay
/// is the catch-up); `None` means the tab boots a fresh `session/new`.
struct Tab {
    id: u64,
    instance_id: String,
    target: Option<String>,
    cwd: String,
    core: SessionCore,
    /// Pending permission/elicitation asks for this tab's connection, plus
    /// the Permission Race state machine.
    approvals: ApprovalCore,
    /// Answer channels for the pending asks (the TS `pendingResponds` maps).
    responders: HashMap<u64, ServerResponder>,
    conn: Option<AcpConnection>,
    state: TabPhase,
    /// The bridge's advertised agent version (status bar parity with M1).
    agent_version: Option<String>,
    /// Epoch-ms when a Reconnecting tab reconnects.
    retry_at: Option<u64>,
    /// Consecutive failed reconnect attempts; drives the exponential backoff
    /// (mirrors appStore.ts `reconnectAttempt`) and resets on a live connect.
    reconnect_attempts: u32,
    /// The bridge advertised the zcode fs capability on the last
    /// `initialize` (Session Files gating, TS `fsCapable`).
    fs_capable: bool,
}

/// The sidebar's history browser state (driver-owned — all wire I/O lives
/// here; `seq` invalidates in-flight listing responses across open/back,
/// the ProjectHistoryDialog `listSeq` port).
#[derive(Default)]
struct HistoryPanel {
    open: bool,
    projects: Option<Vec<HubProject>>,
    projects_error: Option<String>,
    selected: Option<HistorySelectedInner>,
    seq: u64,
}

#[derive(Default)]
struct HistorySelectedInner {
    workspace_path: String,
    rows: Vec<HubHistorySession>,
    next_cursor: Option<HubHistoryCursor>,
    loading_page: bool,
    loading_more: bool,
    error: Option<String>,
    resuming: Option<String>,
}

/// The new-session chooser state (TS ProjectCreateDialog): driver-owned
/// because all wire I/O lives here. The DATA survives a close (the dialog
/// paints the cached project list instantly on reopen — the TS localStorage
/// cache's in-memory equivalent) and re-fetches on open.
#[derive(Default)]
struct NewSessionPanel {
    open: bool,
    projects: Option<Vec<HubProject>>,
    projects_error: Option<String>,
    browse: Option<FsBrowseView>,
    /// The workspace a create is incubating for (dialog `creating`).
    creating: Option<String>,
    /// Invalidates in-flight project reads across close/reopen; browse
    /// reads carry their own staleness via `browse.loading` (a late
    /// directory answers into the field the user navigated to, keyed by
    /// the resolved path — a stale one just paints and is navigated away).
    seq: u64,
}

/// The System Status page state (the TS configSlice `configSystemStats*`
/// slice): driver-owned because all wire I/O lives here. The DATA survives a
/// page close (the TS store keeps it) and is re-probed on the next open; a
/// full re-bootstrap (Retry) resets it alongside the connection (the TS
/// `connectionResetPatch`: configuration state describes ONE machine).
#[derive(Default)]
struct SystemStatusPanel {
    open: bool,
    stats: Option<SystemStatsResponse>,
    /// First-load failure story only — a transient failure over an existing
    /// snapshot keeps the dashboard (no error paint over working content).
    error: Option<String>,
    /// 404 once — retires the page alone, never the global config verdict.
    supported: bool,
    /// A read in flight (the refresh control's spinner).
    loading: bool,
    /// Epoch-ms when the 5s poll fires next (set when a read spawns).
    next_poll_at: u64,
}

impl SystemStatusPanel {
    /// The connection-reset patch (a new hub may support the route, and the
    /// old machine's snapshot must not flash over the new one's).
    fn reset_data(&mut self) {
        self.stats = None;
        self.error = None;
        // `supported` defaults true in TS; keep `open` — the page re-seeds
        // itself on the next tick after the new hub connects.
        self.supported = true;
        self.loading = false;
        self.next_poll_at = 0;
    }
}

/// The Models config page state (the TS configSlice `configModels*`
/// fields): driver-owned because all wire I/O lives here. The payload
/// survives a page close (the TS store keeps it) and re-probes on the next
/// open; a full re-bootstrap resets it alongside the connection (the TS
/// `connectionResetPatch`: configuration state describes ONE machine).
#[derive(Default)]
struct ModelsPanel {
    open: bool,
    /// `None` = not probed; `Some(false)` = an older hub (bridge < 0.47.0).
    supported: Option<bool>,
    loading: bool,
    error: Option<String>,
    models: Option<ModelsPayload>,
    /// A model write is in flight (the form's busy).
    busy: bool,
}

impl ModelsPanel {
    /// The connection-reset patch — same doctrine as
    /// [`SystemStatusPanel::reset_data`]: the capability verdict and the
    /// machine's payloads never travel across a hub switch. `open`
    /// survives; the steady-state entry re-seeds the reads.
    fn reset_data(&mut self) {
        self.supported = None;
        self.loading = false;
        self.error = None;
        self.models = None;
        self.busy = false;
    }
}

/// The Usage config page state (the TS configSlice `configUsage*` /
/// `configPlatformUsage*` fields): driver-owned because all wire I/O lives
/// here. The payloads survive a page close (the TS store keeps them) and
/// re-read on the next open; a full re-bootstrap resets them alongside the
/// connection (the TS `connectionResetPatch`: configuration state describes
/// ONE machine).
struct UsagePanel {
    open: bool,
    /// The page's tab (TS `UsageTab`); chrome, not machine data — it
    /// survives the connection-reset patch like `open` does.
    tab: UsageTab,
    /// TS `configSupported` (the /settings/all verdict): `None` = not
    /// probed; `Some(false)` = an older hub (bridge < 0.47.0).
    supported: Option<bool>,
    /// TS `configLoading` — the store's shared section-loading flag.
    loading: bool,
    /// TS `configUsage` (the Local tab's payload).
    local: Option<SettingsUsage>,
    /// TS `configUsageRange` — the LAST range asked for. The ask is recorded
    /// BEFORE the await, so an answer landing out of order is recognised as
    /// stale and dropped rather than shown under the wrong pill.
    local_range: String,
    /// TS `configPlatformUsage` (the Platform tab's payload, decoded).
    platform: Option<SettingsPlatformUsage>,
    /// TS `configPlatformUsageRange` — same ask-record discipline.
    platform_range: String,
    /// TS `configPlatformUsageSupported`: `false` = THIS hub predates the
    /// platform route (404) — degrades the platform tab alone.
    platform_supported: bool,
    /// A platform payload that failed to decode (no TS counterpart — the
    /// untyped cast cannot fail); surfaced, never hidden.
    platform_error: Option<String>,
}

impl UsagePanel {
    fn new() -> Self {
        Self {
            open: false,
            tab: UsageTab::default(),
            supported: None,
            loading: false,
            local: None,
            // The TS store's default ranges are "7d".
            local_range: "7d".to_string(),
            platform: None,
            platform_range: "7d".to_string(),
            platform_supported: true,
            platform_error: None,
        }
    }

    /// The connection-reset patch — same doctrine as
    /// [`ModelsPanel::reset_data`]: the capability verdicts, the ranges, and
    /// the machine's payloads never travel across a hub switch. `open` and
    /// `tab` survive; the steady-state entry re-runs the page's mount loads.
    fn reset_data(&mut self) {
        let (open, tab) = (self.open, self.tab);
        *self = Self::new();
        self.open = open;
        self.tab = tab;
    }
}

/// The plan-quota config page state (the TS configSlice reset-card +
/// usage-stats slices QuotaPage reads): driver-owned because all wire I/O
/// lives here. The payloads survive a page close (the TS store keeps them)
/// and re-probe on the next open; a full re-bootstrap resets them alongside
/// the connection (the TS `connectionResetPatch`: configuration state
/// describes ONE machine).
#[derive(Default)]
struct QuotaPanel {
    open: bool,
    /// `None` = not probed; `Some(false)` = an older hub (bridge < 0.47.0).
    supported: Option<bool>,
    /// The /settings/all snapshot read is in flight (the eligibility half of
    /// TS `loadConfigAll`).
    loading: bool,
    error: Option<String>,
    // ---- the allowance (TS `refreshUsageStats`) ----
    usage: Option<AccountUsageStats>,
    quota_unavailable: bool,
    /// Epoch-ms of the last successful quota refresh (TS `usageStatsAt` —
    /// the footer stamp).
    usage_stats_at: Option<u64>,
    // ---- reset cards ----
    /// The provider ids the hub says COULD own cards (a picker is the norm —
    /// the hub does not know which one this account has).
    eligible: Vec<String>,
    /// Whether the machine's credential store could be decrypted at all.
    credentials: bool,
    /// The user's provider choice; nothing is pre-selected.
    provider_id: Option<String>,
    cards: Option<ResetCardInventory>,
    /// What a spend must send back — tied to the read that minted it.
    nonce: Option<String>,
    /// One key per GESTURE, minted alongside the nonce so every retry of
    /// that spend sends the same value.
    idempotency_key: Option<String>,
    busy: bool,
    /// A denied reset opportunity's "try again" moment. Nothing arms it
    /// today — TS parity: the store field exists, the page spends straight
    /// off the status inventory.
    next_try_at: Option<u64>,
}

impl QuotaPanel {
    /// The connection-reset patch — same doctrine as
    /// [`ModelsPanel::reset_data`]: the machine's payloads and the provider
    /// session (the nonce included) never travel across a hub switch. `open`
    /// survives; the steady-state entry re-seeds the reads.
    fn reset_data(&mut self) {
        self.supported = None;
        self.loading = false;
        self.error = None;
        self.usage = None;
        self.quota_unavailable = false;
        self.usage_stats_at = None;
        self.eligible.clear();
        self.credentials = true;
        self.provider_id = None;
        self.cards = None;
        self.nonce = None;
        self.idempotency_key = None;
        self.busy = false;
        self.next_try_at = None;
    }
}

/// The Session Files browser state (TS FileBrowser): driver-owned because
/// all wire I/O lives here. A listing belongs to the (instance, session)
/// that served it — a session switch resets the whole panel (the TS dialog
/// is per-mount), and `seq` invalidates reads still in flight for the
/// previous owner. No cross-directory cache: dockings re-read (the TS mount
/// effect) and up-navigation re-fetches.
#[derive(Default)]
struct FilesPanel {
    /// The (instance_id, session_id) the current state belongs to.
    owner: Option<(String, String)>,
    /// The relative directory being listed; `""` = session root.
    path: String,
    listing: Option<FsListing>,
    loading: bool,
    error: Option<String>,
    viewing: Option<FileViewing>,
    seq: u64,
}

impl FilesPanel {
    fn reset(&mut self) {
        // Everything but nothing — bump seq so in-flight reads from the
        // previous owner/session are dropped on arrival.
        *self = Self {
            seq: self.seq + 1,
            ..Default::default()
        };
    }

    fn view(&self) -> FilesPanelView {
        FilesPanelView {
            path: self.path.clone(),
            listing: self.listing.clone(),
            loading: self.loading,
            error: self.error.clone(),
            viewing: self.viewing.as_ref().map(|v| FileViewerView {
                path: v.path.clone(),
                name: v.name.clone(),
                size: v.size,
                kind: v.kind,
                first_line: v.first_line,
                text: v.text.clone(),
                loading: v.loading,
                error: v.error.clone(),
            }),
        }
    }
}

/// The open viewer's driver half (F3).
#[derive(Debug, Clone)]
struct FileViewing {
    path: String,
    name: String,
    size: u64,
    kind: FileKind,
    first_line: Option<u64>,
    text: Option<String>,
    loading: bool,
    error: Option<String>,
}

struct Driver {
    factory: Arc<dyn WsFactory>,
    clock: Arc<dyn Clock>,
    ev_tx: UnboundedSender<RuntimeEvent>,
    cmd_rx: UnboundedReceiver<UiCommand>,
    loop_tx: UnboundedSender<LoopEvent>,

    phase: Phase,
    hub_url: String,
    token: String,
    note: Option<String>,
    /// Saved Connection Profiles (CONTEXT.md): an active remote entry dials
    /// its hub directly; the local hub is the implicit no-book default.
    book: protocol::profiles::ServerBook,

    instances: Vec<HubInstance>,
    /// Discovery failure note (sidebar hint) — the hub REST call failing.
    discovery_note: Option<String>,
    next_discovery_at: u64,
    discovery_probe: bool,

    tabs: Vec<Tab>,
    active_tab: Option<u64>,
    /// Tabs whose connection WE closed (tab close / Retry) — their Closed
    /// events are expected, not reconnect triggers.
    closing: HashSet<u64>,
    next_tab_id: u64,
    /// Once any tab has existed, discovery stops auto-opening one (the user
    /// expressed intent by closing it); M1 parity until then.
    ever_had_tab: bool,

    history: HistoryPanel,

    /// The new-session chooser (TS ProjectCreateDialog state). `seq`
    /// invalidates in-flight project reads on close, the same contract the
    /// history panel's seq plays for its listings.
    new_session: NewSessionPanel,

    /// The Session Files browser (batch F, see [`FilesPanel`]).
    files: FilesPanel,

    /// The System Status config page (see [`SystemStatusPanel`]).
    system: SystemStatusPanel,
    /// The Models config page (see [`ModelsPanel`]).
    models: ModelsPanel,
    /// The plan-quota config page (see [`QuotaPanel`]).
    quota: QuotaPanel,
    /// The Usage config page (see [`UsagePanel`]).
    usage: UsagePanel,
    /// Config-screen notice (TS `notice`): `notice.*` i18n key or plain text.
    config_notice: Option<String>,
    /// A needs-restart write is pending on the connected bridge — drives the
    /// Backend Restart affordance (armed by config writes; M3 pages).
    pending_restart: bool,

    /// Workbench-level ephemeral toast (notify-toast semantics).
    toast: ToastState,

    last_fingerprint: u64,
}

/// What handling a command asked the driver loop to do.
enum Flow {
    Continue,
    Restart,
    /// Shut the driver down (window closing / confirmed quit).
    Quit,
}

impl Driver {
    fn new(
        cmd_rx: UnboundedReceiver<UiCommand>,
        ev_tx: UnboundedSender<RuntimeEvent>,
        factory: Arc<dyn WsFactory>,
        clock: Arc<dyn Clock>,
        loop_tx: UnboundedSender<LoopEvent>,
    ) -> Self {
        Self {
            factory,
            clock,
            ev_tx,
            cmd_rx,
            loop_tx,
            phase: Phase::Bootstrapping,
            hub_url: String::new(),
            token: String::new(),
            note: None,
            book: protocol::profiles::ServerBook::load(
                &protocol::profiles::servers_path(),
            )
            .unwrap_or_default(),
            instances: Vec::new(),
            discovery_note: None,
            next_discovery_at: 0,
            discovery_probe: true,
            tabs: Vec::new(),
            active_tab: None,
            closing: HashSet::new(),
            next_tab_id: 1,
            ever_had_tab: false,
            history: HistoryPanel::default(),
            new_session: NewSessionPanel::default(),
            files: FilesPanel::default(),
            system: SystemStatusPanel {
                supported: true,
                ..Default::default()
            },
            models: ModelsPanel::default(),
            // TS `resetCredentials` defaults true — an unreadable store is
            // discovered, not assumed.
            quota: QuotaPanel {
                credentials: true,
                ..Default::default()
            },
            usage: UsagePanel::new(),
            config_notice: None,
            pending_restart: false,
            toast: ToastState::new(),
            last_fingerprint: 0,
        }
    }

    fn hub_client(&self) -> HubClient {
        HubClient::new(
            self.hub_url.clone(),
            self.token.clone(),
            Arc::new(LocalHttpTransport),
            Arc::clone(&self.clock),
        )
    }

    fn tab(&self, id: u64) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.id == id)
    }

    fn tab_mut(&mut self, id: u64) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        let id = self.active_tab?;
        self.tab_mut(id)
    }

    // ---- tab lifecycle ----

    /// Open a tab on `instance_id`, attaching `target` (or booting a fresh
    /// `session/new` when `None`). The tab becomes active.
    fn open_tab(&mut self, instance_id: String, target: Option<String>, cwd: String) -> u64 {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        self.ever_had_tab = true;
        self.tabs.push(Tab {
            id,
            instance_id,
            target,
            cwd,
            core: SessionCore::new(),
            approvals: ApprovalCore::new(),
            responders: HashMap::new(),
            conn: None,
            state: TabPhase::Connecting,
            agent_version: None,
            retry_at: None,
            reconnect_attempts: 0,
            fs_capable: false,
        });
        self.active_tab = Some(id);
        traceln!("[driver] tab {id} opened (instance {})", self.tabs.last().unwrap().instance_id);
        self.spawn_connect(id);
        id
    }

    fn close_tab(&mut self, id: u64) {
        // Mark + close without holding the tab borrow across the mutations.
        let conn = self.tab(id).and_then(|t| t.conn.clone());
        if let Some(conn) = conn {
            self.closing.insert(id);
            conn.close();
        }
        self.tabs.retain(|t| t.id != id);
        if self.active_tab == Some(id) {
            self.active_tab = self.tabs.last().map(|t| t.id);
        }
    }

    /// (Re)connect a tab: handshake + `initialize`, results via the loop
    /// channel. The connection handle travels ahead of the result so a
    /// teardown racing the handshake can still close the socket.
    fn spawn_connect(&mut self, tab_id: u64) {
        let Some(instance_id) = self.tab(tab_id).map(|t| t.instance_id.clone()) else {
            return;
        };
        if let Some(t) = self.tab_mut(tab_id) {
            t.state = TabPhase::Connecting;
            t.retry_at = None;
            t.core.conn_open = false;
        }
        let handlers = Arc::new(DriverHandlers {
            tab: tab_id,
            tx: self.loop_tx.clone(),
        });
        let (conn, fut) = AcpConnection::start(
            &self.hub_url,
            &self.token,
            &instance_id,
            handlers,
            Arc::clone(&self.factory),
            Arc::clone(&self.clock),
        );
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(LoopEvent::ConnHandle { tab: tab_id, conn });
            let result = fut.await.map_err(|e| e.to_string());
            let _ = tx.send(LoopEvent::ConnectResult { tab: tab_id, result });
        });
    }

    /// Close every tab and reset the session surfaces (manual Retry).
    fn close_all_tabs(&mut self) {
        for tab in &self.tabs {
            self.closing.insert(tab.id);
            if let Some(conn) = tab.conn.clone() {
                conn.close();
            }
        }
        self.tabs.clear();
        self.active_tab = None;
    }

    // ---- discovery ----

    fn spawn_discovery(&mut self, probe: bool) {
        let hub = self.hub_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub.instances(probe).await;
            let _ = tx.send(LoopEvent::Discovery(result));
        });
    }

    fn on_discovery(&mut self, result: Result<Vec<HubInstance>, HubApiError>, now: u64) {
        match result {
            Ok(list) => {
                traceln!("[driver] discovery: {} instance(s)", list.len());
                self.instances = list;
                self.discovery_note = None;
                self.next_discovery_at = now + DISCOVERY_REFRESH_MS;
                self.discovery_probe = false;
                if self.phase == Phase::Discovering || self.phase == Phase::Reconnecting {
                    self.phase = Phase::Connected;
                }
                // M1 parity auto-attach: while NO tab has ever existed, the
                // most recently updated session (any instance) opens one; a
                // session-less instance gets a fresh `session/new`.
                if self.tabs.is_empty() && !self.ever_had_tab {
                    if let Some((instance_id, attach, cwd)) = pick_target(&self.instances) {
                        traceln!(
                            "[driver] auto-tab on {} (attach {})",
                            instance_id,
                            attach.as_deref().unwrap_or("<session/new>")
                        );
                        self.open_tab(instance_id, attach, cwd);
                    } else {
                        self.discovery_note =
                            Some("no bridge instance on the hub yet — start a session from the \
                                  editor or pick one from history below".to_string());
                    }
                }
            }
            Err(e) => {
                traceln!("[driver] discovery failed: {e}");
                self.discovery_note = Some(format!("discovery failed: {e}"));
                self.next_discovery_at = now + RECONNECT_DELAY_MS;
                if self.phase == Phase::Connected {
                    self.phase = Phase::Reconnecting;
                }
            }
        }
    }

    // ---- history browser ----

    fn spawn_history_projects(&mut self) {
        let seq = self.history.seq;
        let hub = self.hub_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub.projects().await;
            let _ = tx.send(LoopEvent::HistoryProjects { seq, result });
        });
    }

    fn spawn_history_page(&mut self, workspace_path: String, cursor: Option<HubHistoryCursor>) {
        let seq = self.history.seq;
        let hub = self.hub_client();
        let tx = self.loop_tx.clone();
        let more = cursor.is_some();
        tokio::spawn(async move {
            let result = hub.project_sessions(&workspace_path, cursor.as_ref()).await;
            let _ = tx.send(LoopEvent::HistoryPage { seq, more, result });
        });
    }

    fn spawn_resume(&mut self, workspace_path: String, session_id: String) {
        let hub = self.hub_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            // No client-side deadline: instance incubation is slow by design
            // (~12-20s); the resuming row state owns the wait.
            let result = hub.create_instance(&workspace_path, Some(&session_id)).await;
            let _ = tx.send(LoopEvent::ResumeReady {
                workspace_path,
                session_id,
                result,
            });
        });
    }

    // ---- new-session chooser ----

    fn spawn_new_session_projects(&mut self) {
        let seq = self.new_session.seq;
        let hub = self.hub_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub.projects().await;
            let _ = tx.send(LoopEvent::NewSessionProjects { seq, result });
        });
    }

    fn spawn_new_session_browse(&mut self, path: Option<String>) {
        if let Some(b) = self.new_session.browse.as_mut() {
            b.loading = true;
            b.error = None;
        }
        let seq = self.new_session.seq;
        let hub = self.hub_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub.fs_browse(path.as_deref()).await;
            let _ = tx.send(LoopEvent::NewSessionBrowse { seq, result });
        });
    }

    fn spawn_new_session_create(&mut self, workspace_path: String) {
        self.new_session.creating = Some(workspace_path.clone());
        let hub = self.hub_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            // No client-side deadline: incubation is slow by design (the
            // resume flow's contract); the creating row owns the wait.
            let result = hub
                .create_instance(&workspace_path, None)
                .await
                .map(|created| (workspace_path, created));
            let _ = tx.send(LoopEvent::NewSessionCreated { result });
        });
    }

    // ---- Session Files (batch F) ----

    /// The (instance, session) pair the per-session fs calls address — the
    /// ACTIVE TAB's, exactly like the restart affordance.
    fn active_fs_target(&self) -> Option<(String, String)> {
        self.active_tab
            .and_then(|id| self.tab(id))
            .and_then(|t| Some((t.instance_id.clone(), t.core.active_session_id.clone()?)))
    }

    /// A listing belongs to the (instance, session) that served it; the
    /// active tab showing anything else resets the browser — the TS dialog's
    /// per-mount state (every open starts at the new session's root).
    /// Runs on every command and loop event, so the compare borrows only.
    fn files_reconcile(&mut self) {
        let target = self
            .active_tab
            .and_then(|id| self.tab(id))
            .and_then(|t| t.core.active_session_id.as_deref().map(|sid| (t.instance_id.as_str(), sid)));
        let dirty = self.files.owner.is_some()
            || self.files.loading
            || self.files.error.is_some()
            || self.files.listing.is_some()
            || self.files.viewing.is_some();
        let stale = match (&self.files.owner, target) {
            (Some((owner_inst, owner_sess)), Some((inst, sess))) => {
                owner_inst != inst || owner_sess != sess
            }
            // Had an owner, no session anymore — the state names a session
            // the workbench no longer shows.
            (Some(_), None) => true,
            // The only owner-less state is the no-session error; a session
            // existing now makes it stale.
            (None, Some(_)) => dirty,
            (None, None) => false,
        };
        if stale {
            traceln!("[driver] files: session changed; resetting browser");
            self.files.reset();
        }
    }

    /// One `/fs/list` read (TS fsList): the ACTIVE session's root-relative
    /// `path`. The previous listing drops while loading — stale rows would
    /// join against the NEW path on tap (the TS setListing(null)).
    fn spawn_files_list(&mut self, path: String) {
        if !matches!(
            self.phase,
            Phase::Discovering | Phase::Connected | Phase::Reconnecting
        ) {
            return;
        }
        let Some((instance_id, session_id)) = self.active_fs_target() else {
            self.files.owner = None;
            self.files.path = path;
            self.files.listing = None;
            self.files.viewing = None;
            self.files.loading = false;
            self.files.error =
                Some("no active session — open one first".to_string());
            return;
        };
        self.files.owner = Some((instance_id.clone(), session_id.clone()));
        self.files.path = path.clone();
        // Navigation supersedes the viewer (the TS overlay unmounts).
        self.files.viewing = None;
        self.files.loading = true;
        self.files.error = None;
        self.files.listing = None;
        let seq = self.files.seq;
        let hub = self.hub_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub.fs_list(&instance_id, &session_id, &path).await;
            let _ = tx.send(LoopEvent::FilesList { seq, result });
        });
    }

    /// The listing settled: success replaces it whole; a failure keeps the
    /// error card (the previous listing is already gone per the drop above).
    fn on_files_list(&mut self, seq: u64, result: Result<FsListing, HubApiError>) {
        if seq != self.files.seq {
            return; // a reset invalidated this read
        }
        self.files.loading = false;
        match result {
            Ok(listing) => {
                self.files.listing = Some(listing);
                self.files.error = None;
            }
            Err(e) => {
                self.files.error = Some(describe_files_error(&e));
            }
        }
    }

    /// Open the viewer for one file of the CURRENT listing (TS setViewing):
    /// name/size/kind resolve from the entry the view clicked; the text kind
    /// loads immediately, the deferred kinds explain themselves without a
    /// fetch. A path the listing no longer knows is a stale click — ignored.
    fn spawn_files_view(&mut self, path: String) {
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        let Some(entry) = self
            .files
            .listing
            .as_ref()
            .and_then(|l| l.entries.iter().find(|e| e.name == name))
        else {
            return;
        };
        if entry.kind == FsEntryKind::Dir {
            return; // directories navigate, never view
        }
        let kind = file_kind_of(&entry.name);
        self.files.viewing = Some(FileViewing {
            path,
            name,
            size: entry.size,
            kind,
            first_line: None,
            text: None,
            loading: kind == FileKind::Text,
            error: None,
        });
        if kind == FileKind::Text {
            self.spawn_files_text();
        }
    }

    /// The viewer's single whole-file load (F3): ONE line window from line 1
    /// at the TS viewer's MAX_LINES budget — line-window paging is a later
    /// optimization, so there is deliberately no stitching here.
    fn spawn_files_text(&mut self) {
        if !matches!(
            self.phase,
            Phase::Discovering | Phase::Connected | Phase::Reconnecting
        ) {
            return;
        }
        let Some((instance_id, session_id)) = self.files.owner.clone() else {
            if let Some(v) = self.files.viewing.as_mut() {
                v.error = Some("no active session — open one first".to_string());
            }
            return;
        };
        let Some(path) = self.files.viewing.as_ref().map(|v| v.path.clone()) else {
            return;
        };
        if let Some(v) = self.files.viewing.as_mut() {
            v.loading = true;
            v.error = None;
        }
        let seq = self.files.seq;
        let hub = self.hub_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub
                .fs_file_text(&instance_id, &session_id, &path, 1, VIEWER_LINE_BUDGET)
                .await;
            let _ = tx.send(LoopEvent::FilesText { seq, result });
        });
    }

    /// The viewer's window settled: the pair is (first_line, text) with the
    /// server's trailing newline already stripped (protocol::hub_client).
    fn on_files_text(&mut self, seq: u64, result: Result<(u64, String), HubApiError>) {
        if seq != self.files.seq {
            return;
        }
        let Some(v) = self.files.viewing.as_mut() else {
            return;
        };
        v.loading = false;
        match result {
            Ok((first_line, text)) => {
                v.first_line = Some(first_line);
                v.text = Some(text);
                v.error = None;
            }
            Err(e) => {
                v.error = Some(format!("file: {e}"));
            }
        }
    }

    // ---- system status (config page "system") ----

    /// One `/api/system-stats` read (TS `loadSystemStats`): sets the spinner,
    /// schedules the NEXT poll (the 5s interval's tick lands here), and fires.
    /// No-op before a hub is dialable — the TS guard `if (!client) return`.
    fn spawn_system_stats(&mut self, now: u64) {
        if !matches!(
            self.phase,
            Phase::Discovering | Phase::Connected | Phase::Reconnecting
        ) {
            return;
        }
        self.system.loading = true;
        self.system.next_poll_at = now + SYSTEM_STATS_POLL_MS;
        let hub = self.hub_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub.system_stats().await;
            let _ = tx.send(LoopEvent::SystemStats(result));
        });
    }

    /// The read landed (TS loadSystemStats's error doctrine): success
    /// replaces the snapshot; a 404 retires THIS page alone; anything else
    /// is a blip that keeps the last snapshot, except the FIRST load of a
    /// broken connection, which still needs a story.
    fn on_system_stats(&mut self, result: Result<SystemStatsResponse, HubApiError>) {
        self.system.loading = false;
        match result {
            Ok(stats) => {
                self.system.stats = Some(stats);
                self.system.error = None;
            }
            Err(e) if e.status == Some(404) => {
                // This hub predates the route (bridge < 0.62.0) — a version
                // gap, not a failure.
                self.system.supported = false;
            }
            Err(e) => {
                if self.system.stats.is_none() {
                    self.system.error = Some(e.to_string());
                }
            }
        }
    }

    /// The restart affordance's POST (TS `restartConfigBackend`): addressed
    /// to the ACTIVE TAB's bridge — the desktop's connected-session instance.
    /// With no tab open there is nothing to name; the view shows the
    /// no-instance hint instead of the button, so the command never fires.
    fn spawn_backend_restart(&mut self) {
        let Some(instance_id) = self
            .active_tab
            .and_then(|id| self.tab(id))
            .map(|t| t.instance_id.clone())
        else {
            return;
        };
        let hub = self.hub_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub.restart_backend(&instance_id).await;
            let _ = tx.send(LoopEvent::BackendRestart(result));
        });
    }

    /// The restart settled. `closed: false` means the old subprocess
    /// survived — the writes this restart existed to apply did NOT take
    /// effect, so the pending flag stays armed (the affordance remains).
    fn on_backend_restart(&mut self, result: Result<BackendRestartResult, HubApiError>) {
        match result {
            Ok(res) if res.closed => {
                self.pending_restart = false;
                self.config_notice = Some(NOTICE_RESTARTED.to_string());
            }
            Ok(_) => {
                self.config_notice = Some(NOTICE_RESTART_FAILED.to_string());
            }
            Err(e) => {
                // TS `notify` — plain text (not an i18n key) in the banner.
                self.config_notice = Some(format!("backend restart failed: {e}"));
            }
        }
    }

    // ---- models (config page "models") ----

    /// The Settings API client (`protocol::settings_client` — the Rust port
    /// of hub.ts's settings section) over the settings-shaped transport.
    /// Stateless, so one per call like [`Self::hub_client`].
    fn settings_client(&self) -> HubHttpClient<SettingsApiTransport> {
        HubHttpClient::new(
            self.hub_url.clone(),
            self.token.clone(),
            SettingsApiTransport,
        )
    }

    /// The /settings/all capability probe (the verdict half of TS
    /// `loadConfigAll`): 404 marks an OLDER hub — a capability gap the page
    /// explains, never an error. No-op before a hub is dialable (the TS
    /// `if (!client) return` guard).
    fn spawn_config_probe(&mut self) {
        if !matches!(
            self.phase,
            Phase::Discovering | Phase::Connected | Phase::Reconnecting
        ) {
            return;
        }
        let hub = self.settings_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub.settings_all().await.map(|_| ());
            let _ = tx.send(LoopEvent::ConfigProbe(result));
        });
    }

    /// GET /settings/models (TS `loadConfigSection("models")`): the section
    /// read that runs on every page open and after every write.
    fn spawn_models_load(&mut self) {
        if !matches!(
            self.phase,
            Phase::Discovering | Phase::Connected | Phase::Reconnecting
        ) {
            return;
        }
        self.models.loading = true;
        self.models.error = None;
        let hub = self.settings_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub.settings_models().await;
            let _ = tx.send(LoopEvent::ModelsLoaded(result));
        });
    }

    /// The probe settled. Only the verdict is read here — the snapshot's
    /// sections are the section reads' business.
    fn on_config_probe(&mut self, result: Result<(), SettingsApiError>) {
        match result {
            Ok(()) => self.models.supported = Some(true),
            Err(SettingsApiError::Http { status: 404, .. }) => {
                // Bridge < 0.47.0: the "hub too old" card, and the section
                // read behind it cannot work either.
                self.models.supported = Some(false);
                self.models.loading = false;
            }
            Err(_) => {
                // A real failure belongs to the section read's error story;
                // the verdict stays unknown so a later probe can still run
                // (TS: the probe only re-runs while configSupported is null).
            }
        }
    }

    /// The section read settled (TS loadConfigSection's configSlice arms):
    /// Ok decodes permissively through config_decode and clears the error; a
    /// 404 retires the capability verdict; anything else is the error card.
    fn on_models_loaded(&mut self, result: Result<Value, SettingsApiError>) {
        self.models.loading = false;
        match result {
            Ok(v) => {
                // Permissive-wire doctrine: an undecodable shape paints as
                // the empty list, never a failure.
                self.models.models = Some(serde_json::from_value(v).unwrap_or_default());
                self.models.error = None;
            }
            Err(SettingsApiError::Http { status: 404, .. }) => {
                self.models.supported = Some(false);
            }
            Err(e) => {
                self.models.error = Some(e.to_string());
            }
        }
    }

    /// The form's Save (TS `upsertModel` through `applyConfigWrite`): POST
    /// /settings/models, then the Effect Class decides the outcome surface —
    /// `immediate` is applied, `needs-restart` ARMS the Backend Restart
    /// affordance with its notice (never auto-restarted; CONTEXT.md) — and
    /// the models section reloads either way.
    fn spawn_model_upsert(&mut self, adding: bool, body: ModelUpsert) {
        self.models.busy = true;
        let hub = self.settings_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub.add_model(body).await;
            let _ = tx.send(LoopEvent::ModelUpsert { adding, result });
        });
    }

    /// The write settled — the applyConfigWrite outcome port (configSlice):
    /// ok reloads the section and closes the form; a needs-restart effect
    /// arms the restart affordance; a refusal surfaces its BODY (the reason
    /// the user can act on), never a bare status.
    fn on_model_upsert(&mut self, adding: bool, result: Result<WriteEffect, SettingsApiError>) {
        self.models.busy = false;
        let label = if adding { "add model" } else { "update model" };
        let ok = match result {
            Ok(w) if w.ok => {
                // The Effect Class (CONTEXT.md): the models route is
                // `immediate` (the backend polls the provider table), but
                // the outcome is surfaced, not assumed — a needs-restart
                // answer arms the affordance.
                if w.effect == Some(Effect::NeedsRestart) {
                    self.pending_restart = true;
                    self.config_notice = Some(NOTICE_NEEDS_RESTART.to_string());
                }
                self.spawn_models_load();
                true
            }
            Ok(w) => {
                self.config_notice = Some(format!(
                    "{label} failed: {}",
                    w.error.unwrap_or_else(|| "refused".to_string())
                ));
                false
            }
            Err(e) => {
                self.config_notice = Some(format!("{label} failed: {e}"));
                false
            }
        };
        let _ = self.ev_tx.send(RuntimeEvent::ModelWriteResult { ok });
    }

    // ---- usage statistics (config page "usage" / "usagePlatform") ----

    /// The loads a freshly-mounted Usage page runs: the capability probe
    /// once (`/settings/all` — the TS config entry list's load, whose
    /// snapshot seeds the local tab) plus the active tab's own section read.
    /// No-op before a hub is dialable (the TS `if (!client) return` guard).
    fn spawn_usage_loads(&mut self) {
        if !matches!(
            self.phase,
            Phase::Discovering | Phase::Connected | Phase::Reconnecting
        ) {
            return;
        }
        if self.usage.supported.is_none() {
            self.usage.loading = true;
            let hub = self.settings_client();
            let tx = self.loop_tx.clone();
            tokio::spawn(async move {
                let result = hub.settings_all().await;
                let _ = tx.send(LoopEvent::UsageAll(result));
            });
        }
        self.spawn_usage_section(None);
    }

    /// One usage section read (TS `loadConfigSection("usage"` |
    /// `"usagePlatform")`). The range rides the QUERY — an upstream control,
    /// not a client-side filter — and the ask is recorded BEFORE the await:
    /// the stale-response guard below compares answers against the newest
    /// ask, so a read that lands first must not overwrite the record of one
    /// still in flight.
    fn spawn_usage_section(&mut self, range: Option<String>) {
        if !matches!(
            self.phase,
            Phase::Discovering | Phase::Connected | Phase::Reconnecting
        ) {
            return;
        }
        let requested = range.unwrap_or_else(|| "7d".to_string());
        let platform = self.usage.tab == UsageTab::Platform;
        if platform {
            self.usage.platform_range = requested.clone();
        } else {
            self.usage.local_range = requested.clone();
        }
        self.usage.loading = true;
        let hub = self.settings_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = if platform {
                hub.settings_usage_platform(&requested).await
            } else {
                hub.settings_usage(&requested).await
            };
            let _ = tx.send(LoopEvent::UsageSection {
                platform,
                requested,
                result,
            });
        });
    }

    /// The probe settled (TS loadConfigAll): 404 marks an OLDER hub — the
    /// whole-page capability card; the snapshot's usage section seeds the
    /// local tab (the entry-list behavior — a section screen opening
    /// straight after needs no second request for it).
    fn on_usage_all(&mut self, result: Result<SettingsAll, SettingsApiError>) {
        self.usage.loading = false;
        match result {
            Ok(all) => {
                self.usage.supported = Some(true);
                if let Some(u) = all.usage {
                    self.usage.local_range = u.range.as_str().to_string();
                    self.usage.local = Some(u);
                }
            }
            Err(SettingsApiError::Http { status: 404, .. }) => {
                // Bridge < 0.47.0: the "hub too old" card (the whole page).
                self.usage.supported = Some(false);
            }
            Err(_) => {
                // A real failure lands in TS `configError`, which the usage
                // page does not render (its frame shows loading/empty
                // states); the verdict stays unknown so a later probe can
                // still run (the TS probe only runs while the value is null).
            }
        }
    }

    /// One section read settled (the loadConfigSection arms): the
    /// stale-range guard drops answers nobody asked for anymore; a platform
    /// 404 retires the PLATFORM TAB alone (the route verdict belongs to
    /// /settings/all); a local 404 flips the shared verdict (the TS shared
    /// catch).
    fn on_usage_section(
        &mut self,
        platform: bool,
        requested: String,
        result: Result<Value, SettingsApiError>,
    ) {
        self.usage.loading = false;
        let current = if platform {
            self.usage.platform_range.clone()
        } else {
            self.usage.local_range.clone()
        };
        match result {
            Ok(payload) => {
                // The routes wrap the payload (`{ok, usage}` /
                // `{ok, platformUsage}`). Storing the whole response would
                // make every `usage.available` read wrong and the screen
                // would show its empty state forever.
                let key = if platform { "platformUsage" } else { "usage" };
                let body = payload.get(key).cloned();
                let answered = body
                    .as_ref()
                    .and_then(|b| b.get("range"))
                    .and_then(Value::as_str)
                    .unwrap_or(&requested)
                    .to_string();
                // Stale-response guard: the range pills fire a fresh read per
                // tap and nothing serializes them — drop any answer that no
                // longer matches the newest ask (configSlice.ts).
                if answered != current {
                    return;
                }
                match body {
                    Some(b) => {
                        if platform {
                            match serde_json::from_value::<SettingsPlatformUsage>(b) {
                                Ok(u) => {
                                    self.usage.platform = Some(u);
                                    self.usage.platform_error = None;
                                }
                                // The TS page casts the payload untyped, so
                                // this state has no TS shape; surface it
                                // instead of hiding a silently-wrong page.
                                Err(e) => {
                                    self.usage.platform_error =
                                        Some(format!("invalid platform usage payload: {e}"))
                                }
                            }
                        } else {
                            // Permissive-wire doctrine: an undecodable answer
                            // keeps the last snapshot, never fails the page.
                            if let Ok(u) = serde_json::from_value::<SettingsUsage>(b) {
                                self.usage.local_range = u.range.as_str().to_string();
                                self.usage.local = Some(u);
                            }
                        }
                    }
                    None => {
                        // TS `payload.usage ?? null` clears the section.
                        if platform {
                            self.usage.platform = None;
                        } else {
                            self.usage.local = None;
                        }
                    }
                }
            }
            Err(SettingsApiError::Http { status: 404, .. }) if platform => {
                // The bridge predates the platform route — NOT the global
                // verdict (that belongs to /settings/all): only the platform
                // tab shows its upgrade note.
                self.usage.platform_supported = false;
            }
            Err(SettingsApiError::Http { status: 404, .. }) => {
                self.usage.supported = Some(false);
            }
            Err(_) => {
                // TS configError — not rendered by this page (same as the
                // probe's real-failure arm above).
            }
        }
    }

    // ---- plan quota (config page "quota") ----

    /// The /settings/all snapshot read (TS `loadConfigAll`): the capability
    /// verdict plus the reset-card eligibility — the provider ids that COULD
    /// own cards, and whether the credential store was readable at all. A
    /// 404 marks an OLDER hub; the page explains instead of erroring.
    fn spawn_quota_settings_all(&mut self) {
        if !matches!(
            self.phase,
            Phase::Discovering | Phase::Connected | Phase::Reconnecting
        ) {
            return;
        }
        self.quota.loading = true;
        let hub = self.settings_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub.settings_all().await;
            let _ = tx.send(LoopEvent::QuotaSettingsAll(result));
        });
    }

    fn on_quota_settings_all(&mut self, result: Result<SettingsAll, SettingsApiError>) {
        self.quota.loading = false;
        match result {
            Ok(all) => {
                self.quota.supported = Some(true);
                // The hub reports the coding-plan provider ids that COULD
                // own cards, not the ones this account actually has, so a
                // picker is the norm. `credentials: false` means the store
                // could not be decrypted — a different (and non-fatal)
                // state from having no plan at all.
                self.quota.eligible = all.reset_cards.providers;
                self.quota.credentials = all.reset_cards.credentials;
                self.quota.error = None;
            }
            Err(SettingsApiError::Http { status: 404, .. }) => {
                self.quota.supported = Some(false);
            }
            Err(e) => {
                self.quota.error = Some(e.to_string());
            }
        }
    }

    /// The quota read (TS `refreshUsageStats`): pull-only — failures land in
    /// `quota_unavailable` (hide data, retry later), never in a toast.
    fn spawn_quota_usage(&mut self) {
        if !matches!(
            self.phase,
            Phase::Discovering | Phase::Connected | Phase::Reconnecting
        ) {
            return;
        }
        let hub = self.settings_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            // parseUsageStats: a 2xx body that does not fit the combined
            // shape reads as no data — NOT as a failed fetch.
            let result = match hub.settings_quota().await {
                Ok(v) => Ok(serde_json::from_value::<AccountUsageStats>(v).ok()),
                Err(e) => Err(e),
            };
            let _ = tx.send(LoopEvent::QuotaUsage { result });
        });
    }

    fn on_quota_usage(
        &mut self,
        result: Result<Option<AccountUsageStats>, SettingsApiError>,
        now: u64,
    ) {
        match result {
            Ok(parsed) => {
                self.quota.usage = parsed;
                self.quota.quota_unavailable = false;
                // TS `Date.now()` rides the successful set() only; a failed
                // refresh keeps the old stamp.
                self.quota.usage_stats_at = Some(now);
            }
            Err(_) => {
                self.quota.usage = None;
                self.quota.quota_unavailable = true;
            }
        }
    }

    /// One provider's card inventory (TS `loadResetCards`). The previous
    /// provider's cards drop NOW, not when the read lands: while it is in
    /// flight the screen must not show provider A's cards under a nonce
    /// minted for another plan — and a failed read must not keep them.
    fn spawn_quota_cards(&mut self, provider_id: String) {
        if !matches!(
            self.phase,
            Phase::Discovering | Phase::Connected | Phase::Reconnecting
        ) {
            return;
        }
        self.quota.busy = true;
        self.quota.provider_id = Some(provider_id.clone());
        self.quota.cards = None;
        self.quota.nonce = None;
        self.quota.idempotency_key = None;
        let hub = self.settings_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub.reset_card_status(&provider_id).await;
            let _ = tx.send(LoopEvent::QuotaCards { provider_id, result });
        });
    }

    fn on_quota_cards(
        &mut self,
        provider_id: String,
        result: Result<ResetCardStatus, SettingsApiError>,
    ) {
        // Stale-read guard (configSlice): the picker fires a read per tap,
        // and the SLOWER one must not land its nonce under a provider the
        // user has already moved past — a spend would pair the two and the
        // route answers 409, reading as a broken button.
        if self.quota.provider_id.as_deref() != Some(provider_id.as_str()) {
            return;
        }
        self.quota.busy = false;
        match result {
            Ok(status) => {
                self.quota.nonce = Some(status.reset_cards.nonce.clone());
                self.quota.cards = Some(status.reset_cards);
                // Minted HERE, with the nonce it belongs to, so a spend's
                // every retry sends the same key (a fresh key per attempt
                // defeats the backend's dedupe and can burn a second card).
                self.quota.idempotency_key = Some(new_reset_idempotency_key());
                // The unread-badge clear (QuotaPage's mount effect): purely
                // cosmetic, then the reload clears the flag; a failure is
                // not worth a toast.
                if self.quota.open
                    && self
                        .quota
                        .cards
                        .as_ref()
                        .is_some_and(|c| c.has_unread_history)
                {
                    self.spawn_quota_history_read(provider_id);
                }
            }
            Err(e) => {
                self.config_notice = Some(format!("reset card status failed: {e}"));
            }
        }
    }

    fn spawn_quota_history_read(&mut self, provider_id: String) {
        if !matches!(
            self.phase,
            Phase::Discovering | Phase::Connected | Phase::Reconnecting
        ) {
            return;
        }
        let hub = self.settings_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub.mark_reset_history_read(&provider_id).await.map(|_| ());
            let _ = tx.send(LoopEvent::QuotaHistoryRead { provider_id, result });
        });
    }

    /// The badge clear reloads the inventory (TS `markResetHistoryRead`) —
    /// that reload also issues the next nonce.
    fn on_quota_history_read(&mut self, provider_id: String, result: Result<(), SettingsApiError>) {
        if result.is_ok() {
            self.spawn_quota_cards(provider_id);
        }
    }

    /// The spend (TS `spendResetCard`). The provider + nonce were captured
    /// at the confirmation; the driver re-checks the provider (a switch
    /// since would pair the nonce with another plan's inventory) and keeps
    /// one spend in flight at a time.
    fn spawn_quota_spend(&mut self, provider_id: String, nonce: String, reset_type: ResetCardType) {
        if !matches!(
            self.phase,
            Phase::Discovering | Phase::Connected | Phase::Reconnecting
        ) {
            return;
        }
        self.quota.busy = true;
        // The key minted WITH this nonce, NOT a fresh one: the backend keys
        // the spend on it, so a retry (the 502/504 the bridge answers when a
        // spend may already have been carried out) gets the same outcome.
        let idempotency_key = self
            .quota
            .idempotency_key
            .clone()
            .unwrap_or_else(new_reset_idempotency_key);
        let hub = self.settings_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub
                .spend_reset_card(SpendResetCard {
                    provider_id,
                    reset_type,
                    nonce,
                    idempotency_key,
                })
                .await;
            let _ = tx.send(LoopEvent::QuotaSpent { result });
        });
    }

    fn on_quota_spent(&mut self, result: Result<Value, SettingsApiError>) {
        self.quota.busy = false;
        let provider_id = self.quota.provider_id.clone();
        match result {
            Ok(res) => {
                let used = res.get("used").and_then(Value::as_bool) != Some(false);
                self.config_notice = Some(
                    if used {
                        NOTICE_RESET_SPENT
                    } else {
                        NOTICE_RESET_SPEND_FAILED
                    }
                    .to_string(),
                );
                // Refresh both surfaces: the card list (one fewer card — and
                // the next nonce) and the quota (the window it cleared). A
                // spend that shows no result reads as a failure even when it
                // worked.
                if let Some(provider_id) = provider_id {
                    self.spawn_quota_cards(provider_id);
                }
                self.spawn_quota_usage();
            }
            Err(SettingsApiError::Http { status: 409, .. }) => {
                // 409 = the nonce is no longer the one the server issued (a
                // newer status read replaced it). Re-read to mint a fresh
                // one and say so: without this the button looks broken. The
                // route burns a nonce only AFTER the spend settles, so a
                // mid-request failure is NOT a 409 — that retry path keeps
                // its key.
                if let Some(provider_id) = provider_id {
                    self.spawn_quota_cards(provider_id);
                }
                self.config_notice = Some(NOTICE_RESET_STALE.to_string());
            }
            Err(e) => {
                self.config_notice = Some(format!("reset card spend failed: {e}"));
            }
        }
    }

    // ---- tick ----

    fn tick(&mut self, now: u64) {
        for tab in &mut self.tabs {
            tab.core.flush_update_batch(now);
            tab.core.flush_late_page(now);
            tab.core.poll_earlier_window(now);
        }
        // Toast auto-clear (the notify timer's body).
        self.toast.expire(now);
        // The System Status page's 5s poll (TS `setInterval(loadSystemStats,
        // POLL_MS)`) — live only while the page is open.
        if self.system.open && now >= self.system.next_poll_at {
            self.spawn_system_stats(now);
        }
        // Reconnect due tabs (collected first — spawn_connect borrows self).
        let due: Vec<u64> = self
            .tabs
            .iter()
            .filter(|t| t.retry_at.map(|at| now >= at).unwrap_or(false))
            .map(|t| t.id)
            .collect();
        for id in due {
            traceln!("[driver] tab {id} reconnecting");
            self.spawn_connect(id);
        }
        if now >= self.next_discovery_at {
            let probe = self.discovery_probe
                || self.instances.is_empty()
                || self.phase == Phase::Reconnecting;
            self.next_discovery_at =
                now + if self.instances.is_empty() && self.phase == Phase::Connected {
                    NO_INSTANCE_RETRY_MS
                } else {
                    DISCOVERY_REFRESH_MS
                };
            self.spawn_discovery(probe);
        }
    }

    // ---- wire pump ----

    /// Sends a tab's outbox wire requests, shipping their results back
    /// through the loop channel. Wire shapes come from protocol's builders;
    /// the two envelopes protocol does not yet expose helpers for
    /// (`session/load_earlier`, `session/prompt`) use the exact field
    /// spellings of the TS store.
    fn pump_tab(&mut self, tab_id: u64) {
        // Take the outbox + connection first — the loop below borrows self
        // again for the channel clone.
        let wires = match self.tab_mut(tab_id) {
            Some(tab) if tab.state == TabPhase::Connected => {
                let conn = tab.conn.clone();
                let outbox = tab.core.take_wire();
                match conn {
                    Some(conn) => (conn, outbox),
                    None => return,
                }
            }
            _ => return,
        };
        let (conn, outbox) = wires;
        let cwd = self.tab(tab_id).map(|t| t.cwd.clone()).unwrap_or_default();
        for wire in outbox {
            let tx = self.loop_tx.clone();
            match wire {
                WireRequest::Load(req) => {
                    let conn = conn.clone();
                    let params = session_load_params(&req.session_id, &cwd, req.limit as u64);
                    tokio::spawn(async move {
                        let res = conn.request("session/load", params).await.map_err(|e| e.0);
                        let _ = tx.send(LoopEvent::LoadResult {
                            tab: tab_id,
                            req,
                            result: res,
                        });
                    });
                }
                WireRequest::LoadEarlier(req) => {
                    let conn = conn.clone();
                    let session_id = req.session_id.clone();
                    let params = load_earlier_params(&req);
                    tokio::spawn(async move {
                        let res =
                            conn.request("session/load_earlier", params).await.map_err(|e| e.0);
                        let _ = tx.send(LoopEvent::LoadEarlierResult {
                            tab: tab_id,
                            session_id,
                            result: res,
                        });
                    });
                }
                WireRequest::Prompt(req) => {
                    let conn = conn.clone();
                    let params = prompt_params(&req);
                    tokio::spawn(async move {
                        let res = conn.request("session/prompt", params).await.map_err(|e| e.0);
                        let _ = tx.send(LoopEvent::PromptResult {
                            tab: tab_id,
                            req,
                            result: res,
                        });
                    });
                }
                WireRequest::CancelTurn { session_id } => {
                    conn.notify("session/cancel", json!({ "sessionId": session_id }));
                }
                WireRequest::SetConfig {
                    session_id,
                    config_id,
                    value,
                } => {
                    let conn = conn.clone();
                    tokio::spawn(async move {
                        let res = conn
                            .request(
                                "session/set_config_option",
                                json!({
                                    "sessionId": session_id,
                                    "configId": config_id,
                                    "value": value,
                                }),
                            )
                            .await
                            .map_err(|e| e.0);
                        let _ = tx.send(LoopEvent::SetConfigResult { tab: tab_id, result: res });
                    });
                }
            }
        }
    }

    fn pump_all(&mut self) {
        let ids: Vec<u64> = self.tabs.iter().map(|t| t.id).collect();
        for id in ids {
            self.pump_tab(id);
        }
    }

    // ---- event handling ----

    fn handle_loop_event(&mut self, ev: LoopEvent, now: u64) {
        // Session switches also arrive as events (session/new, load results)
        // — keep the files browser honest about its owner here too.
        self.files_reconcile();
        match ev {
            LoopEvent::ConnHandle { tab, conn } => {
                if self.tab(tab).is_some() && !self.closing.contains(&tab) {
                    if let Some(t) = self.tab_mut(tab) {
                        t.conn = Some(conn);
                    }
                } else {
                    // Tab gone (closed while the handshake raced) — drop it.
                    conn.close();
                }
            }
            LoopEvent::ConnectResult { tab, result } => {
                self.on_connect_result(tab, result, now);
            }
            LoopEvent::Closed { tab } => {
                if self.closing.remove(&tab) {
                    return; // we closed it — expected
                }
                if self.tab(tab).is_none() {
                    return;
                }
                // Only a live socket dying reconnects; a handshake failure
                // fires Closed too but retries through ConnectResult.
                if let Some(t) = self.tab_mut(tab) {
                    if t.state == TabPhase::Connected {
                        traceln!("[driver] tab {tab} socket closed; reconnecting");
                        // The pending asks died with the socket — their
                        // responders point into the void and would silently
                        // swallow taps; broadcast activity is equally
                        // untrustworthy now (appStore.ts "closed": clear
                        // permissions/elicitations/sessionStates).
                        t.approvals.clear();
                        t.responders.clear();
                        t.core.session_states.clear();
                        let delay = reconnect_delay_ms(t.reconnect_attempts);
                        t.reconnect_attempts += 1;
                        t.state = TabPhase::Reconnecting;
                        t.retry_at = Some(now + delay);
                        t.core.conn_open = false;
                    }
                }
            }
            LoopEvent::Update {
                tab,
                session_id,
                update,
                meta,
            } => {
                if let Some(t) = self.tab_mut(tab) {
                    t.core.on_update(now, &session_id, update, meta);
                }
            }
            LoopEvent::TurnState {
                tab,
                session_id,
                running,
                turn,
            } => {
                if let Some(t) = self.tab_mut(tab) {
                    // SessionCore reads usage/turnMs off the raw turn block;
                    // rebuild that shape.
                    let turn_value = turn.map(|t| json!({
                        "usage": t.usage,
                        "turnMs": t.turn_ms,
                    }));
                    t.core
                        .on_turn_state(now, &session_id, running, turn_value.as_ref());
                }
            }
            LoopEvent::NewSession { tab, result } => match result {
                Ok(info) => {
                    if let Some(t) = self.tab_mut(tab) {
                        // The TS createProjectSession shape: a new session
                        // attaches with no replay.
                        t.core.active_session_id = info.session_id.clone();
                        t.core.config_options = info.config_options;
                        t.core.current_mode_id = info.current_mode_id;
                        t.core.loading_session = false;
                    }
                }
                Err(e) => {
                    traceln!("[driver] tab {tab} session/new failed: {e}");
                    if let Some(t) = self.tab_mut(tab) {
                        t.state = TabPhase::Failed(format!("session/new failed: {e}"));
                    }
                }
            },
            LoopEvent::LoadResult { tab, req, result } => {
                if let Some(t) = self.tab_mut(tab) {
                    match result {
                        Ok(value) => t.core.on_load_result(now, &req, &value),
                        Err(_) => t.core.on_load_failed(&req),
                    }
                }
            }
            LoopEvent::LoadEarlierResult {
                tab,
                session_id,
                result,
            } => {
                if let Some(t) = self.tab_mut(tab) {
                    match result {
                        Ok(value) => {
                            t.core.on_load_earlier_result(now, &value);
                        }
                        Err(e) => {
                            t.core.on_load_earlier_failed(&session_id, &e);
                        }
                    }
                }
            }
            LoopEvent::PromptResult { tab, req, result } => {
                if let Some(t) = self.tab_mut(tab) {
                    match result {
                        Ok(value) => t.core.on_prompt_result(now, &req, &value),
                        Err(_) => t.core.on_prompt_failed(now),
                    }
                }
            }
            // A failed set surfaces nowhere but the log — the chips keep the
            // last broadcast truth (the TS notice path is not ported).
            LoopEvent::SetConfigResult { tab, result } => {
                if let (Ok(value), Some(t)) = (&result, self.tab_mut(tab)) {
                    t.core.on_set_config_result(value);
                }
            }
            // ---- approval cards (Permission Race; CONTEXT.md) ----
            LoopEvent::ServerRequest { tab, request, responder } => {
                let is_active_tab = self.active_tab == Some(tab);
                let toast_text = if let Some(t) = self.tab_mut(tab) {
                    // Let the raising tool_call land before resolving the
                    // card context: the bridge emits it right before the
                    // request, so it may still sit in the 120ms coalescing
                    // batch (CONTEXT.md: context comes from the tool_call
                    // matched by toolCallId).
                    t.core.flush_update_batch(now);
                    let active_session = if is_active_tab {
                        t.core.active_session_id.clone()
                    } else {
                        None
                    };
                    let report = t.approvals.on_request(
                        request.id,
                        &request.method,
                        &request.params,
                        &t.core.messages,
                        active_session.as_deref(),
                    );
                    match report.ask {
                        IncomingAsk::Stored {
                            session_id,
                            displaced_request_id,
                        } => {
                            // A bridge re-send (reconnect) displaced a stale
                            // entry — its responder is dead by construction.
                            if let Some(old) = displaced_request_id {
                                t.responders.remove(&old);
                            }
                            t.responders.insert(request.id, responder);
                            t.core.set_activity_awaiting(&session_id, true);
                        }
                        IncomingAsk::AnswerNow(result) => responder.send(result),
                        // Unsupported interaction: deliberately NOT answered —
                        // first-response-wins, and the always-attached editor
                        // client may still answer.
                        IncomingAsk::Unsupported => {}
                    }
                    report.toast
                } else {
                    None
                };
                if let Some(text) = toast_text {
                    self.toast.notify(now, &text);
                }
            }
            LoopEvent::CancelRequest { tab, id } => {
                // We lost the Permission Race: drop the card(s) for this
                // request without answering (the server's race is settled).
                if let Some(t) = self.tab_mut(tab) {
                    let cleared = t.approvals.on_cancel_request(id);
                    t.responders.remove(&id);
                    for sid in cleared {
                        let awaiting = t.approvals.has_pending(&sid);
                        t.core.set_activity_awaiting(&sid, awaiting);
                    }
                }
            }
            LoopEvent::AskSettled {
                tab,
                session_id,
                kind,
                tool_call_id,
            } => {
                // The ask was decided elsewhere (or the wait broke): dismiss
                // this tab's copy of the card. Our own answers already
                // removed their entries, so this no-ops for the winner.
                if let Some(t) = self.tab_mut(tab) {
                    let removed =
                        t.approvals
                            .on_ask_settled(&session_id, kind.as_deref(), tool_call_id.as_deref());
                    for id in removed {
                        t.responders.remove(&id);
                    }
                    let awaiting = t.approvals.has_pending(&session_id);
                    t.core.set_activity_awaiting(&session_id, awaiting);
                }
            }
            // ---- hub REST ----
            LoopEvent::Discovery(result) => self.on_discovery(result, now),
            LoopEvent::HistoryProjects { seq, result } => {
                if seq != self.history.seq {
                    return; // superseded by an open/back toggle
                }
                match result {
                    Ok(list) => {
                        self.history.projects = Some(list);
                        self.history.projects_error = None;
                    }
                    Err(e) => {
                        self.history.projects_error = Some(format!("projects: {e}"));
                    }
                }
            }
            LoopEvent::HistoryPage { seq, more, result } => {
                if seq != self.history.seq {
                    return; // a late page must never land in the wrong list
                }
                let Some(sel) = self.history.selected.as_mut() else { return };
                match result {
                    Ok(page) => {
                        if more {
                            sel.rows.extend(page.sessions);
                        } else {
                            sel.rows = page.sessions;
                        }
                        sel.next_cursor = page.next_cursor;
                        sel.error = None;
                    }
                    Err(e) => {
                        sel.error = Some(describe_history_error(&e));
                    }
                }
                sel.loading_page = false;
                sel.loading_more = false;
            }
            LoopEvent::ResumeReady {
                workspace_path,
                session_id,
                result,
            } => {
                let resuming = self.history.selected.as_mut().map(|sel| {
                    sel.resuming = None;
                    &mut sel.error
                });
                match result {
                    Ok(created) => {
                        if let Some(err) = resuming {
                            *err = None;
                        }
                        traceln!(
                            "[driver] resume {} -> instance {} (reused: {})",
                            session_id,
                            created.id,
                            created.reused
                        );
                        // Attach the same id on the (new or reused) instance
                        // — one more independent ACP context.
                        self.open_tab(created.id, Some(session_id), workspace_path);
                        // Discovery refresh (best-effort) so the new instance
                        // shows in the sidebar.
                        self.spawn_discovery(false);
                    }
                    Err(e) => {
                        if let Some(err) = resuming {
                            *err = Some(describe_history_error(&e));
                        }
                    }
                }
            }
            // ---- new-session chooser ----
            LoopEvent::NewSessionProjects { seq, result } => {
                if seq != self.new_session.seq || !self.new_session.open {
                    return; // superseded by a close/reopen
                }
                match result {
                    Ok(list) => {
                        self.new_session.projects = Some(list);
                        self.new_session.projects_error = None;
                    }
                    Err(e) => {
                        let msg = if e.status == Some(404) {
                            // Bridge < 0.17.0: the route itself is missing.
                            "this hub is too old for remote session-create".to_string()
                        } else {
                            format!("projects: {e}")
                        };
                        self.new_session.projects_error = Some(msg);
                    }
                }
            }
            LoopEvent::NewSessionBrowse { seq, result } => {
                if seq != self.new_session.seq || !self.new_session.open {
                    return;
                }
                let Some(b) = self.new_session.browse.as_mut() else {
                    return;
                };
                b.loading = false;
                match result {
                    Ok(res) => {
                        b.path = res.path;
                        b.parent = res.parent;
                        b.creatable = res.creatable;
                        b.truncated = res.truncated;
                        // Hidden (`.`-prefixed) entries stay server-visible
                        // but off the picker (the TS dialog's filter).
                        b.entries = res
                            .entries
                            .into_iter()
                            .filter(|e| !e.name.starts_with('.'))
                            .map(|e| e.name)
                            .collect();
                        b.error = None;
                    }
                    Err(e) => {
                        let msg = if e.status == Some(404) && b.path.is_empty() {
                            // Entering browse lists HOME, which always
                            // exists — a 404 there is a bridge too old for
                            // /api/fs/list (the TS `first` distinction).
                            "this hub is too old for directory browsing".to_string()
                        } else {
                            format!("browse: {e}")
                        };
                        b.error = Some(msg);
                    }
                }
            }
            LoopEvent::NewSessionCreated { result } => {
                self.new_session.creating = None;
                match result {
                    Ok((workspace_path, created)) => {
                        traceln!(
                            "[driver] new session in {workspace_path} -> instance {} (reused: {})",
                            created.id,
                            created.reused
                        );
                        self.new_session.open = false;
                        // target None = a FRESH session/new on connect, cwd
                        // = the picked workspace (the tab's identity).
                        self.open_tab(created.id, None, workspace_path);
                        self.spawn_discovery(false);
                    }
                    Err(e) => {
                        let msg = if e.status == Some(403) {
                            "this project is not in the hub's allowlist".to_string()
                        } else {
                            format!("create session failed: {e}")
                        };
                        self.new_session.projects_error = Some(msg);
                    }
                }
            }
            // ---- config pages ----
            LoopEvent::SystemStats(result) => self.on_system_stats(result),
            LoopEvent::BackendRestart(result) => self.on_backend_restart(result),
            LoopEvent::ConfigProbe(result) => self.on_config_probe(result),
            LoopEvent::ModelsLoaded(result) => self.on_models_loaded(result),
            LoopEvent::ModelUpsert { adding, result } => self.on_model_upsert(adding, result),
            LoopEvent::UsageAll(result) => self.on_usage_all(result),
            LoopEvent::UsageSection {
                platform,
                requested,
                result,
            } => self.on_usage_section(platform, requested, result),
            LoopEvent::QuotaSettingsAll(result) => self.on_quota_settings_all(result),
            LoopEvent::QuotaUsage { result } => self.on_quota_usage(result, now),
            LoopEvent::QuotaCards { provider_id, result } => {
                self.on_quota_cards(provider_id, result);
            }
            LoopEvent::QuotaSpent { result } => self.on_quota_spent(result),
            LoopEvent::QuotaHistoryRead { provider_id, result } => {
                self.on_quota_history_read(provider_id, result);
            }
            // ---- Session Files (batch F) ----
            LoopEvent::FilesList { seq, result } => self.on_files_list(seq, result),
            LoopEvent::FilesText { seq, result } => self.on_files_text(seq, result),
        }
    }

    /// Initialize landed for a tab: the Desktop Presence identity is on the
    /// wire (clientInfo rides `initialize`, protocol::acp_client) — attach.
    fn on_connect_result(&mut self, tab: u64, result: Result<AcpInitializeResult, String>, now: u64) {
        // Collect the spawn inputs before the mutable tab borrow.
        let mut fresh_session: Option<(AcpConnection, String)> = None;
        if let Some(t) = self.tab_mut(tab) {
            match result {
                Ok(init) => {
                    traceln!(
                        "[driver] tab {tab} connected; agent {}",
                        init.agent_version().unwrap_or("<unknown>")
                    );
                    t.core.on_initialize(init.agent_version());
                    t.agent_version = init.agent_version().map(str::to_string);
                    t.fs_capable = init.fs_capability();
                    t.core.conn_open = true;
                    t.reconnect_attempts = 0;
                    // A fresh wire knows nothing until events arrive; the
                    // active session's activity is restored by the replay
                    // below (appStore.ts "open" resets sessionStates).
                    t.core.session_states.clear();
                    t.state = TabPhase::Connected;
                    // Attach: an already-active session (reconnect) replays;
                    // otherwise the tab's target, else a fresh session/new.
                    match t.core.active_session_id.clone().or_else(|| t.target.clone()) {
                        Some(sid) => t.core.load_session(&sid),
                        None => {
                            let conn =
                                t.conn.clone().expect("ConnHandle precedes ConnectResult");
                            fresh_session = Some((conn, t.cwd.clone()));
                        }
                    }
                }
                Err(e) => {
                    traceln!("[driver] tab {tab} connect failed: {e}");
                    let delay = reconnect_delay_ms(t.reconnect_attempts);
                    t.reconnect_attempts += 1;
                    t.state = TabPhase::Reconnecting;
                    t.retry_at = Some(now + delay);
                    t.core.conn_open = false;
                }
            }
        }
        if let Some((conn, cwd)) = fresh_session {
            let tx = self.loop_tx.clone();
            tokio::spawn(async move {
                let res = conn
                    .request("session/new", session_new_params(&cwd))
                    .await
                    .map(|v| parse_session_new_result(&v))
                    .map_err(|e| e.0);
                let _ = tx.send(LoopEvent::NewSession { tab, result: res });
            });
        }
    }

    // ---- commands ----

    fn handle_cmd(&mut self, cmd: UiCommand, now: u64) -> Flow {
        // Session Files state must never outlive the session it belongs to
        // (tabs switch here; load/new events reconcile in the loop handler).
        self.files_reconcile();
        match cmd {
            // Consumed by the driver's guided-setup wait loop while it is
            // up; in the steady state there is no plan to confirm — ignore.
            UiCommand::ConfirmTokenWrite => {}
            // Connection Profile edits persist the book and restart the
            // lifecycle so the next dial uses the new book (CONTEXT.md).
            UiCommand::SwitchProfile { id } => {
                self.book.active_id = id;
                if let Err(e) = self.book.save(&protocol::profiles::servers_path()) {
                    traceln!("[driver] server book save failed: {e}");
                }
                return Flow::Restart;
            }
            UiCommand::AddServer {
                name,
                hub_url,
                token,
            } => {
                let id = format!("srv-{}-{}", self.clock.now_ms(), name.len());
                self.book
                    .upsert_active(protocol::hub::SavedServer {
                        id,
                        name,
                        hub_url,
                        token,
                    });
                if let Err(e) = self.book.save(&protocol::profiles::servers_path()) {
                    traceln!("[driver] server book save failed: {e}");
                }
                return Flow::Restart;
            }
            UiCommand::RemoveServer { id } => {
                let was_active = self.book.active_id.as_deref() == Some(id.as_str());
                self.book.remove(&id);
                if let Err(e) = self.book.save(&protocol::profiles::servers_path()) {
                    traceln!("[driver] server book save failed: {e}");
                }
                if was_active {
                    return Flow::Restart;
                }
            }
            UiCommand::SendPrompt { text, images } => {
                if let Some(t) = self.active_tab_mut() {
                    t.core.send_prompt(now, &text, images);
                }
            }
            UiCommand::CancelTurn => {
                if let Some(t) = self.active_tab_mut() {
                    if let (Some(sid), Some(conn)) =
                        (t.core.active_session_id.clone(), t.conn.clone())
                    {
                        conn.notify("session/cancel", json!({ "sessionId": sid }));
                    }
                }
            }
            UiCommand::SetConfigOption { config_id, value } => {
                if let Some(t) = self.active_tab_mut() {
                    t.core.set_config_option(&config_id, &value);
                }
            }
            UiCommand::ForceSendPending => {
                if let Some(t) = self.active_tab_mut() {
                    t.core.force_send_pending(now, None);
                }
            }
            UiCommand::ForceSendPendingAt { index } => {
                if let Some(t) = self.active_tab_mut() {
                    let outcome = t.core.force_send_pending(now, Some(index));
                    traceln!("[driver] force-send pending[{index}] -> {outcome:?}");
                }
            }
            UiCommand::DiscardPending { index } => {
                if let Some(t) = self.active_tab_mut() {
                    t.core.discard_pending(index);
                }
            }
            UiCommand::EditPending { index } => {
                let draft = self
                    .active_tab_mut()
                    .and_then(|t| t.core.edit_pending(index));
                if let Some(draft) = draft {
                    let _ = self
                        .ev_tx
                        .send(RuntimeEvent::ComposerPrefill(draft));
                }
            }
            UiCommand::LoadEarlier => {
                if let Some(t) = self.active_tab_mut() {
                    t.core.load_earlier(now);
                }
            }
            UiCommand::Retry => {
                self.close_all_tabs();
                return Flow::Restart;
            }
            UiCommand::SelectTab { id } => {
                if self.tab(id).is_some() {
                    self.active_tab = Some(id);
                }
            }
            UiCommand::CloseTab { id } => {
                self.close_tab(id);
            }
            UiCommand::OpenSession {
                instance_id,
                session_id,
            } => {
                // A tab already holding this session just activates
                // (appStore openSession's same-session branch).
                let existing = self.tabs.iter().find(|t| {
                    t.instance_id == instance_id
                        && (t.core.active_session_id.as_deref() == Some(session_id.as_str())
                            || t.target.as_deref() == Some(session_id.as_str()))
                });
                match existing.map(|t| t.id) {
                    Some(id) => self.active_tab = Some(id),
                    None => {
                        let cwd = self
                            .instances
                            .iter()
                            .find(|i| i.id == instance_id)
                            .and_then(|i| i.workspace.clone())
                            .filter(|w| !w.is_empty())
                            .unwrap_or_else(|| "/".to_string());
                        self.open_tab(instance_id, Some(session_id), cwd);
                        self.spawn_discovery(false);
                    }
                }
            }
            UiCommand::NewSessionToggle => {
                self.new_session.open = !self.new_session.open;
                if self.new_session.open {
                    // Paint the cached list at once; the fetch below
                    // refreshes it (the TS dialog's cached-then-fetch pair).
                    if self.new_session.projects.is_none() {
                        self.spawn_new_session_projects();
                    }
                } else {
                    // In-flight reads belong to the closed sheet.
                    self.new_session.seq += 1;
                    self.new_session.creating = None;
                }
            }
            UiCommand::NewSessionBrowse { path } => {
                if self.new_session.browse.is_none() {
                    self.new_session.browse = Some(FsBrowseView::default());
                }
                self.spawn_new_session_browse(path);
            }
            UiCommand::NewSessionCloseBrowse => {
                self.new_session.browse = None;
            }
            UiCommand::NewSessionCreate { workspace_path } => {
                if self.new_session.creating.is_some() {
                    return Flow::Continue; // mid-create — no navigation
                }
                self.spawn_new_session_create(workspace_path);
            }
            UiCommand::OpenHistorySession {
                workspace_path,
                session_id,
            } => {
                if let Some(sel) = self.history.selected.as_mut() {
                    if sel.resuming.is_some() {
                        return Flow::Continue;
                    }
                    sel.resuming = Some(session_id.clone());
                    sel.error = None;
                }
                self.spawn_resume(workspace_path, session_id);
            }
            UiCommand::HistoryToggle => {
                self.history.open = !self.history.open;
                if self.history.open {
                    if self.history.projects.is_none() {
                        self.spawn_history_projects();
                    }
                } else {
                    // A load-more still in flight belongs to the view we are
                    // leaving (the TS back() invalidation).
                    self.history.seq += 1;
                }
            }
            UiCommand::HistoryOpenProject { workspace_path } => {
                self.history.seq += 1;
                self.history.selected = Some(HistorySelectedInner {
                    workspace_path: workspace_path.clone(),
                    loading_page: true,
                    ..Default::default()
                });
                self.spawn_history_page(workspace_path, None);
            }
            UiCommand::HistoryBack => {
                self.history.seq += 1;
                self.history.selected = None;
            }
            UiCommand::HistoryLoadMore => {
                let Some(sel) = self.history.selected.as_ref() else { return Flow::Continue };
                if sel.loading_page || sel.loading_more || sel.next_cursor.is_none() {
                    return Flow::Continue;
                }
                let cursor = sel.next_cursor.clone();
                let workspace_path = sel.workspace_path.clone();
                if let Some(sel) = self.history.selected.as_mut() {
                    sel.loading_more = true;
                }
                // seq NOT bumped: this view owns the sequence until
                // back/openProject (the TS loadMore contract).
                self.spawn_history_page(workspace_path, cursor);
            }
            UiCommand::RefreshSessions => {
                self.spawn_discovery(true);
            }
            UiCommand::AnswerPermission {
                request_id,
                option_id,
            } => {
                if let Some(t) = self.active_tab_mut() {
                    // First answer wins; an unknown id (race already lost or
                    // settled) is a no-op, exactly like the TS store.
                    if let Some(out) = t.approvals.answer_permission(request_id, &option_id) {
                        if let Some(responder) = t.responders.remove(&request_id) {
                            responder.send(out.result);
                        }
                        for sid in &out.cleared_sessions {
                            let awaiting = t.approvals.has_pending(sid);
                            t.core.set_activity_awaiting(sid, awaiting);
                        }
                    }
                }
            }
            UiCommand::AnswerElicitation {
                request_id,
                answers,
            } => {
                if let Some(t) = self.active_tab_mut() {
                    if let Some(out) = t.approvals.answer_elicitation(request_id, answers.as_ref())
                    {
                        if let Some(responder) = t.responders.remove(&request_id) {
                            responder.send(out.result);
                        }
                        for sid in &out.cleared_sessions {
                            let awaiting = t.approvals.has_pending(sid);
                            t.core.set_activity_awaiting(sid, awaiting);
                        }
                    }
                }
            }
            UiCommand::DismissToast => {
                self.toast.dismiss();
            }
            UiCommand::OpenSystemStatus => {
                self.system.open = true;
                // Seed a read on EVERY open (the TS ZCodeConfigScreen mount
                // effect — not only the first): a reopened page refreshes
                // over its surviving snapshot.
                self.spawn_system_stats(now);
            }
            UiCommand::CloseSystemStatus => {
                self.system.open = false;
            }
            UiCommand::RefreshSystemStatus => {
                self.spawn_system_stats(now);
            }
            UiCommand::DismissConfigNotice => {
                self.config_notice = None;
            }
            UiCommand::RestartBackend => {
                self.spawn_backend_restart();
            }
            UiCommand::OpenModels => {
                self.models.open = true;
                // The TS mount pair, on EVERY open (not only the first): a
                // reopened page refreshes over its surviving payload, and
                // the probe only runs while the verdict is still unknown.
                if self.models.supported.is_none() {
                    self.spawn_config_probe();
                }
                self.spawn_models_load();
            }
            UiCommand::CloseModels => {
                self.models.open = false;
            }
            UiCommand::RefreshModels => {
                self.spawn_models_load();
            }
            UiCommand::UpsertModel { adding, body } => {
                // One write at a time (the form's Save disables while busy;
                // this is the same guard on the driver side).
                if !self.models.busy {
                    self.spawn_model_upsert(adding, body);
                }
            }
            UiCommand::OpenUsage => {
                // A fresh page mount (UsagePage.tsx): platform tab first —
                // the account-level view the page leads with — then the
                // mount loads.
                self.usage.open = true;
                self.usage.tab = UsageTab::Platform;
                self.spawn_usage_loads();
            }
            UiCommand::CloseUsage => {
                self.usage.open = false;
            }
            UiCommand::UsageTab { platform } => {
                self.usage.tab = if platform {
                    UsageTab::Platform
                } else {
                    UsageTab::Local
                };
                // The TS tab switch REMOUNTS the tab component, whose effect
                // refetches the section (default range included) — so a
                // switch always reads, even switching back.
                self.spawn_usage_section(None);
            }
            UiCommand::UsageRange { platform, range } => {
                self.usage.tab = if platform {
                    UsageTab::Platform
                } else {
                    UsageTab::Local
                };
                self.spawn_usage_section(Some(range));
            }
            UiCommand::OpenQuota => {
                self.quota.open = true;
                // The TS mount pair, on EVERY open (not only the first): the
                // /settings/all snapshot (capability + card eligibility) and
                // the quota read — a reopened page refreshes over its
                // surviving payloads.
                self.spawn_quota_settings_all();
                self.spawn_quota_usage();
            }
            UiCommand::CloseQuota => {
                self.quota.open = false;
            }
            UiCommand::RefreshQuota => {
                self.spawn_quota_usage();
            }
            UiCommand::QuotaPickProvider { provider_id } => {
                // The picker chips disable while busy (TS `resetBusy`).
                if !self.quota.busy {
                    self.spawn_quota_cards(provider_id);
                }
            }
            UiCommand::QuotaSpendCard {
                provider_id,
                nonce,
                reset_type,
            } => {
                // The view captured both halves at the confirmation; refuse
                // an unspendable pair (TS `if (!spendProviderId ||
                // !spendNonce) return`) and a provider switched since (the
                // nonce was minted for another inventory — spending it would
                // burn the wrong card).
                if provider_id.is_empty() || nonce.is_empty() {
                    return Flow::Continue;
                }
                if self
                    .quota
                    .provider_id
                    .as_deref()
                    .is_some_and(|p| p != provider_id)
                {
                    return Flow::Continue;
                }
                if !self.quota.busy {
                    self.spawn_quota_spend(provider_id, nonce, reset_type);
                }
            }
            // ---- Session Files (batch F) ----
            UiCommand::FilesNavigate { path } => {
                self.spawn_files_list(path);
            }
            UiCommand::FilesView { path } => {
                self.spawn_files_view(path);
            }
            UiCommand::FilesCloseView => {
                self.files.viewing = None;
            }
        }
        Flow::Continue
    }

    // ---- snapshot ----

    /// Distinct sessions whose Session Activity reads `running` across all
    /// tabs, plus each tab's optimistic local turn. This is the quit
    /// guard's number (requirements §4).
    fn running_sessions(&self) -> HashSet<String> {
        let mut running = HashSet::new();
        for tab in &self.tabs {
            for (sid, activity) in &tab.core.session_states {
                if activity.running {
                    running.insert(sid.clone());
                }
            }
            if tab.core.is_running {
                if let Some(sid) = &tab.core.active_session_id {
                    running.insert(sid.clone());
                }
            }
        }
        running
    }

    fn session_rows(&self, running: &HashSet<String>, awaiting: &HashSet<String>) -> Vec<SessionRow> {
        let mut rows = Vec::new();
        for inst in &self.instances {
            for sess in &inst.sessions {
                rows.push(SessionRow {
                    instance_id: inst.id.clone(),
                    workspace: inst.workspace.clone().unwrap_or_default(),
                    session_id: sess.session_id.clone(),
                    title: sess.title.clone(),
                    updated_at: sess.updated_at,
                    running: running.contains(&sess.session_id),
                    awaiting: awaiting.contains(&sess.session_id),
                    open_tab: self.tabs.iter().find_map(|t| {
                        (t.core.active_session_id.as_deref() == Some(sess.session_id.as_str())
                            || t.target.as_deref() == Some(sess.session_id.as_str()))
                        .then_some(t.id)
                    }),
                });
            }
        }
        rows
    }

    /// Distinct sessions whose Session Activity reads `awaiting
    /// confirmation` (a pending approval ask) across all tabs.
    fn awaiting_sessions(&self) -> HashSet<String> {
        let mut awaiting = HashSet::new();
        for tab in &self.tabs {
            for (sid, activity) in &tab.core.session_states {
                if activity.awaiting_permission || tab.approvals.has_pending(sid) {
                    awaiting.insert(sid.clone());
                }
            }
        }
        awaiting
    }

    fn tab_view(tab: &Tab) -> TabView {
        let pending = tab
            .core
            .pending_prompts
            .get(tab.core.active_session_id.as_deref().unwrap_or(""))
            .map(|q| {
                q.iter()
                    .map(|d| PendingDraftView {
                        text: d.text.clone(),
                        images: d.images.len(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        TabView {
            session_id: tab.core.active_session_id.clone(),
            agent_version: tab.agent_version(),
            phase: tab.state.clone(),
            running: tab.core.is_running,
            loading_session: tab.core.loading_session,
            messages: tab.core.messages.clone(),
            pending,
            has_more: tab.core.has_more,
            total_messages: tab.core.total_messages,
            loading_earlier: tab.core.loading_earlier,
            approval: tab
                .core
                .active_session_id
                .as_deref()
                .and_then(|sid| tab.approvals.pending_for_session(sid)),
            config_options: tab.core.config_options.clone(),
            usage: tab.core.usage,
            cache_hit: tab.core.cache_hit,
            turn_stats: tab.core.turn_stats,
            plan_entries: tab.core.plan_entries.clone(),
            fs_capable: tab.fs_capable,
        }
    }

    /// Cheap content signature deciding whether the tick owes the window a
    /// snapshot (idle ticks must not trigger redraws).
    fn fingerprint(&self) -> u64 {
        let mut sig: u64 = 17;
        let mix = |v: u64, sig: &mut u64| {
            *sig = sig.wrapping_mul(31).wrapping_add(v);
        };
        mix(self.phase.fingerprint(), &mut sig);
        mix(self.note.clone().map(|n| n.len() as u64).unwrap_or(u64::MAX), &mut sig);
        mix(self.discovery_note.clone().map(|n| n.len() as u64).unwrap_or(u64::MAX), &mut sig);
        mix(self.instances.len() as u64, &mut sig);
        for row in self.session_rows(&self.running_sessions(), &self.awaiting_sessions()) {
            mix(row.session_id.len() as u64, &mut sig);
            mix(row.updated_at.unwrap_or(0), &mut sig);
            mix(row.running as u64, &mut sig);
            mix(row.awaiting as u64, &mut sig);
            mix(row.open_tab.unwrap_or(0), &mut sig);
        }
        for tab in &self.tabs {
            mix(tab.id, &mut sig);
            mix(messages_signature(&tab.core.messages), &mut sig);
            mix(tab.core.is_running as u64, &mut sig);
            mix(tab.core.active_session_id.clone().map(|s| s.len() as u64).unwrap_or(u64::MAX), &mut sig);
            // The queue's CONTENT, not just its length — an edit/discard
            // swaps drafts without a count change.
            for d in tab
                .core
                .pending_prompts
                .get(tab.core.active_session_id.as_deref().unwrap_or(""))
                .into_iter()
                .flatten()
            {
                mix(d.text.len() as u64, &mut sig);
                mix(d.images.len() as u64, &mut sig);
            }
            mix(tab.core.loading_session as u64, &mut sig);
            mix(tab.core.loading_earlier as u64, &mut sig);
            mix(tab.core.has_more as u64, &mut sig);
            mix(tab.core.total_messages.unwrap_or(u64::MAX), &mut sig);
            mix(tab.state.fingerprint(), &mut sig);
            // Session Files gating: the files panel's capability note and
            // the dock-seed decision read it off the active tab.
            mix(tab.fs_capable as u64, &mut sig);
            // Config chips: the picked values (model/mode/thought) relabel
            // the tab toolbar's buttons.
            for opt in &tab.core.config_options {
                mix(opt.id.len() as u64, &mut sig);
                mix(
                    opt.current_value.as_ref().map(|v| v.len() as u64).unwrap_or(u64::MAX),
                    &mut sig,
                );
                mix(
                    opt.options.as_ref().map(|o| o.len() as u64).unwrap_or(u64::MAX),
                    &mut sig,
                );
            }
            // Title-bar readouts (E1): usage/cache/tps repaint the status
            // bar segments and the progress hairline.
            mix(
                tab.core
                    .usage
                    .map(|u| u.used.wrapping_mul(31).wrapping_add(u.size))
                    .unwrap_or(u64::MAX),
                &mut sig,
            );
            mix(tab.core.cache_hit.map(u64::from).unwrap_or(u64::MAX), &mut sig);
            mix(
                tab.core
                    .turn_stats
                    .map(|s| s.tps.to_bits() ^ s.output_tokens.rotate_left(17) ^ s.seconds.to_bits())
                    .unwrap_or(u64::MAX),
                &mut sig,
            );
            // The plan/todo strip (E3): entry count plus each entry's
            // content/status — a same-count status flip must still re-emit.
            mix(
                tab.core
                    .plan_entries
                    .as_ref()
                    .map(|entries| {
                        let mut s: u64 = entries.len() as u64;
                        for e in entries {
                            s = s.wrapping_mul(31).wrapping_add(fold_str(&e.content));
                            s = s.wrapping_mul(31).wrapping_add(
                                e.status.as_ref().map(|st| st.len() as u64).unwrap_or(u64::MAX),
                            );
                        }
                        s
                    })
                    .unwrap_or(u64::MAX),
                &mut sig,
            );
            // The pending approval card: request id + channel tag (the card
            // content itself is immutable once stored).
            if let Some(ask) = tab
                .core
                .active_session_id
                .as_deref()
                .and_then(|sid| tab.approvals.pending_for_session(sid))
            {
                mix(ask.request_id().wrapping_mul(2), &mut sig);
                mix(matches!(ask, PendingAsk::Elicitation(_)) as u64, &mut sig);
            } else {
                mix(u64::MAX, &mut sig);
            }
        }
        mix(self.toast.current().map(|t| t.id).unwrap_or(u64::MAX), &mut sig);
        mix(self.active_tab.unwrap_or(0), &mut sig);
        let h = &self.history;
        mix(h.open as u64, &mut sig);
        mix(h.seq, &mut sig);
        mix(h.projects.as_ref().map(|p| p.len() as u64).unwrap_or(u64::MAX), &mut sig);
        mix((h.projects_error.is_some()) as u64, &mut sig);
        if let Some(sel) = &h.selected {
            mix(sel.rows.len() as u64, &mut sig);
            mix(sel.loading_page as u64, &mut sig);
            mix(sel.loading_more as u64, &mut sig);
            mix(sel.error.is_some() as u64, &mut sig);
            mix(sel.resuming.is_some() as u64, &mut sig);
            mix(sel.next_cursor.is_some() as u64, &mut sig);
        }
        // The new-session chooser: open/close plus the fields the sheet
        // paints (project identity, browse directory, the busy row).
        let ns = &self.new_session;
        mix(ns.open as u64, &mut sig);
        mix(ns.seq, &mut sig);
        mix(
            ns.projects
                .as_ref()
                .map(|p| fold_str(&p.last().map(|x| x.workspace_path.clone()).unwrap_or_default()))
                .unwrap_or(u64::MAX),
            &mut sig,
        );
        mix((ns.projects_error.is_some()) as u64, &mut sig);
        mix(ns.creating.as_ref().map(|c| c.len() as u64).unwrap_or(u64::MAX), &mut sig);
        if let Some(b) = &ns.browse {
            mix(fold_str(&b.path), &mut sig);
            mix(b.entries.len() as u64, &mut sig);
            mix(b.creatable as u64, &mut sig);
            mix(b.truncated as u64, &mut sig);
            mix(b.loading as u64, &mut sig);
            mix(b.error.is_some() as u64, &mut sig);
        }
        // The config surfaces: page open/close, snapshot identity (the
        // hub's collectedAt changes every collect), the state flags, and the
        // notice/pending-restart pair.
        mix(self.system.open as u64, &mut sig);
        mix(
            self.system
                .stats
                .as_ref()
                .map(|s| s.collected_at)
                .unwrap_or(u64::MAX),
            &mut sig,
        );
        mix(self.system.loading as u64, &mut sig);
        mix(self.system.supported as u64, &mut sig);
        mix(self.system.error.is_some() as u64, &mut sig);
        mix(
            self.config_notice
                .as_ref()
                .map(|n| n.len() as u64)
                .unwrap_or(u64::MAX),
            &mut sig,
        );
        mix(self.pending_restart as u64, &mut sig);
        // The Models page: open/close, the state flags, and a content
        // signature of the payload — every field the page renders (a
        // same-count reload with an edited rule must still re-emit).
        mix(self.models.open as u64, &mut sig);
        mix(
            self.models.supported.map(|s| s as u64).unwrap_or(u64::MAX),
            &mut sig,
        );
        mix(self.models.loading as u64, &mut sig);
        mix(self.models.busy as u64, &mut sig);
        mix(self.models.error.is_some() as u64, &mut sig);
        mix(
            self.models
                .models
                .as_ref()
                .map(models_signature)
                .unwrap_or(u64::MAX),
            &mut sig,
        );
        // The Usage page: open/tab chrome, the verdicts, and content
        // signatures of both payloads (a same-count reload with different
        // numbers must still re-emit).
        mix(self.usage.open as u64, &mut sig);
        mix(self.usage.tab as u64, &mut sig);
        mix(
            self.usage.supported.map(|s| s as u64).unwrap_or(u64::MAX),
            &mut sig,
        );
        mix(self.usage.loading as u64, &mut sig);
        mix(self.usage.platform_supported as u64, &mut sig);
        mix(self.usage.platform_error.is_some() as u64, &mut sig);
        mix(fold_str(&self.usage.local_range), &mut sig);
        mix(
            self.usage
                .local
                .as_ref()
                .map(|u| fold_str(&serde_json::to_string(u).unwrap_or_default()))
                .unwrap_or(u64::MAX),
            &mut sig,
        );
        mix(fold_str(&self.usage.platform_range), &mut sig);
        mix(
            self.usage
                .platform
                .as_ref()
                .map(|p| fold_str(&serde_json::to_string(p).unwrap_or_default()))
                .unwrap_or(u64::MAX),
            &mut sig,
        );
        // The plan-quota page: open/close, the state flags, the allowance
        // and card-inventory content (the nonce included — a fresh nonce
        // must repaint the halves a spend gesture captures).
        mix(self.quota.open as u64, &mut sig);
        mix(
            self.quota.supported.map(|s| s as u64).unwrap_or(u64::MAX),
            &mut sig,
        );
        mix(self.quota.loading as u64, &mut sig);
        mix(self.quota.error.is_some() as u64, &mut sig);
        mix(self.quota.quota_unavailable as u64, &mut sig);
        mix(self.quota.usage_stats_at.unwrap_or(u64::MAX), &mut sig);
        mix(quota_payload_signature(&self.quota.usage), &mut sig);
        mix(self.quota.eligible.len() as u64, &mut sig);
        for id in &self.quota.eligible {
            mix(id.len() as u64, &mut sig);
        }
        mix(self.quota.credentials as u64, &mut sig);
        mix(
            self.quota
                .provider_id
                .as_ref()
                .map(|p| p.len() as u64)
                .unwrap_or(u64::MAX),
            &mut sig,
        );
        mix(self.quota.busy as u64, &mut sig);
        mix(
            self.quota
                .nonce
                .as_ref()
                .map(|n| n.len() as u64)
                .unwrap_or(u64::MAX),
            &mut sig,
        );
        mix(
            self.quota
                .cards
                .as_ref()
                .map(quota_cards_signature)
                .unwrap_or(u64::MAX / 2),
            &mut sig,
        );
        // The Session Files browser (F1/F3): the reset seq (a session switch
        // must repaint even between all-empty states), the current path,
        // the state flags, the listing content, and the viewer.
        mix(self.files.seq, &mut sig);
        mix(fold_str(&self.files.path), &mut sig);
        mix(self.files.loading as u64, &mut sig);
        mix(self.files.error.is_some() as u64, &mut sig);
        mix(
            self.files
                .listing
                .as_ref()
                .map(fs_listing_signature)
                .unwrap_or(u64::MAX),
            &mut sig,
        );
        mix(
            self.files
                .viewing
                .as_ref()
                .map(file_viewing_signature)
                .unwrap_or(u64::MAX / 2),
            &mut sig,
        );
        sig
    }

    fn emit_if_changed(&mut self) {
        let fp = self.fingerprint();
        if fp == self.last_fingerprint {
            return;
        }
        self.last_fingerprint = fp;
        let running = self.running_sessions();
        let running_session_count = running.len();
        let awaiting = self.awaiting_sessions();
        let sessions = self.session_rows(&running, &awaiting);
        let tabs: Vec<TabSummary> = self
            .tabs
            .iter()
            .map(|t| TabSummary {
                id: t.id,
                session_id: t.core.active_session_id.clone().or_else(|| t.target.clone()),
                phase: t.state.clone(),
                running: t.core.is_running,
                pending: pending_count(t),
                awaiting: t
                    .core
                    .active_session_id
                    .as_deref()
                    .map(|sid| t.approvals.has_pending(sid))
                    .unwrap_or(false),
            })
            .collect();
        let active = self
            .active_tab
            .and_then(|id| self.tab(id))
            .map(Self::tab_view);
        let history = HistoryPanelView {
            open: self.history.open,
            projects: self.history.projects.as_ref().map(|list| {
                list.iter()
                    .map(|p| HistoryProjectRow {
                        workspace_path: p.workspace_path.clone(),
                        sessions: p.sessions,
                        last_active: p.last_active,
                    })
                    .collect()
            }),
            projects_error: self.history.projects_error.clone(),
            selected: self.history.selected.as_ref().map(|sel| HistorySelected {
                workspace_path: sel.workspace_path.clone(),
                rows: sel
                    .rows
                    .iter()
                    .map(|r| HistoryRow {
                        session_id: r.session_id.clone(),
                        title: r.title.clone(),
                        updated_at: r.updated_at.clone(),
                        live: r.live.unwrap_or(false),
                    })
                    .collect(),
                next_cursor: sel.next_cursor.clone(),
                loading_page: sel.loading_page,
                loading_more: sel.loading_more,
                error: sel.error.clone(),
                resuming: sel.resuming.clone(),
            }),
        };
        let new_session = self.new_session.open.then(|| NewSessionView {
            projects: self.new_session.projects.as_ref().map(|list| {
                list.iter()
                    .map(|p| HistoryProjectRow {
                        workspace_path: p.workspace_path.clone(),
                        sessions: p.sessions,
                        last_active: p.last_active,
                    })
                    .collect()
            }),
            error: self.new_session.projects_error.clone(),
            browse: self.new_session.browse.clone(),
            creating: self.new_session.creating.clone(),
        });
        let _ = self.ev_tx.send(RuntimeEvent::Snapshot(WorkbenchSnapshot {
            phase: self.phase.clone(),
            hub_addr: Some(self.hub_url.clone()).filter(|_| !self.hub_url.is_empty()),
            bootstrap_note: self
                .note
                .clone()
                .or_else(|| self.discovery_note.clone()),
            sessions,
            tabs,
            active_tab: self.active_tab,
            active,
            history,
            running_session_count,
            toast: self.toast.current().cloned(),
            servers: self.book.servers.clone(),
            active_server_id: self.book.active_id.clone(),
            new_session,
            system_status: self.system.open.then(|| SystemStatusPageView {
                stats: self.system.stats.clone(),
                error: self.system.error.clone(),
                supported: self.system.supported,
                loading: self.system.loading,
            }),
            models_page: self.models.open.then(|| ModelsPageView {
                supported: self.models.supported,
                loading: self.models.loading,
                error: self.models.error.clone(),
                models: self.models.models.clone(),
                busy: self.models.busy,
            }),
            usage_page: self.usage.open.then(|| UsagePageView {
                tab: self.usage.tab,
                supported: self.usage.supported,
                loading: self.usage.loading,
                local: self.usage.local.clone(),
                local_range: self.usage.local_range.clone(),
                platform: self.usage.platform.clone(),
                platform_range: self.usage.platform_range.clone(),
                platform_supported: self.usage.platform_supported,
                platform_error: self.usage.platform_error.clone(),
            }),
            quota_page: self.quota.open.then(|| QuotaPageView {
                supported: self.quota.supported,
                loading: self.quota.loading,
                error: self.quota.error.clone(),
                quota_unavailable: self.quota.quota_unavailable,
                glm_kind: self.quota.usage.as_ref().map(|u| u.glm.kind.clone()),
                glm_items: self
                    .quota
                    .usage
                    .as_ref()
                    .and_then(|u| u.glm.items.clone())
                    .unwrap_or_default(),
                usage_stats_at: self.quota.usage_stats_at,
                eligible: self.quota.eligible.clone(),
                credentials: self.quota.credentials,
                provider_id: self.quota.provider_id.clone(),
                cards: self.quota.cards.clone(),
                nonce: self.quota.nonce.clone(),
                busy: self.quota.busy,
                next_try_at: self.quota.next_try_at,
                now_ms: self.clock.now_ms(),
            }),
            config_notice: self.config_notice.clone(),
            pending_restart: self.pending_restart,
            files: self.files.view(),
        }));
    }
}

impl Tab {
    fn agent_version(&self) -> Option<String> {
        // The core learns it per (re)connect through on_initialize; keep the
        // snapshot field for the status bar parity with M1.
        self.agent_version.clone()
    }
}

// Tab/Phase fingerprint helpers keep the PartialEq-free cheap compare.
trait Fingerprint {
    fn fingerprint(&self) -> u64;
}

impl Fingerprint for Phase {
    fn fingerprint(&self) -> u64 {
        match self {
            Phase::Bootstrapping => 1,
            Phase::Discovering => 2,
            Phase::Connected => 3,
            Phase::Reconnecting => 4,
            Phase::Failed(m) => 5u64.wrapping_mul(31).wrapping_add(m.len() as u64),
            Phase::TokenGuide {
                path,
                preview,
                error,
            } => 6u64
                .wrapping_mul(31)
                .wrapping_add(path.len() as u64)
                .wrapping_mul(31)
                .wrapping_add(preview.len() as u64)
                .wrapping_mul(31)
                .wrapping_add(error.as_ref().map(|e| e.len()).unwrap_or(0) as u64),
        }
    }
}

impl Fingerprint for TabPhase {
    fn fingerprint(&self) -> u64 {
        match self {
            TabPhase::Connecting => 1,
            TabPhase::Connected => 2,
            TabPhase::Reconnecting => 3,
            TabPhase::Failed(m) => 4u64.wrapping_mul(31).wrapping_add(m.len() as u64),
        }
    }
}

fn pending_count(tab: &Tab) -> usize {
    tab.core
        .pending_prompts
        .get(tab.core.active_session_id.as_deref().unwrap_or(""))
        .map(Vec::len)
        .unwrap_or(0)
}

/// Message-content signature (the M1 helper, per tab now).
fn messages_signature(messages: &[ChatMessage]) -> u64 {
    use protocol::chat::ChatPart;
    let mut sig: u64 = messages.len() as u64;
    for msg in messages {
        sig = sig.wrapping_mul(31).wrapping_add(msg.id.len() as u64);
        for part in &msg.parts {
            let part_sig = match part {
                ChatPart::Text { text } | ChatPart::Thought { text } => text.len() as u64,
                ChatPart::Image { image } => image.len() as u64,
                ChatPart::ToolCall(tc) => {
                    (tc.status.len() as u64)
                        .wrapping_mul(7)
                        .wrapping_add(tc.detail.len() as u64)
                }
            };
            sig = sig.wrapping_mul(31).wrapping_add(part_sig);
        }
    }
    sig
}

/// ProjectHistoryDialog's `describeListError` port: an older bridge lacks
/// the route (404); a dropped project leaves the known list (403).
fn describe_history_error(e: &HubApiError) -> String {
    if e.status == Some(404) {
        return "bridge 0.19.0+ required for session history".to_string();
    }
    if e.status == Some(403) {
        return "project unknown to the hub — refresh and re-pick".to_string();
    }
    format!("history: {e}")
}

/// The files panel's error story: a dead bridge is the one failure with an
/// action behind it (the hub revives the instance on its next use, then a
/// retry works); everything else is the plain wire error.
fn describe_files_error(e: &HubApiError) -> String {
    if e.instance_dead {
        return "the bridge serving this session went away — it revives on its \
                next use; retry afterwards"
            .to_string();
    }
    format!("files: {e}")
}

/// Content signature of a listing for the fingerprint: root, truncation,
/// and every entry's identity (name/kind/size) — a same-count reload with
/// a changed entry must still re-emit.
fn fs_listing_signature(listing: &FsListing) -> u64 {
    let mut sig: u64 = 19;
    sig = sig.wrapping_mul(31).wrapping_add(fold_str(&listing.root));
    sig = sig.wrapping_mul(31).wrapping_add(listing.truncated as u64);
    sig = sig.wrapping_mul(31).wrapping_add(listing.entries.len() as u64);
    for e in &listing.entries {
        sig = sig.wrapping_mul(31).wrapping_add(fold_str(&e.name));
        sig = sig.wrapping_mul(31).wrapping_add(e.size);
        sig = sig.wrapping_mul(31).wrapping_add(match e.kind {
            FsEntryKind::File => 1,
            FsEntryKind::Dir => 2,
            FsEntryKind::Symlink => 3,
        });
    }
    sig
}

/// Content signature of the open viewer for the fingerprint: identity,
/// kind, the loaded window's extent, and the state flags.
fn file_viewing_signature(v: &FileViewing) -> u64 {
    let mut sig: u64 = 23;
    sig = sig.wrapping_mul(31).wrapping_add(fold_str(&v.path));
    sig = sig.wrapping_mul(31).wrapping_add(v.size);
    sig = sig.wrapping_mul(31).wrapping_add(match v.kind {
        FileKind::Image => 1,
        FileKind::Text => 2,
        FileKind::Binary => 3,
    });
    sig = sig.wrapping_mul(31).wrapping_add(v.first_line.unwrap_or(u64::MAX));
    sig = sig
        .wrapping_mul(31)
        .wrapping_add(v.text.as_ref().map(|t| t.len() as u64).unwrap_or(u64::MAX));
    sig = sig.wrapping_mul(31).wrapping_add(v.loading as u64);
    sig = sig.wrapping_mul(31).wrapping_add(v.error.is_some() as u64);
    sig
}

/// FNV-1a over a string — the usage payloads' content signature for the
/// fingerprint (a length-only mix would miss same-length rewrites, and the
/// page renders every number in the payload).
fn fold_str(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Content signature of the models payload for the fingerprint: everything
/// the page renders — row identity, the rule joins (context window,
/// reasoning levels, enabled), and the provider list the add-form offers.
fn models_signature(payload: &ModelsPayload) -> u64 {
    let mut sig: u64 = 7;
    let mix = |v: u64, sig: &mut u64| {
        *sig = sig.wrapping_mul(31).wrapping_add(v);
    };
    let str_sig = |s: &Option<String>| match s {
        Some(s) if s.is_empty() => 1,
        Some(s) => s.len() as u64 + 2,
        None => 0,
    };
    if let Some(block) = &payload.models {
        match &block.available {
            Some(rows) => {
                mix(rows.len() as u64, &mut sig);
                for m in rows {
                    mix(str_sig(&m.provider_id), &mut sig);
                    mix(str_sig(&m.provider_name), &mut sig);
                    mix(str_sig(&m.model_id), &mut sig);
                }
            }
            None => mix(u64::MAX, &mut sig),
        }
        match &block.providers {
            Some(rows) => {
                mix(rows.len() as u64, &mut sig);
                for p in rows {
                    mix(str_sig(&p.provider_id), &mut sig);
                    mix(str_sig(&p.provider_name), &mut sig);
                }
            }
            None => mix(u64::MAX, &mut sig),
        }
        match &block.model_rules {
            Some(rows) => {
                mix(rows.len() as u64, &mut sig);
                for r in rows {
                    mix(str_sig(&r.provider_id), &mut sig);
                    mix(str_sig(&r.model_id), &mut sig);
                    mix((r.enabled == Some(false)) as u64, &mut sig);
                    mix(r.context_window.unwrap_or(u64::MAX), &mut sig);
                    mix(
                        r.reasoning_levels
                            .as_ref()
                            .map(|l| l.len() as u64)
                            .unwrap_or(u64::MAX),
                        &mut sig,
                    );
                }
            }
            None => mix(u64::MAX, &mut sig),
        }
    } else {
        mix(u64::MAX, &mut sig);
    }
    sig
}

// ---------------------------------------------------------------------------
// Plan-quota helpers
// ---------------------------------------------------------------------------

/// Idempotency key for one reset-card GESTURE (the configSlice mint): a
/// spent card is gone, so the backend keys the spend on this value — one
/// key per gesture, reused on every retry. Same base36 shape as the TS
/// mint; the entropy source is a process-wide sequence, which is uniqueness
/// enough for keys that live between two calls.
fn new_reset_idempotency_key() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}-{}", to_base36(now.as_millis() as u64), to_base36(seq))
}

fn to_base36(mut value: u64) -> String {
    if value == 0 {
        return "0".to_string();
    }
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    while value > 0 {
        out.push(DIGITS[(value % 36) as usize]);
        value /= 36;
    }
    out.reverse();
    String::from_utf8(out).expect("base36 digits are ascii")
}

/// FNV-1a fold of a serialized payload — the fingerprint's content
/// signature for the quota section.
fn quota_str_sig(text: &str) -> u64 {
    text.bytes().fold(1469598103934665603u64, |h, b| {
        (h ^ b as u64).wrapping_mul(1099511628211)
    })
}

/// Content signature of the quota payload: the GLM section the page
/// renders (a same-count reload with a changed percent must re-emit).
fn quota_payload_signature(usage: &Option<AccountUsageStats>) -> u64 {
    let Some(u) = usage else {
        return u64::MAX;
    };
    quota_str_sig(&serde_json::to_string(&u.glm).unwrap_or_default())
}

/// Content signature of the card inventory: counts, each expiry, the
/// latest-use stamps, and the unread badge (the nonce is mixed separately).
fn quota_cards_signature(cards: &ResetCardInventory) -> u64 {
    let mut sig: u64 = 13;
    let mix = |v: u64, sig: &mut u64| {
        *sig = sig.wrapping_mul(31).wrapping_add(v);
    };
    mix(cards.available_five_hour.len() as u64, &mut sig);
    mix(cards.available_week.len() as u64, &mut sig);
    for card in cards.available_five_hour.iter().chain(&cards.available_week) {
        mix(card.expire_at, &mut sig);
    }
    mix(
        cards
            .latest_five_hour
            .as_ref()
            .map(|u| u.used_at)
            .unwrap_or(u64::MAX),
        &mut sig,
    );
    mix(
        cards
            .latest_week
            .as_ref()
            .map(|u| u.used_at)
            .unwrap_or(u64::MAX),
        &mut sig,
    );
    mix(cards.has_unread_history as u64, &mut sig);
    sig
}

// ---------------------------------------------------------------------------
// Instance selection
// ---------------------------------------------------------------------------

/// The M1 attach rule, mirroring the supervisor's auto-attach: the most
/// recently updated session across instances; a session-less instance gets a
/// fresh `session/new` in its workspace. Returns
/// `(instance id, attach session id, cwd)`.
fn pick_target(instances: &[HubInstance]) -> Option<(String, Option<String>, String)> {
    let mut best: Option<(&HubInstance, u64, String)> = None;
    for inst in instances {
        for sess in &inst.sessions {
            let updated = sess.updated_at.unwrap_or(0);
            let better = match &best {
                Some((_, best_updated, _)) => updated > *best_updated,
                None => true,
            };
            if better {
                best = Some((inst, updated, sess.session_id.clone()));
            }
        }
    }
    if let Some((inst, _, sid)) = best {
        return Some((inst.id.clone(), Some(sid), workspace_or_root(inst)));
    }
    instances
        .first()
        .map(|inst| (inst.id.clone(), None, workspace_or_root(inst)))
}

fn workspace_or_root(inst: &HubInstance) -> String {
    inst.workspace
        .clone()
        .filter(|w| !w.is_empty())
        .unwrap_or_else(|| "/".to_string())
}

// ---------------------------------------------------------------------------
// The driver loop
// ---------------------------------------------------------------------------

async fn driver(cmd_rx: UnboundedReceiver<UiCommand>, ev_tx: UnboundedSender<RuntimeEvent>) {
    let factory: Arc<dyn WsFactory> = Arc::new(TungsteniteWsFactory);
    let clock: Arc<dyn Clock> = Arc::new(TokioClock);
    let (loop_tx, mut loop_rx) = unbounded_channel::<LoopEvent>();
    let mut d = Driver::new(cmd_rx, ev_tx, factory, clock, loop_tx);
    // Bootstrap failures are usually transient (cold daemon, slow
    // registration); each lifecycle round that fails bumps this and the next
    // auto-retry backs off like a dropped tab socket.
    let mut bootstrap_attempts: u32 = 0;

    'lifecycle: loop {
        // ---- Hub Bootstrap (requirements §3): read the local config, probe
        // the port, spawn the daemon detached when dark (joining the EADDRINUSE
        // singleton race), connect to whichever daemon won.
        d.phase = Phase::Bootstrapping;
        d.note = None;
        d.instances.clear();
        d.close_all_tabs();
        // The config connection-reset patch (TS `connectionResetPatch`): the
        // dashboard and its capability verdict describe ONE machine — re-probe
        // on the next hub instead of trusting the old one. `pending_restart`
        // is the one that can do damage: left armed across a switch, the next
        // machine's screens would offer a restart whose backend never had the
        // write, cancelling its in-flight turns for nothing.
        d.system.reset_data();
        d.models.reset_data();
        d.quota.reset_data();
        d.usage.reset_data();
        // Session Files belong to a (hub, instance, session) triple — a new
        // lifecycle means a new machine's paths; nothing survives.
        d.files.reset();
        d.config_notice = None;
        d.pending_restart = false;
        d.emit_if_changed();

        // Connection Profile (CONTEXT.md): an active remote book entry dials
        // its saved hub directly — Hub Bootstrap is the local-hub path only;
        // a remote hub is never spawned, only dialed.
        let remote = d.book.active().cloned();
        let config = if let Some(server) = remote {
            traceln!("[driver] remote profile '{}' active; dialing {}", server.name, server.hub_url);
            d.hub_url = server.hub_url.clone();
            d.token = server.token.clone();
            d.note = Some(format!("remote: {}", server.name));
            None
        } else {
            let env = BootstrapEnv::from_env();
            if !assess_local_hub(&env).enabled {
                d.note =
                    Some("remote access disabled in ~/.config/zcode-acp/config.json".to_string());
            }
            let config = match ensure_hub(&env, &EnsureOptions::default()).await {
            Ok((outcome, config)) => {
                d.note = Some(
                    match outcome {
                        protocol::bootstrap::EnsureHubOutcome::AlreadyListening => {
                            "hub already listening".to_string()
                        }
                        protocol::bootstrap::EnsureHubOutcome::SpawnWon => {
                            "hub daemon spawned".to_string()
                        }
                        protocol::bootstrap::EnsureHubOutcome::SpawnLostToExisting => {
                            "spawn lost the port race; existing hub serves".to_string()
                        }
                    },
                );
                config
            }
            Err(err) => {
                traceln!("[driver] bootstrap failed: {err}");
                // MissingToken is not a failure — it is the guided-setup
                // entry (requirements §3.1): plan a token, surface the exact
                // write, and wait for confirmation before anything lands.
                if matches!(err, protocol::bootstrap::BootstrapError::MissingToken) {
                    match protocol::bootstrap::plan_guided_token_setup(&env) {
                        Ok((_token, plan)) => {
                            d.phase = Phase::TokenGuide {
                                path: plan.path.display().to_string(),
                                preview: plan.content.clone(),
                                error: None,
                            };
                            d.emit_if_changed();
                            loop {
                                match d.cmd_rx.recv().await {
                                    None => return,
                                    Some(UiCommand::ConfirmTokenWrite) => {
                                        match apply_token_write(&plan) {
                                            Ok(()) => {
                                                traceln!(
                                                    "[driver] token written to {}",
                                                    plan.path.display()
                                                );
                                                continue 'lifecycle;
                                            }
                                            Err(e) => {
                                                d.phase = Phase::TokenGuide {
                                                    path: plan.path.display().to_string(),
                                                    preview: plan.content.clone(),
                                                    error: Some(e.to_string()),
                                                };
                                                d.emit_if_changed();
                                            }
                                        }
                                    }
                                    // Retry re-plans (a fresh token) through
                                    // the lifecycle; anything else waits.
                                    Some(UiCommand::Retry) => continue 'lifecycle,
                                    Some(_) => {}
                                }
                            }
                        }
                        Err(pe) => {
                            d.phase = Phase::Failed(pe.to_string());
                        }
                    }
                } else {
                    d.phase = Phase::Failed(err.to_string());
                }
                d.emit_if_changed();
                // Auto-retry on the same backoff curve as a dropped tab
                // socket; a manual Retry still short-circuits the wait.
                let delay = reconnect_delay_ms(bootstrap_attempts);
                bootstrap_attempts += 1;
                if !wait_for_retry(&mut d.cmd_rx, Some(Duration::from_millis(delay))).await {
                    return;
                }
                continue 'lifecycle;
            }
            };
            Some(config)
        };
        bootstrap_attempts = 0;
        // The local path resolves the dial target here; a remote profile
        // already set both before the branch.
        if let Some(config) = config {
            d.hub_url = format!(
                "http://{}:{}",
                dial_host(&config.hub_host),
                config.hub_port
            );
            d.token = config.token.unwrap_or_default();
        }
        traceln!(
            "[driver] hub ready {} ({})",
            d.hub_url,
            d.note.as_deref().unwrap_or("")
        );

        // ---- Steady state: one 120ms tick, commands, and every tab's wire
        // events. Discovery (and the initial auto-tab) ride the same loop;
        // replay (a fresh `session/load`) is each tab's catch-up.
        d.phase = Phase::Discovering;
        d.next_discovery_at = 0; // fire on the first tick
        // A config page the user left open through the re-bootstrap re-seeds
        // itself against the new hub (its data did not survive the switch).
        if d.models.open {
            d.spawn_config_probe();
            d.spawn_models_load();
        }
        // The quota page re-seeds itself against the new hub the same way
        // (its payloads — and the provider session with its nonce — did not
        // survive the switch).
        if d.quota.open {
            d.spawn_quota_settings_all();
            d.spawn_quota_usage();
        }
        // The usage page re-runs its mount loads on the new hub the same
        // way (probe + section read; its payloads did not survive the
        // switch). The TS app blanks the open page across a hub switch;
        // asking again matches the page's own mount semantics.
        if d.usage.open {
            d.spawn_usage_loads();
        }
        d.emit_if_changed();

        let mut batch = tokio::time::interval(Duration::from_millis(UPDATE_BATCH_MS));
        batch.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let now = d.clock.now_ms();
            tokio::select! {
                _ = batch.tick() => {
                    d.tick(now);
                }
                cmd = d.cmd_rx.recv() => {
                    match cmd {
                        None => {
                            d.close_all_tabs();
                            return;
                        }
                        Some(c) => match d.handle_cmd(c, now) {
                            Flow::Continue => {}
                            Flow::Restart => continue 'lifecycle,
                            Flow::Quit => {
                                d.close_all_tabs();
                                return;
                            }
                        },
                    }
                }
                ev = loop_rx.recv() => {
                    match ev {
                        None => {}
                        Some(e) => d.handle_loop_event(e, now),
                    }
                }
            }
            d.pump_all();
            d.emit_if_changed();
        }
    }
}

/// `session/load_earlier` envelope (appStore.ts: `{sessionId, before, limit}`
/// — `before` is the stored cursor, passed back VERBATIM).
fn load_earlier_params(req: &LoadEarlierRequest) -> Value {
    json!({
        "sessionId": req.session_id,
        "before": req.before,
        "limit": req.limit,
    })
}

/// `session/prompt` envelope; the content blocks were built by SessionCore
/// (`run_prompt`): text first, then image attachments.
fn prompt_params(req: &PromptRequest) -> Value {
    json!({
        "sessionId": req.session_id,
        "prompt": req.prompt,
    })
}

/// Persist a confirmed guided-setup plan: create the config directory when
/// missing, then write the byte-exact preview body. protocol::bootstrap
/// deliberately exposes no write API — the caller persists only after the
/// user confirmed the exact content (requirements §3.1).
fn apply_token_write(plan: &protocol::bootstrap::TokenWritePlan) -> std::io::Result<()> {
    if let Some(dir) = plan.path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&plan.path, &plan.content)
}

/// Wait for a manual Retry, optionally auto-retrying after `auto`. Returns
/// false when the driver should shut down (window handle dropped).
async fn wait_for_retry(
    cmd_rx: &mut UnboundedReceiver<UiCommand>,
    auto: Option<Duration>,
) -> bool {
    if let Some(delay) = auto {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                None | Some(UiCommand::Retry) => true,
                _ => true,
            },
            _ = tokio::time::sleep(delay) => true,
        }
    } else {
        loop {
            match cmd_rx.recv().await {
                None | Some(UiCommand::Retry) => return true,
                _ => {}
            }
        }
    }
}

// Keep the HashMap import exercised for the tab-view bookkeeping contract
// documented in workbench.rs (per-tab identity caches keyed by tab id).
#[allow(dead_code)]
type _TabViewMap = HashMap<u64, ()>;
