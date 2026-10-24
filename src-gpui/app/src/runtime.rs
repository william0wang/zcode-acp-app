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

// The gpui engine surfaces only through the kit here (the app never adds a
// direct gpui dependency); Image/ImageFormat ride the kit's re-export.
use gpui_kit::{Image, ImageFormat};

use protocol::acp::SessionUpdate;
use protocol::acp_client::{
    parse_session_new_result, session_load_params, session_new_params, AcpConnection,
    AcpConnectionState, AcpHandlers, AcpInitializeResult, ServerRequest, ServerResponder,
    SessionAttachInfo, TurnStateInfo,
};
use protocol::approval::{ApprovalCore, ElicitAnswers, IncomingAsk, PendingAsk};
use protocol::bootstrap::{assess_local_hub, dial_host, ensure_hub, BootstrapEnv, EnsureOptions};
use protocol::chat::{ChatMessage, PromptDraft};
use protocol::clock::{Clock, TokioClock};
use protocol::config_decode::ModelsPayload;
use protocol::config_store::{
    NOTICE_NEEDS_RESTART, NOTICE_RESET_SPEND_FAILED, NOTICE_RESET_SPENT, NOTICE_RESET_STALE,
    NOTICE_RESTARTED, NOTICE_RESTART_FAILED,
};
use protocol::quota::{AccountUsageStats, QuotaItem};
use protocol::settings::{
    AgentUpsert, Effect, HookEntryPatch, McpServerUpsert, ModelUpsert, PendingRestartStatus,
    ResetCardInventory, ResetCardStatus, ResetCardType, SettingsAll, SettingsPlatformUsage,
    SettingsUsage, SpendResetCard, WriteEffect, WorkflowGateBlock, WorkflowGateModeSetting,
    WorkflowGateSetResponse,
};
use protocol::hub::{
    FsBrowseResult, FsEntryKind, FsListing, HubCreateInstanceResult, HubHistoryCursor,
    HubHistoryPage, HubHistorySession, HubInstance, HubProject, HubUpgradeResult, InstanceOrigin,
};
use protocol::hub_client::{HubApiError, HubClient};
use protocol::session::{LoadEarlierRequest, PromptRequest, SessionCore, WireRequest};
use protocol::settings::BackendRestartResult;
use protocol::settings_client::{HubApiError as SettingsApiError, HubHttpClient};
use rust_i18n::t;
use protocol::system_stats::SystemStatsResponse;
use protocol::toast::{Toast, ToastState};
use protocol::ws::{TungsteniteWsFactory, WsFactory};
use protocol::workflow::{
    AmendRunSettingsBody, AmendRunSettingsResponse, ConversationRunSummary, ConversationRunsResponse,
    DismissRunResponse, DismissRunsResponse, RunArtifactsResponse, WorkflowArtifact,
    WorkflowArtifactItemsResponse,
    WorkflowArtifactReadResponse, WorkflowCreatePromptResponse, WorkflowDetailResponse,
    WorkflowNodeResultResponse, WorkflowOverviewResponse, WorkflowRunEvent,
    WorkflowRunEventsResponse, WorkflowRunRow, WorkflowRunsHistoryResponse, WorkflowScope,
    WorkflowStartResponse, WorkflowWorkspaceNode, WorkflowWorkspaceResponse,
};
use protocol::workflow_run_detail::{
    artifact_is_preset, binary_verdict, decode_base64, latest_version, map_artifact_items,
    merge_artifact_items, ArtifactDataItem, READ_CHUNK,
};
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::file_browser::{file_kind_of, viewer_next_window, viewer_window, FileKind};
use crate::http_transport::{LocalHttpTransport, SettingsApiTransport};
use crate::pending_store::{merge_pending_queues, pending_path, PendingStore};
use crate::readout_store::{readouts_path, ReadoutStore, SessionReadout};
use crate::workflow_launches::{launches_path, WorkflowLaunchStore};

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
/// Sidebar discovery refresh cadence once the hub answers. Also the hub's
/// Desktop Presence heartbeat (ADR-0010 amendment): every poll carries the
/// desktop identity header and the hub keeps the app "present" for 45s past
/// the last one (server DESKTOP_PRESENCE_TTL_MS) — raising this past ~40s
/// would let presence lapse between polls and reopen terminal windows for
/// phone-originated sessions while the app runs.
const DISCOVERY_REFRESH_MS: u64 = 15_000;
/// How long a closed session stays hidden from incoming discovery lists —
/// long enough to cover the hub's ≤10s heartbeat staleness plus a bridge
/// re-advertising a retired conversation, short enough that a session
/// genuinely reused elsewhere (ADR-0006's re-arm) comes back promptly.
const CLOSE_TOMBSTONE_MS: u64 = 60_000;
/// Failed reconnects after which a tab whose instance has left discovery is
/// re-incubated (POST /api/instances, the resume form). Three honest retries
/// first: a hub restart's ≤10s bridge re-registration window must not
/// re-spawn bridges that are coming back on their own.
const TAB_REINCUBATE_ATTEMPTS: u32 = 3;
/// Minimum spacing between one tab's auto re-incubations — a backend that
/// dies on arrival must not loop the spawn.
const TAB_REINCUBATE_COOLDOWN_MS: u64 = 60_000;
/// Re-probe cadence while the hub is up but no bridge instance exists yet.
const NO_INSTANCE_RETRY_MS: u64 = 5_000;
/// System-status poll cadence while the page is open (TS SystemStatusPage
/// `POLL_MS`): a live dashboard — and the hub caches one collect for 1s, so
/// clients must not cache.
const SYSTEM_STATS_POLL_MS: u64 = 5_000;
/// Workflows overview poll cadence (TS WorkflowsPage POLL_LIVE_MS /
/// POLL_IDLE_MS): fast while anything is live anywhere (a journal row can
/// be terminal-and-wrong — bridge-side correction happens per read), slow
/// once everything is settled.
const WORKFLOWS_POLL_LIVE_MS: u64 = 5_000;
const WORKFLOWS_POLL_IDLE_MS: u64 = 30_000;
/// Run-detail journal poll cadence while the run is unsettled (TS
/// WorkflowRunDetail's 3s tick): live-ness is polling, not push.
const WORKFLOW_RUN_POLL_MS: u64 = 3_000;
/// One workflow's runs-history poll cadence while its newest run flies (TS
/// WorkflowDetail's 5s tick, `skipDetail` mode: meta is static between
/// runs, only history + summaries refresh).
const WORKFLOW_DETAIL_POLL_MS: u64 = 5_000;
/// The active-session conversationRuns poll cadence (TS ActiveRunsBlock's
/// 5s interval): the third session-resolution ladder's map refresh.
const WORKFLOW_SESSION_RUNS_POLL_MS: u64 = 5_000;
/// App Update progress poll cadence while the hub downloads (TS
/// AppUpdatePage's 2s `pollAppUpdate`): the download runs in the hub, and
/// the only channel back is the same GET that reports the check.
const APP_UPDATE_POLL_MS: u64 = 2_000;
/// How long the hub-upgrade verdict note stays (TS SidePanel's 5s
/// `setTimeout(() => setUpgradeNote(null))`).
const HUB_UPGRADE_NOTE_MS: u64 = 5_000;

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
    /// The quit dialog's stop-all: `session/cancel` on EVERY tab whose turn
    /// this app started (`local_prompt_active`), not just the active one.
    /// Turns other clients started are not ours to stop.
    CancelOwnTurns,
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
    /// Edit a saved server in place (TS `saveServer`, appStore.ts:1696):
    /// keep the active id; editing the ACTIVE server restarts the dial so
    /// the new URL/token takes effect immediately.
    UpdateServer {
        id: String,
        name: String,
        hub_url: String,
        token: String,
    },
    /// The settings drawer's hub staleness check (TS SettingsPanel
    /// `checkHubUpgrade`, POST /api/upgrade): the hub alone decides whether
    /// to restart onto newer on-disk code; discovery's poll and the tabs'
    /// reconnect ride out the respawn.
    UpgradeHub,
    /// The App Update page's channel segment (TS `setUpdateChannel`):
    /// persisted (the TS `saveUpdateChannel`), and while the page is open
    /// the check re-runs for the new stream — the stored verdict was read
    /// for the OTHER channel, and an install must name the stream the
    /// offered version came from.
    SetUpdateChannel { channel: String },
    /// The App Update page's confirmed install (TS `installAppUpdate`, ADR
    /// 0009): the hub downloads + verifies the artifact itself; this app
    /// only watches the outcome. The channel travels with the request — the
    /// route refuses a version that is not the latest on the channel it is
    /// told.
    InstallAppUpdate {
        version: String,
        url: String,
        channel: String,
    },
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
    /// Drag-reorder (the tab strip): move `id` in front of `before_id`, or
    /// to the strip's end when `None`. Pure strip order — no connection
    /// moves with it.
    MoveTab {
        id: u64,
        before_id: Option<u64>,
    },
    /// Tab context menu: close every tab except `id`.
    CloseOtherTabs { id: u64 },
    /// Tab context menu: close every tab to the right of `id`.
    CloseRightTabs { id: u64 },
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
    // ---- session row actions (batch G-light) ----
    /// A sidebar session row's Rename (appStore `renameSession`): the hub's
    /// HTTP rename — the bridge pins the title and broadcasts
    /// session_info_update so attached editors update live. The driver
    /// applies the TS action's trim + 80-char cap before sending.
    RenameSession {
        instance_id: String,
        session_id: String,
        title: String,
    },
    /// A sidebar session row's End session (appStore `closeRemoteSession`):
    /// retire the conversation from remote discovery (ADR-0006 — close, not
    /// delete). Refused with 409 while a turn is running.
    CloseSession {
        instance_id: String,
        session_id: String,
    },
    /// A history row's Delete from history (ADR-0031 tombstone): hides the
    /// conversation from every listing; nothing is purged. The driver
    /// resolves the serving instance from the selected history page — the
    /// same `page.instance.id` the TS dialog keeps for its deletes.
    DeleteHistorySession { session_id: String },
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
    /// DELETE /settings/models/{providerId}/{modelId} — removes the model's
    /// rule and its entry in the provider's model list, so it stops being
    /// offered anywhere. Two-gesture confirmed in the view.
    RemoveModel {
        provider_id: String,
        model_id: String,
    },
    /// PUT /settings/providers/{id} — a provider's endpoint (base URL / API
    /// type / key) and its enabled flag. `apiKey: None` keeps the stored key
    /// (the server never sends it back, so a blank box cannot mean "clear").
    UpdateProvider {
        provider_id: String,
        enabled: Option<bool>,
        provider_name: Option<String>,
        base_url: Option<String>,
        api_type: Option<String>,
        api_key: Option<String>,
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
    /// Open the plan-quota drawer. Every open re-runs the TS mount pair: the
    /// /settings/all snapshot (capability + card eligibility) and the quota
    /// read (QuotaPage's `loadConfigSection("quota")` →
    /// `refreshUsageStats`).
    OpenQuota,
    /// The drawer's close gesture (X, Esc, the rail toggle) — close it. The
    /// payloads survive (the TS store keeps them), so a reopen paints
    /// instantly and re-seeds.
    CloseQuota,
    /// The drawer header's refresh control (TS `refreshUsageStats`).
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
    // ---- ZCode config sections (batch I: skills / mcp / hooks / agents /
    // backups) ----
    /// Open one config-section page. Every open re-runs the TS mount pair:
    /// the capability probe (while the verdict is unknown) and the section
    /// read; the Agents page additionally seeds the models read its picker
    /// needs (TS AgentsPage's `loadConfigSection("models")` effect).
    OpenConfigPage { page: ConfigPageKind },
    /// The page's back gesture — close it. Payloads survive (the TS store
    /// keeps them), so a reopen paints instantly and re-reads.
    CloseConfigPage { page: ConfigPageKind },
    /// The page header's refresh control (the section read again).
    RefreshConfigPage { page: ConfigPageKind },
    /// POST /settings/skills/enable (TS `setSkillEnabled`) — immediate.
    SetSkillEnabled { path: String, enable: bool },
    /// POST /settings/skills/copy-to-user (TS `copySkillToUser`).
    CopySkillToUser { path: String },
    /// DELETE /settings/skills/{path} (TS `deleteSkill`) — removes the skill
    /// directory; the view confirms first (unrecoverable). Only rows the
    /// server marks `deletable` offer the control.
    DeleteSkill { path: String },
    /// PUT /settings/mcp/{name} (TS `upsertMcp`): add or edit one server;
    /// the body carries only its transport's fields (config_decode's
    /// `mcp_upsert_body`). `adding` names the failure label only.
    UpsertMcpServer {
        adding: bool,
        name: String,
        body: McpServerUpsert,
    },
    /// POST /settings/mcp/enable (TS `setMcpEnabled`) — the row switch,
    /// without opening the form. The route reads `enabled`, not `enable`.
    SetMcpEnabled { name: String, enable: bool },
    /// DELETE /settings/mcp/{name} (TS `removeMcp`). The view confirms
    /// first — the entry is gone from the config file with no undo.
    DeleteMcpServer { name: String },
    /// POST /settings/hooks/enabled (TS `setHooksEnabled`) — the whole
    /// tree's switch.
    SetHooksEnabled { enabled: bool },
    /// PUT /settings/hooks/{event}/{matcherIndex} + body `hookIndex`
    /// (TS `updateHookEntry`) — edit ONE existing entry; no add, no remove.
    UpdateHookEntry {
        event: String,
        matcher_index: usize,
        hook_index: usize,
        patch: HookEntryPatch,
    },
    /// POST /settings/agents/{name}/enable (TS `setAgentEnabled`).
    SetAgentEnabled { name: String, enable: bool },
    /// PUT /settings/agents/{name} (TS `upsertAgent`): a personal agent's
    /// frontmatter patch; the model/thought-level keys travel only when the
    /// user actually picked (the modelTouched gate, TS PersonalAgentForm).
    UpsertAgent { name: String, body: AgentUpsert },
    /// DELETE /settings/agents/{name} (TS `deleteAgent`). The view confirms
    /// first — deleting an agent loses its prompt body, which is not
    /// recoverable from the app.
    DeleteAgent { name: String },
    /// A built-in agent's model override (PUT on a built-in name). `None`
    /// clears the override, restoring inheritance from the workspace default.
    SetBuiltInAgentModel {
        name: String,
        selection: Option<BuiltInAgentSelection>,
    },
    /// POST /settings/models/test — one minimal request against a provider,
    /// to answer "does this model work" without spending a real turn. The
    /// answer is reported inline on the model row, never as a form error.
    TestModelConnectivity { provider_id: String, model_id: String },
    /// POST /settings/backups/restore (TS `restoreBackup`) — two-gesture
    /// confirmed in the view before it ever reaches here.
    RestoreBackup { file: String, path: String },
    // ---- Workflows page + gate (batch J) ----
    /// Open the workflows page (TS WorkflowsPage mount): the overview read
    /// plus the gate probe while its verdict is unknown. Stays open with a
    /// poll (5s live / 30s idle) until closed.
    OpenWorkflows,
    /// The page's back gesture — close it. Payloads survive (the TS store
    /// keeps them), so a reopen paints instantly and re-reads.
    CloseWorkflows,
    /// The page header's refresh control (the overview read again).
    RefreshWorkflows,
    /// A group card: open one workflow (TS WorkflowDetail) — the detail read
    /// plus the journal history on the group's instance.
    OpenWorkflowDetail {
        scope: WorkflowScope,
        name: String,
        instance_id: String,
    },
    /// The workflow page's back gesture — close it and re-read the overview
    /// (badges follow whatever the run just did).
    CloseWorkflowDetail,
    /// POST …/start (TS `startWorkflow`): on success the page closes and the
    /// returned `acpSessionId` opens as a tab (the TS closeConfig +
    /// openSession pair).
    StartWorkflow {
        scope: WorkflowScope,
        name: String,
        instance_id: String,
        /// The validated args object; `None` = no args.
        args: Option<Value>,
    },
    /// Promote a project workflow to the global scope (TS `promoteViaChat`):
    /// no dedicated REST — the bridge's own chat-prefill doctrine. The
    /// driver stages the prompt into the composer and closes the page.
    WorkflowPromoteGlobal { name: String },
    /// PUT …/meta (TS `saveMeta`): the whole-meta REPLACE with the rest of
    /// the loaded meta merged back in — description required, an empty
    /// whenToUse drops the key (never sent as "").
    UpdateWorkflowMeta {
        scope: WorkflowScope,
        name: String,
        instance_id: String,
        /// The complete next meta object (the form already merged).
        meta: Value,
    },
    /// POST …/move (TS `moveToProject`): global → this project; on success
    /// the page backs out to the overview.
    MoveWorkflowToProject {
        scope: WorkflowScope,
        name: String,
        instance_id: String,
    },
    /// DELETE …/workflows/{scope}/{name} (TS `remove`): the armed two-step
    /// confirm's fire; on success the page backs out to the overview.
    DeleteWorkflow {
        scope: WorkflowScope,
        name: String,
        instance_id: String,
    },
    /// GET …/workflow-create-prompt (TS `createViaChat`): stage the bridge's
    /// prefilled "create via conversation" prompt, never auto-send it.
    WorkflowCreateViaChat { instance_id: String },
    /// POST …/resume (TS `resumeWorkflowRun`): on success open the run's
    /// session and refresh whatever list the row lives in.
    WorkflowResumeRun {
        run_id: String,
        session_id: String,
        name: Option<String>,
        instance_id: String,
    },
    /// POST …/stop (TS `stopWorkflowRun`): the hub route first (it reaches
    /// ANY instance), `session/cancelBackgroundTask` over the ACP socket as
    /// the fallback — `None` (or a failed hub stop) falls through to the
    /// connected tab's socket; 409 = already settled.
    WorkflowStopRun {
        run_id: String,
        session_id: String,
        /// The resolved owner (hub annotation → launch memory → connected
        /// instance); `None` rides the ACP fallback directly.
        instance_id: Option<String>,
    },
    /// POST …/dismiss (TS `dismissWorkflowRun`): hide one settled journal
    /// row; the reloads make it leave every list.
    DismissWorkflowRun {
        instance_id: String,
        run_id: String,
    },
    /// POST …/dismiss-batch (TS `clearFinished`): hide every settled row of
    /// the open workflow's history at once; still-flying ids are skipped
    /// hub-side.
    DismissWorkflowRuns {
        instance_id: String,
        run_ids: Vec<String>,
    },
    /// A run row's Details (TS `runDetail`): the conversation summary plus
    /// the journal's head on that row's address.
    OpenWorkflowRun {
        instance_id: String,
        session_id: String,
        run_id: String,
        name: Option<String>,
    },
    /// The run page's "Load more" — the next journal page past the highest
    /// sequence held (`afterSequence` cursor, `hasMore` gate).
    WorkflowRunLoadMore,
    /// The run page's Retry (TS: the explicit button when the first pull
    /// failed and the run is settled) — the head read again.
    WorkflowRunRetryEvents,
    /// The run page's tab strip (TS `setTab`, batch L): the artifacts and
    /// workspace tabs load lazily on their first open — one fetch per tab.
    WorkflowRunSelectTab { tab: WorkflowRunTab },
    /// A workspace row (TS `loadNode`): expand its result (one GET on the
    /// node's site/ordinal), or collapse it when it is the open one.
    WorkflowRunToggleNode {
        key: String,
        site_id: String,
        ordinal: u64,
    },
    /// An artifact row (TS `setOpenArtifact`): open the viewer and start its
    /// first read (text chunk or data page, by kind).
    WorkflowRunOpenArtifact { artifact_id: String },
    /// The viewer's back.
    WorkflowRunCloseArtifact,
    /// The viewer's "Load more": the next text chunk (`nextOffset`) or the
    /// next data page (`afterSequence`), by kind.
    WorkflowRunArtifactLoadMore,
    /// The run page's back gesture.
    CloseWorkflowRun,
    /// POST …/settings (TS `amendWorkflowRun`): three-state body; a supersede
    /// re-points the open run view to the returned run id.
    AmendWorkflowRun {
        instance_id: String,
        session_id: String,
        run_id: String,
        body: AmendRunSettingsBody,
    },
    /// PUT /settings/workflow-gate (TS `setWorkflowGate`): the machine-level
    /// switch — auto / always-on / disabled, read live by every bridge.
    SetWorkflowGate {
        instance_id: String,
        mode: WorkflowGateModeSetting,
    },
    /// Read the gate verdict off the active tab's bridge (the switch row's
    /// lazy seed — TS `loadWorkflowGate` on config-screen mount).
    ProbeWorkflowGate,
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
    /// One config-section write settled (batch I). `ok` closes the open
    /// MCP/hook/agent form — the same TS `if (ok) onClose()` contract;
    /// switches and restores have no form and simply ignore it.
    ConfigWriteResult { ok: bool },
    /// A queued draft returned to the composer (ChatView `editPending`):
    /// prefill the text and re-stage the images. One-shot — never part of
    /// the snapshot, so it cannot re-fire on a fingerprint change.
    ComposerPrefill(protocol::chat::PromptDraft),
}

/// A built-in agent's model override (server `AgentModelSelection`): the
/// provider/model pair plus the reasoning level, which is part of the override
/// rather than a separate setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltInAgentSelection {
    pub provider_id: String,
    pub model_id: String,
    pub reasoning_level: Option<String>,
}

/// The six ZCode-config section pages (batch I, requirements §7): one
/// shared panel shape — every one is a section read plus writes through the
/// applyConfigWrite path (effect class surfaced, section reloaded). App
/// Update (ADR-0009) rides the same shape: its "write" is the install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigPageKind {
    Skills,
    Mcp,
    Hooks,
    Agents,
    Backups,
    AppUpdate,
}

impl ConfigPageKind {
    pub const ALL: [Self; 6] = [
        Self::Skills,
        Self::Mcp,
        Self::Hooks,
        Self::Agents,
        Self::Backups,
        Self::AppUpdate,
    ];

    fn index(self) -> usize {
        match self {
            Self::Skills => 0,
            Self::Mcp => 1,
            Self::Hooks => 2,
            Self::Agents => 3,
            Self::Backups => 4,
            Self::AppUpdate => 5,
        }
    }

    /// The section's i18n title (the page header).
    pub fn title(self) -> &'static str {
        match self {
            Self::Skills => "zconfig.skills",
            Self::Mcp => "zconfig.mcp",
            Self::Hooks => "zconfig.hooks",
            Self::Agents => "zconfig.agents",
            Self::Backups => "zconfig.backups",
            Self::AppUpdate => "zconfig.appUpdate",
        }
    }
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
    /// Session Activity `finishedAt` (ms) — the TS badge's just-finished
    /// window is a 60s freshness check at paint time.
    pub finished_at: Option<u64>,
    /// The discovery row's REST `status == "running"` — the heartbeat
    /// fallback (ADR-0005), shown only when NO broadcast activity entry
    /// exists for the row.
    pub rest_running: bool,
    /// Whether any tab's Session Activity map holds this session (the TS
    /// `!activity` gate in front of the restRunning fallback).
    pub activity_known: bool,
    /// The tab already holding this session, if any.
    pub open_tab: Option<u64>,
}

/// Tab-strip entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabSummary {
    pub id: u64,
    pub session_id: Option<String>,
    /// The tab's workspace path — the tab strip's project identity: the
    /// leaf name prefixes the session title (the CLI composer-cap shape)
    /// and the hover carries the full path.
    pub workspace: Option<String>,
    /// The session's display name (discovery's title — the rename target,
    /// or whatever the bridge titled it). The tab strip shows this, falling
    /// back to the short id while discovery has not named it yet.
    pub session_title: Option<String>,
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
    /// Slash commands from `available_commands_update` — the composer's "/"
    /// completion menu (ChatView.tsx `commands`).
    pub available_commands: Vec<protocol::acp::SlashCommand>,
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
    /// The row whose tombstone DELETE is in flight (batch G-light).
    pub deleting: Option<String>,
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

/// The side panels' account quota card (TS QuotaSection): the combined
/// multi-provider allowance, shared by the settings drawer and the session
/// panel. The same driver state the plan-quota page reads (TS keeps ONE
/// `usageStats` store field for all three surfaces), so the card repaints
/// whenever a refresh lands from any of them.
#[derive(Debug, Clone, Default)]
pub struct QuotaCardView {
    pub usage: Option<AccountUsageStats>,
    pub quota_unavailable: bool,
    pub usage_stats_at: Option<u64>,
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
    /// The decoded raster for the image kind (batch K): `None` while the
    /// fetch is in flight, after a failure, or for kinds that never load.
    pub image: Option<Arc<Image>>,
    pub loading: bool,
    pub error: Option<String>,
}

/// One open ZCode-config section page (batch I): the section payload stays
/// a raw `Value` — every screen decodes through `protocol::config_decode`
/// at paint time (the module the TS rule says the screens must call, never
/// fork). Rendered only while the page is open.
#[derive(Debug, Clone)]
pub struct ConfigSectionPageView {
    pub kind: ConfigPageKind,
    /// `None` = not probed; `Some(false)` = an older hub — the capability
    /// card, not an error.
    pub supported: Option<bool>,
    pub loading: bool,
    pub error: Option<String>,
    /// GET /settings/{section}, permissive-wire doctrine.
    pub payload: Option<Value>,
    /// A write from this page is in flight (the form's busy / switch
    /// disable).
    pub busy: bool,
    /// Agents only: the model picker's data (the models section payload),
    /// seeded on open because the agents section does not carry it. `None`
    /// for every other kind.
    pub models: Option<ModelsPayload>,
    /// App Update only: the release channel the page's check names (TS
    /// `updateChannel`).
    pub update_channel: String,
    /// App Update only (TS `appUpdateInstall`): the install block the
    /// progress poll adopts; `None` until a poll carries one or an install
    /// POST fails.
    pub install: Option<Value>,
}

/// The Workflows page (batch J): the hub's machine-level overview rendered
/// as-is (groups + active + recent), with the two drill-downs the mobile
/// page owns — one workflow (detail + journal history) and one run (the
/// journal events themselves).
#[derive(Debug, Clone)]
pub struct WorkflowsPageView {
    pub loading: bool,
    pub error: Option<String>,
    pub overview: Option<WorkflowOverviewResponse>,
    /// The open workflow sub-page; `None` = the overview list.
    pub detail: Option<WorkflowDetailView>,
    /// The open run sub-page; `None` = no run open.
    pub run: Option<WorkflowRunView>,
    /// A workflow-level write (start / resume / dismiss) is in flight.
    pub busy: bool,
    /// The active tab's instance — the LAST-RESORT action address (TS
    /// `instanceId`): rows the hub could not place fall back to it.
    pub action_instance: Option<String>,
    /// The launch-memory joins for the run rows this page paints (batch M,
    /// TS `lookupLaunch`): run id → the session/instance the app's own
    /// start minted. Rows without an `acpSessionId` resolve through this
    /// table into actionable rows; a stale entry just fails its open.
    pub launches: HashMap<String, WorkflowLaunchRef>,
    /// The third session ladder's join (TS ActiveRunsBlock `sessionRuns`):
    /// run id → the CURRENT attached session, filled by the 5s poll while
    /// live rows are on screen. Active-zone rows the first two rungs
    /// missed resolve through this map; entries are run-id-exact, so a
    /// stale one can only act on a run that WAS in this session.
    pub session_runs: HashMap<String, String>,
}

/// One launch-memory resolution carried to the view (the two fields the
/// row ladders read off a launch).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowLaunchRef {
    pub instance_id: String,
    pub acp_session_id: String,
}

/// One open workflow sub-page (TS `WorkflowDetail`).
#[derive(Debug, Clone)]
pub struct WorkflowDetailView {
    pub scope: WorkflowScope,
    pub name: String,
    /// The instance whose group opened this page — actions address it.
    pub instance_id: String,
    pub loading: bool,
    pub detail: Option<WorkflowDetailResponse>,
    /// `None` = never landed (loading or failed — the empty state must not
    /// dress a failure up as "no runs recorded yet").
    pub runs: Option<Vec<WorkflowRunRow>>,
    /// The resumable-verdict join: acp session id → run summaries.
    pub summaries: HashMap<String, Vec<ConversationRunSummary>>,
}

/// The run page's three tabs (TS `Tab`): the journal is the default view,
/// artifacts and the workspace only cost a request once opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowRunTab {
    Events,
    Artifacts,
    Workspace,
}

/// One expanded workspace node's result (TS `nodeResult`): `body` is the
/// error when the backend sent one, else the result.
#[derive(Debug, Clone)]
pub struct WorkflowNodeResultView {
    pub key: String,
    pub status: Option<String>,
    pub body: Value,
    pub truncated: bool,
}

/// The open artifact viewer (TS `ArtifactViewer` locals). Text kinds
/// (markdown / file) stream through `text`; the preset kinds (board / table
/// / metrics / chart) through `items`; anything else renders its metadata.
#[derive(Debug, Clone)]
pub struct WorkflowArtifactViewerView {
    pub artifact: WorkflowArtifact,
    pub busy: bool,
    /// The lossy UTF-8 decode of every byte held so far (shared so the
    /// snapshot clone stays cheap); `None` until the first chunk lands, or
    /// after an undecodable chunk.
    pub text: Option<Arc<str>>,
    /// The binary verdict (>1% U+FFFD once the chunks stop, or an
    /// undecodable base64 payload).
    pub binary: bool,
    pub total_bytes: Option<u64>,
    /// `Some` = another chunk follows (the "Load more" gate).
    pub next_offset: Option<u64>,
    /// The preset kinds' paginated inventory; `None` until the first page.
    pub items: Option<Vec<ArtifactDataItem>>,
    pub items_has_more: bool,
}

/// One open run sub-page (TS `WorkflowRunDetail`): the journal events
/// (default tab), the artifacts, and the workspace nodes.
#[derive(Debug, Clone)]
pub struct WorkflowRunView {
    pub instance_id: String,
    pub session_id: String,
    /// Re-pointed on supersede (the amendment returned a NEW run id).
    pub run_id: String,
    pub name: Option<String>,
    pub summary: Option<ConversationRunSummary>,
    /// `None` = the head read has not landed.
    pub events: Option<Vec<WorkflowRunEvent>>,
    pub events_busy: bool,
    pub has_more: bool,
    /// The polls stopped believing the session is reachable — the read-only
    /// degradation (TS `sessionDead`).
    pub session_dead: bool,
    /// The summary says the run is unsettled (or is still unknown).
    pub active: bool,
    pub amend_busy: bool,
    pub tab: WorkflowRunTab,
    /// `None` = never landed (loading or failed — the empty state must not
    /// dress a failure up as "no artifacts produced").
    pub artifacts: Option<Vec<WorkflowArtifact>>,
    pub artifacts_busy: bool,
    pub nodes: Option<Vec<WorkflowWorkspaceNode>>,
    pub nodes_busy: bool,
    /// The one expanded workspace row.
    pub node_result: Option<WorkflowNodeResultView>,
    /// The row key whose result read is in flight.
    pub node_busy: Option<String>,
    /// The open artifact viewer (replaces the tabs while open).
    pub artifact: Option<WorkflowArtifactViewerView>,
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
    /// tabs — the status bar's number.
    pub running_session_count: usize,
    /// Tabs whose prompt this app sent is still in flight — the quit
    /// dialog's own-task number (requirements §4): turns other clients
    /// started keep running untouched and never gate quitting.
    pub own_running_session_count: usize,
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
    /// The plan-quota drawer; `None` = closed. The drawer floats above the
    /// workbench (the only config surface that does NOT replace it — the
    /// rail gauge and the settings entry both open it).
    pub quota_page: Option<QuotaPageView>,
    /// The side panels' quota card (TS QuotaSection): always present — the
    /// settings drawer and the session panel both render it while they are
    /// up.
    pub quota_card: QuotaCardView,
    /// The hub's preformatted quota dock line (ADR-0021) — the top status
    /// bar's rightmost segment. `None` = no quota data (hide, not an error).
    pub quota_dock: Option<String>,
    /// Config-screen notice (TS `notice`): a `notice.*` i18n key or plain
    /// failure text, rendered by every config page's frame.
    pub config_notice: Option<String>,
    /// A needs-restart write has not been applied yet (TS `pendingRestart`)
    /// — drives the Backend Restart affordance on the config pages.
    pub pending_restart: bool,
    /// The last connectivity verdict per `(providerId, modelId)`, painted on
    /// the model row. Not cleared on reload: the verdict describes a request
    /// that already happened, and a section refresh does not make it stale.
    pub model_tests:
        std::collections::HashMap<(String, String), protocol::settings::ConnectivityResult>,
    /// The probe currently in flight, if any — one at a time, so the row's
    /// spinner names a specific target.
    pub model_test_pending: Option<(String, String)>,
    /// Connection Profile book (CONTEXT.md): saved remote hubs plus the
    /// active id; `None` active = the local hub (Hub Bootstrap path).
    pub servers: Vec<protocol::hub::SavedServer>,
    pub active_server_id: Option<String>,
    /// The new-session chooser (TS ProjectCreateDialog); `None` = closed.
    pub new_session: Option<NewSessionView>,
    /// The Session Files browser (batch F). Always present — the dock
    /// panel's visibility is view-local, like every dock panel.
    pub files: FilesPanelView,
    /// The open ZCode-config section page (batch I); `None` = closed (a
    /// full page of its own, like every config section — ADR-0009).
    pub config_section_page: Option<ConfigSectionPageView>,
    /// The Workflows page (batch J); `None` = closed (a full page of its
    /// own, like every config section — ADR-0009).
    pub workflows_page: Option<WorkflowsPageView>,
    /// The workflow-gate verdict (batch J, per-instance probe): drives the
    /// config panel's switch row (position label). `None` = not probed.
    pub workflow_gate: Option<WorkflowGateBlock>,
    /// A workflow-gate write is in flight.
    pub workflow_gate_busy: bool,
    /// The active tab's bridge instance — the write target the gate sheet
    /// addresses. Snapshot-level on purpose: the sheet's entry lives in the
    /// CONFIG panel, which only renders with the workflows page CLOSED, so
    /// the page's own `action_instance` is `None` on every real open (the
    /// TS store writes the active session's instance, page-independent).
    pub active_instance: Option<String>,
    /// The settings drawer's hub-upgrade check (TS SettingsPanel): in flight, and
    /// the last verdict's display text.
    pub hub_upgrade_busy: bool,
    pub hub_upgrade_note: Option<String>,
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
            own_running_session_count: 0,
            toast: None,
            system_status: None,
            models_page: None,
            usage_page: None,
            quota_page: None,
            quota_card: QuotaCardView::default(),
            config_notice: None,
            pending_restart: false,
            model_tests: std::collections::HashMap::new(),
            model_test_pending: None,
            servers: Vec::new(),
            active_server_id: None,
            new_session: None,
            files: FilesPanelView::default(),
            config_section_page: None,
            workflows_page: None,
            workflow_gate: None,
            workflow_gate_busy: false,
            active_instance: None,
            hub_upgrade_busy: false,
            hub_upgrade_note: None,
            quota_dock: None,
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
    /// One model-connectivity probe settled (POST /settings/models/test). The
    /// answer is painted on the row the probe was started from, so the
    /// provider/model pair travels back with it.
    ModelTestSettled {
        provider_id: String,
        model_id: String,
        result: Result<protocol::settings::ConnectivityResult, SettingsApiError>,
    },    ConnectResult { tab: u64, result: Result<AcpInitializeResult, String> },
    /// A dead tab's re-incubation settled (POST /api/instances, the resume
    /// form): `Ok` carries the (new or reused) instance the tab re-points at
    /// and reconnects to; `Err` parks the tab in Failed — its backend could
    /// not be brought back.
    TabReincubated {
        tab: u64,
        result: Result<HubCreateInstanceResult, HubApiError>,
    },
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
    // ---- session row actions (batch G-light, hub REST, no tab) ----
    SessionRenamed {
        instance_id: String,
        session_id: String,
        title: String,
        result: Result<(), HubApiError>,
    },
    SessionClosed {
        instance_id: String,
        session_id: String,
        result: Result<(), HubApiError>,
    },
    /// The bridge's `$/zcode/session_closed` broadcast on `tab`'s socket:
    /// another client retired `session_id` through the hub. Tagged with the
    /// tab because every tab dialing that bridge receives its own copy.
    SessionClosedRemotely {
        tab: u64,
        session_id: String,
    },
    HistorySessionDeleted {
        session_id: String,
        result: Result<(), HubApiError>,
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
    /// GET /api/quota settled (TS `refreshUsageStats`): `Ok(None)` = a
    /// 2xx body that did not fit the combined shape.
    QuotaUsage {
        result: Result<Option<AccountUsageStats>, SettingsApiError>,
    },
    /// GET /api/quota/dock settled (ADR-0021): the hub's preformatted
    /// one-line dock string; `Ok(None)` = no quota data (hide, not an error).
    /// Carries the lifecycle generation it was minted under — a hub switch
    /// drops results from the OLD hub mid-flight (the TS `profile` guard).
    QuotaDock {
        gen: u64,
        result: Result<Option<String>, HubApiError>,
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
    /// `/fs/file` (token-in-query, binary body) settled for the open image
    /// viewer's whole-file fetch (batch K).
    FilesImage {
        seq: u64,
        result: Result<Vec<u8>, HubApiError>,
    },
    // ---- ZCode config sections (batch I) ----
    /// The capability probe settled for one config-section page (the verdict
    /// half of the TS mount pair).
    ConfigSectionProbe {
        page: ConfigPageKind,
        result: Result<(), SettingsApiError>,
    },
    /// One section read settled (TS `loadConfigSection(section)`).
    ConfigSectionLoaded {
        page: ConfigPageKind,
        result: Result<Value, SettingsApiError>,
    },
    /// One config-section write settled (the applyConfigWrite outcome):
    /// `label` names the failure, `reload` the sections the write's success
    /// must re-read.
    ConfigWriteSettled {
        page: ConfigPageKind,
        label: &'static str,
        reload: &'static [ConfigPageKind],
        result: Result<WriteEffect, SettingsApiError>,
    },
    /// GET /api/instances/{id}/settings/pending-restart settled — a
    /// cross-client needs-restart the driver never saw (another client's
    /// write on the active bridge). Only ever ARMS the flag; the restart
    /// flow and a hub switch own clearing it.
    PendingRestartChecked(Result<PendingRestartStatus, SettingsApiError>),
    /// POST /api/upgrade settled — the hub's own staleness verdict.
    HubUpgraded(Result<HubUpgradeResult, SettingsApiError>),
    /// POST …/app-update/install settled: the POST's body is discarded (TS
    /// does the same), so this only signals completion vs transport failure.
    AppUpdateInstalled(Result<(), SettingsApiError>),
    /// The install progress poll settled (TS `pollAppUpdate`): the check
    /// GET's answer, whose `appUpdate.install` block is the only install
    /// state there is.
    AppUpdatePolled(Result<Value, SettingsApiError>),
    // ---- Workflows page + gate (batch J) ----
    /// The overview read settled (the page's poll and manual refresh).
    WorkflowOverview(Result<WorkflowOverviewResponse, SettingsApiError>),
    /// The gate probe settled (`/settings/all`'s workflow block, per the
    /// asking instance). `Ok(None)` = an older bridge without the block.
    /// Carries the instance so a probe from before an instance switch can't
    /// answer for the current one.
    WorkflowGateProbed {
        instance_id: String,
        result: Result<Option<WorkflowGateBlock>, SettingsApiError>,
    },
    /// PUT /settings/workflow-gate settled.
    WorkflowGateSet(Result<WorkflowGateSetResponse, SettingsApiError>),
    /// GET …/workflows/{scope}/{name} settled for the open detail page.
    WorkflowDetailLoaded {
        seq: u64,
        result: Result<WorkflowDetailResponse, SettingsApiError>,
    },
    /// GET …/workflows/runs settled for the open detail page.
    WorkflowRunsLoaded {
        seq: u64,
        result: Result<WorkflowRunsHistoryResponse, SettingsApiError>,
    },
    /// The detail page's resumable-verdict join settled: per session, the
    /// conversation run summaries (TS `loadRunSummaries`) — silent by
    /// design, so dead sessions simply leave their rows without a resume
    /// button.
    WorkflowDetailSummaries {
        seq: u64,
        result: Result<Vec<(String, Vec<ConversationRunSummary>)>, SettingsApiError>,
    },
    /// POST …/start settled.
    WorkflowStarted {
        scope: WorkflowScope,
        name: String,
        instance_id: String,
        result: Result<WorkflowStartResponse, SettingsApiError>,
    },
    /// PUT …/workflows/{scope}/{name}/meta settled for the open detail page.
    WorkflowMetaUpdated {
        seq: u64,
        result: Result<Value, SettingsApiError>,
    },
    /// POST …/workflows/{scope}/{name}/move settled.
    WorkflowMoved {
        seq: u64,
        result: Result<Value, SettingsApiError>,
    },
    /// DELETE …/workflows/{scope}/{name} settled.
    WorkflowDeleted {
        seq: u64,
        result: Result<Value, SettingsApiError>,
    },
    /// GET …/workflow-create-prompt settled.
    WorkflowCreatePrompt(Result<WorkflowCreatePromptResponse, SettingsApiError>),
    /// POST …/resume settled.
    WorkflowRunResumed {
        session_id: String,
        instance_id: String,
        result: Result<Value, SettingsApiError>,
    },
    /// POST …/stop settled.
    WorkflowRunStopped(Result<Value, SettingsApiError>),
    /// POST …/dismiss settled.
    WorkflowRunDismissed {
        run_id: String,
        result: Result<DismissRunResponse, SettingsApiError>,
    },
    /// POST …/dismiss-batch settled (the detail page's clear-finished).
    WorkflowRunsDismissed(Result<DismissRunsResponse, SettingsApiError>),
    /// The third ladder's conversationRuns read settled (the pair is the
    /// epoch — an answer from a previous instance/session drops).
    WorkflowSessionRuns {
        pair: (String, String),
        result: Result<ConversationRunsResponse, SettingsApiError>,
    },
    /// The run page's conversation-runs read settled.
    WorkflowRunSummary {
        seq: u64,
        result: Result<ConversationRunsResponse, SettingsApiError>,
    },
    /// The run page's journal read settled (`initial` = the head, else the
    /// `afterSequence` append page).
    WorkflowRunEvents {
        seq: u64,
        initial: bool,
        result: Result<WorkflowRunEventsResponse, SettingsApiError>,
    },
    /// POST …/settings (the amendment) settled.
    WorkflowRunAmended {
        seq: u64,
        result: Result<AmendRunSettingsResponse, SettingsApiError>,
    },
    /// GET …/artifacts settled for the open run (the artifacts tab's lazy
    /// read, batch L).
    WorkflowRunArtifacts {
        seq: u64,
        result: Result<RunArtifactsResponse, SettingsApiError>,
    },
    /// GET …/workspace settled for the open run (the workspace tab's lazy
    /// read).
    WorkflowRunWorkspace {
        seq: u64,
        result: Result<WorkflowWorkspaceResponse, SettingsApiError>,
    },
    /// GET …/nodes/{site}/{ordinal} settled: one expanded workspace row.
    WorkflowRunNodeResult {
        seq: u64,
        key: String,
        result: Result<WorkflowNodeResultResponse, SettingsApiError>,
    },
    /// GET …/artifacts/{id}/read settled: one text chunk of the open
    /// viewer (`seq` is the VIEWER's — a late chunk for a closed or
    /// re-opened viewer must not land).
    WorkflowRunArtifactChunk {
        seq: u64,
        append: bool,
        result: Result<WorkflowArtifactReadResponse, SettingsApiError>,
    },
    /// GET …/artifacts/{id}/data settled: one page of the open viewer's
    /// inventory (same viewer-seq contract).
    WorkflowRunArtifactItems {
        seq: u64,
        initial: bool,
        result: Result<WorkflowArtifactItemsResponse, SettingsApiError>,
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

    fn on_session_closed(&self, session_id: &str) {
        let _ = self.tx.send(LoopEvent::SessionClosedRemotely {
            tab: self.tab,
            session_id: session_id.to_string(),
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

    /// An inert handle with no driver thread (window-layer tests): `send`
    /// still works but nothing consumes it, and `recorded_commands`
    /// (tests only) still shows every `send`.
    #[cfg(test)]
    pub fn inert() -> Self {
        let (cmd_tx, _rx) = unbounded_channel();
        Self {
            cmd_tx,
            #[cfg(test)]
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
    /// Epoch-ms of the last auto re-incubation (dead-backend revival);
    /// another within TAB_REINCUBATE_COOLDOWN_MS is refused.
    reincubated_at: Option<u64>,
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
    /// The instance that served the FIRST page (`page.instance.id`) — the
    /// delete target for every row of the view (the TS dialog captures the
    /// same id; load-more pages never re-capture it).
    instance_id: String,
    rows: Vec<HubHistorySession>,
    next_cursor: Option<HubHistoryCursor>,
    loading_page: bool,
    loading_more: bool,
    error: Option<String>,
    resuming: Option<String>,
    /// The row whose tombstone DELETE is in flight (TS dialog `deleting`).
    deleting: Option<String>,
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
                image: v.image.clone(),
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
    /// Decoded raster for the image kind (batch K).
    image: Option<Arc<Image>>,
    loading: bool,
    error: Option<String>,
}

/// One ZCode-config section page's driver state (batch I — the TS configSlice
/// fields the five screens read: `config{Section}`, `configSupported`,
/// `configLoading`, `configError` + the write busy). The payload survives a
/// page close (the TS store keeps it) and re-reads on the next open; a full
/// re-bootstrap resets it alongside the connection (the TS
/// `connectionResetPatch`: configuration state describes ONE machine).
#[derive(Default)]
struct ConfigSectionPanel {
    open: bool,
    /// `None` = not probed; `Some(false)` = an older hub (bridge < 0.47.0).
    supported: Option<bool>,
    loading: bool,
    error: Option<String>,
    payload: Option<Value>,
    /// A write from this page is in flight.
    busy: bool,
    /// App Update only (TS `appUpdateInstall`): the install POST's outcome
    /// block. Wins over the payload's in-flight `install` block while set;
    /// cleared when a new install starts so the poll's progress shows.
    install: Option<Value>,
}

impl ConfigSectionPanel {
    /// The connection-reset patch — same doctrine as
    /// [`ModelsPanel::reset_data`]. `open` survives; the steady-state entry
    /// re-seeds the reads.
    fn reset_data(&mut self) {
        self.supported = None;
        self.loading = false;
        self.error = None;
        self.payload = None;
        self.busy = false;
        self.install = None;
    }
}

/// The Workflows page's driver state (batch J — the TS workflowSlice fields
/// the page reads). Payloads survive a page close and re-read on the next
/// open; a full re-bootstrap resets them alongside the connection.
#[derive(Default)]
struct WorkflowsPanel {
    open: bool,
    loading: bool,
    error: Option<String>,
    overview: Option<WorkflowOverviewResponse>,
    /// The gate verdict (per-instance probe; also updated by a successful
    /// gate write). Lives OUTSIDE `open` — the config panel's switch row
    /// reads it while the page is closed.
    gate: Option<WorkflowGateBlock>,
    gate_busy: bool,
    /// The instance the last gate probe addressed (`None` = nothing probed
    /// yet — the first live instance triggers one). While the verdict is
    /// unknown, a CHANGE of the active instance re-probes (TS
    /// ZCodeConfigScreen's `instanceId` effect dep); a probe that failed for
    /// THIS instance waits for the next config-dock / page open instead of
    /// being hammered on a timer.
    gate_probe_instance: Option<String>,
    /// Epoch-ms when the overview poll fires next while open.
    next_poll_at: u64,
    /// The third session-resolution ladder (TS ActiveRunsBlock's
    /// `sessionRuns`): run id → the CURRENT attached session, filled by the
    /// 5s conversationRuns poll. Only consulted for ACTIVE-zone rows the
    /// first two rungs (launch memory, the bridge-joined alias) missed.
    session_runs: HashMap<String, String>,
    /// The (instance, session) pair the map and its in-flight poll are
    /// scoped to — TS's epoch ref: an answer from a previous instance or
    /// session must not land, and a pair switch drops the map first.
    session_runs_pair: Option<(String, String)>,
    /// Epoch-ms when the sessionRuns poll fires next (armed only while the
    /// page is open, live rows are on screen, and a session is attached).
    next_session_runs_poll_at: u64,
    /// The open workflow sub-page; `seq` invalidates in-flight reads on
    /// close/reopen (a late answer must not land in a NEW page's state).
    detail: Option<WorkflowDetailPanel>,
    /// The open run sub-page; same seq contract (a supersede re-point bumps
    /// it too — the old run's journal cursor must not append into the new).
    run: Option<WorkflowRunPanel>,
    /// A workflow-level write (start / resume / dismiss) is in flight.
    busy: bool,
    /// Monotone seq for the detail/run sub-pages' in-flight reads.
    workflow_seq: u64,
}

/// One workflow sub-page's driver state (TS `WorkflowDetail` locals).
struct WorkflowDetailPanel {
    seq: u64,
    scope: WorkflowScope,
    name: String,
    instance_id: String,
    loading: bool,
    detail: Option<WorkflowDetailResponse>,
    runs: Option<Vec<WorkflowRunRow>>,
    /// The resumable-verdict join: acp session id → that session's run
    /// summaries (the resume button's source of truth).
    summaries: HashMap<String, Vec<ConversationRunSummary>>,
    busy: bool,
    /// Epoch-ms when the 5s history poll fires next (armed only while this
    /// workflow's newest run flies — TS WorkflowDetail's `runActive` tick).
    next_runs_poll_at: u64,
}

/// One run sub-page's driver state (TS `WorkflowRunDetail` locals). The 3s
/// poll appends via the `afterSequence` cursor; `fail` counts consecutive
/// failed summary reads (two retire the poll → `session_dead`, the
/// read-only degradation). The artifacts/workspace tabs hold their lazy
/// reads; the open artifact viewer and the one expanded node ride along —
/// a supersede re-point resets all of it (the TS `key={runId}` remount).
#[derive(Default)]
struct WorkflowRunPanel {
    seq: u64,
    instance_id: String,
    session_id: String,
    run_id: String,
    name: Option<String>,
    summary: Option<ConversationRunSummary>,
    events: Option<Vec<WorkflowRunEvent>>,
    events_busy: bool,
    has_more: bool,
    next_poll_at: u64,
    fail: u32,
    session_dead: bool,
    amend_busy: bool,
    tab: WorkflowRunTab,
    artifacts: Option<Vec<WorkflowArtifact>>,
    artifacts_busy: bool,
    nodes: Option<Vec<WorkflowWorkspaceNode>>,
    nodes_busy: bool,
    node_result: Option<WorkflowNodeResultView>,
    node_busy: Option<String>,
    artifact: Option<WorkflowArtifactViewer>,
}

impl Default for WorkflowRunTab {
    fn default() -> Self {
        WorkflowRunTab::Events
    }
}

/// The open artifact viewer's driver state (TS `ArtifactViewer` locals).
/// `seq` is the viewer's own read guard (bumped per open). Text kinds keep
/// the RAW bytes across chunks and re-decode the whole buffer (the TS
/// streaming TextDecoder's guarantee: a chunk boundary splitting a
/// multi-byte character never becomes U+FFFD).
struct WorkflowArtifactViewer {
    seq: u64,
    artifact: WorkflowArtifact,
    busy: bool,
    bytes: Vec<u8>,
    text: Option<Arc<str>>,
    binary: bool,
    total_bytes: Option<u64>,
    next_offset: Option<u64>,
    items: Option<Vec<ArtifactDataItem>>,
    items_has_more: bool,
    /// The last item sequence received (the `afterSequence` cursor).
    items_cursor: Option<u64>,
}

impl WorkflowArtifactViewer {
    fn view(&self) -> WorkflowArtifactViewerView {
        WorkflowArtifactViewerView {
            artifact: self.artifact.clone(),
            busy: self.busy,
            text: self.text.clone(),
            binary: self.binary,
            total_bytes: self.total_bytes,
            next_offset: self.next_offset,
            items: self.items.clone(),
            items_has_more: self.items_has_more,
        }
    }
}

impl WorkflowRunPanel {
    /// The supersede re-point / fresh-open reset of everything the TS
    /// `key={runId}` remount drops: tabs, lazy payloads, the viewer.
    fn reset_tabs(&mut self) {
        self.tab = WorkflowRunTab::Events;
        self.artifacts = None;
        self.artifacts_busy = false;
        self.nodes = None;
        self.nodes_busy = false;
        self.node_result = None;
        self.node_busy = None;
        self.artifact = None;
    }
}

impl WorkflowsPanel {
    /// The connection-reset patch (the TS `connectionResetPatch`: the
    /// workflow payloads describe ONE machine). `open` survives.
    fn reset_data(&mut self) {
        self.loading = false;
        self.error = None;
        self.overview = None;
        self.gate = None;
        self.gate_busy = false;
        self.gate_probe_instance = None;
        self.next_poll_at = 0;
        self.session_runs.clear();
        self.session_runs_pair = None;
        self.next_session_runs_poll_at = 0;
        self.detail = None;
        self.run = None;
        self.busy = false;
        self.workflow_seq += 1;
    }
}

/// Journal pages drained per run-events call (TS `DRAIN_PAGES`).
const RUN_EVENT_DRAIN_PAGES: usize = 8;

/// The third ladder's map fold (TS `new Map(conv.runs.map((r) => [r.runId,
/// sid]))`): every run the CURRENT session reports, keyed by run id. A
/// REPLACE, not a merge — a run that left the session must leave the map
/// too, or its stale entry could pin a phantom session onto a later row.
fn fold_session_runs(
    runs: &[ConversationRunSummary],
    session_id: &str,
) -> HashMap<String, String> {
    runs.iter()
        .map(|r| (r.run_id.clone(), session_id.to_string()))
        .collect()
}

/// Highest journal sequence held — the safe `afterSequence` cursor. NOT the
/// last element's sequence: a clipped journal page can end on an event
/// without one, and a degenerate cursor makes the server replay the whole
/// journal (TS `maxSequence`).
fn max_run_sequence(events: &[WorkflowRunEvent]) -> u64 {
    events.iter().map(|e| e.sequence).max().unwrap_or(0)
}

/// The journal append (TS `fetchEvents`'s non-initial branch): an append
/// page is sequence-filtered REGARDLESS — a cursor the server could not
/// honor answers with the FULL journal, which must never re-append onto the
/// list. Keeps only incoming sequences past the highest one held.
fn merge_run_events(
    held: Vec<WorkflowRunEvent>,
    incoming: Vec<WorkflowRunEvent>,
) -> Vec<WorkflowRunEvent> {
    let floor = max_run_sequence(&held);
    let mut merged = held;
    merged.extend(incoming.into_iter().filter(|e| e.sequence > floor));
    merged
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
    /// The persisted pending-prompt queues (batch H1): session id → drafts,
    /// restored into a tab's core when it attaches that session and folded
    /// back on every queue change (the TS localStorage map's port —
    /// `pending_prompts` in `SessionCore` stays the in-memory truth).
    pending: PendingStore,
    /// The persisted per-session turn readouts (batch T): the first-open
    /// seed for ctx/cache/tps — the in-memory snapshot LRU dies with the
    /// process, this map does not.
    readouts: ReadoutStore,
    /// The persisted workflow-launch memory (batch M): run id → the
    /// session/instance `POST …/start` minted — the TS
    /// `zcode.workflowLaunches` table's port. The driver is the single
    /// writer; the view reads the per-row joins off the snapshot.
    launches: WorkflowLaunchStore,

    instances: Vec<HubInstance>,
    /// Discovery failure note (sidebar hint) — the hub REST call failing.
    discovery_note: Option<String>,
    next_discovery_at: u64,
    discovery_probe: bool,
    /// Sessions THIS client closed, with the close time. A discovery refresh
    /// must not resurrect them inside the window: the hub serves the last
    /// heartbeat's list (up to ~10s stale, and a bridge that re-advertises a
    /// retired conversation — a turn settling after the force-close — keeps
    /// it for good). Bounded so a session genuinely reused elsewhere
    /// reappears once the window lapses (ADR-0006's re-arm).
    closed_sessions: HashMap<(String, String), u64>,

    /// Session ids discovery has EVER advertised (grows monotonically) —
    /// the remote-open watch's memory. An id appearing in a LATER snapshot
    /// that this set has never carried is a mid-run remote open (a phone
    /// create, a remote resume) and pops an attaching tab; an id seen once
    /// never pops again, so a tombstone-lapsed re-advertisement (a stubborn
    /// post-close row) is a reappearance, not an opening.
    seen_sessions: HashSet<String>,
    /// Discovery's cold-start baseline has landed (one successful
    /// snapshot): tabs may pop for mid-run arrivals only — the boot
    /// snapshot registers every already-advertised session without opening
    /// anything (the 2026-10-22 quiet-cold-start half).
    discovery_baselined: bool,

    tabs: Vec<Tab>,
    active_tab: Option<u64>,
    /// Tabs whose connection WE closed (tab close / Retry) — their Closed
    /// events are expected, not reconnect triggers.
    closing: HashSet<u64>,
    next_tab_id: u64,

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
    /// The hub's preformatted quota dock line (ADR-0021) — the status bar's
    /// rightmost segment. Sticky last-known on failure (the TUI's refresher
    /// keeps it the same way); only a successful `None` clears it.
    quota_dock: Option<String>,
    /// A dock read is in flight — dedupes the discovery-tick + turn-end
    /// triggers piling onto the hub (which caches 15s anyway).
    quota_dock_loading: bool,
    /// Lifecycle generation (the `history.seq` pattern): bumped on every hub
    /// switch so a dock read minted for the OLD hub drops on arrival instead
    /// of painting the wrong account's line over the new hub's.
    quota_dock_gen: u64,
    /// The Usage config page (see [`UsagePanel`]).
    usage: UsagePanel,
    /// The six ZCode-config section pages (batch I, see
    /// [`ConfigSectionPanel`]), indexed by [`ConfigPageKind::index`]).
    sections: [ConfigSectionPanel; 6],
    /// The App Update page's release channel (TS `updateChannel`): the
    /// persisted pick (ui-prefs), stable by default — the check names it.
    update_channel: String,
    /// POST /api/upgrade in flight (the settings drawer's check button).
    hub_upgrade_busy: bool,
    /// The last hub-upgrade verdict as display text (TS `upgradeNote`);
    /// cleared by the next check or by its 5s timer (the TS effect).
    hub_upgrade_note: Option<String>,
    /// Epoch-ms when the hub-upgrade note auto-clears.
    hub_upgrade_note_until: u64,
    /// Epoch-ms when the App Update progress poll fires next while an
    /// install runs (TS AppUpdatePage's 2s `pollAppUpdate` interval).
    next_app_update_poll_at: u64,
    /// The Workflows page + gate (batch J, see [`WorkflowsPanel`]).
    workflows: WorkflowsPanel,
    /// Config-screen notice (TS `notice`): `notice.*` i18n key or plain text.
    config_notice: Option<String>,
    /// A needs-restart write is pending on the connected bridge — drives the
    /// Backend Restart affordance (armed by config writes; M3 pages).
    pending_restart: bool,
    /// The last connectivity verdict per `(providerId, modelId)`.
    model_tests: std::collections::HashMap<(String, String), protocol::settings::ConnectivityResult>,
    /// The probe in flight, if any — one at a time.
    model_test_pending: Option<(String, String)>,

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
            pending: PendingStore::load(&pending_path()),
            readouts: ReadoutStore::load(&readouts_path()),
            launches: WorkflowLaunchStore::load(&launches_path()),
            instances: Vec::new(),
            discovery_note: None,
            next_discovery_at: 0,
            discovery_probe: true,
            closed_sessions: HashMap::new(),
            seen_sessions: HashSet::new(),
            discovery_baselined: false,
            tabs: Vec::new(),
            active_tab: None,
            closing: HashSet::new(),
            next_tab_id: 1,
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
            quota_dock: None,
            quota_dock_loading: false,
            quota_dock_gen: 0,
            usage: UsagePanel::new(),
            sections: Default::default(),
            update_channel: crate::ui_prefs::load_update_channel()
                .unwrap_or(crate::ui_prefs::CHANNEL_STABLE)
                .to_string(),
            hub_upgrade_busy: false,
            hub_upgrade_note: None,
            hub_upgrade_note_until: 0,
            next_app_update_poll_at: 0,
            workflows: WorkflowsPanel::default(),
            config_notice: None,
            pending_restart: false,
            model_tests: std::collections::HashMap::new(),
            model_test_pending: None,
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
        // One tab per session: a second open of the same target — a
        // discovery row, a history resume, a workflow's "open session" —
        // activates the tab that already holds it instead of stacking a
        // duplicate. An attaching tab counts (its target IS the session);
        // the instance is deliberately NOT part of the key, so a session
        // that moved bridges keeps one tab too.
        if let Some(sid) = target.as_deref() {
            if let Some(existing) = self.tabs.iter().find(|t| {
                t.target.as_deref() == Some(sid)
                    || t.core.active_session_id.as_deref() == Some(sid)
            }) {
                traceln!(
                    "[driver] session {sid} already has tab {}; activating",
                    existing.id
                );
                self.active_tab = Some(existing.id);
                return existing.id;
            }
        }
        let id = self.next_tab_id;
        self.next_tab_id += 1;
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
            reincubated_at: None,
            fs_capable: false,
        });
        self.active_tab = Some(id);
        traceln!("[driver] tab {id} opened (instance {})", self.tabs.last().unwrap().instance_id);
        self.spawn_connect(id);
        id
    }

    fn close_tab(&mut self, id: u64) {
        // Mark + close without holding the tab borrow across the mutations.
        // A closed TAB is not a remote open: its session ids go into the
        // seen set so the remote-open watch never pops a tab back for a
        // conversation whose id has not reached discovery yet (our own
        // new/resumed session inside the heartbeat + poll lag, up to ~25s).
        let (conn, held): (Option<_>, Vec<String>) = match self.tab(id) {
            Some(t) => (
                t.conn.clone(),
                [t.target.clone(), t.core.active_session_id.clone()]
                    .into_iter()
                    .flatten()
                    .collect(),
            ),
            None => (None, Vec::new()),
        };
        for sid in held {
            self.seen_sessions.insert(sid);
        }
        if let Some(conn) = conn {
            self.closing.insert(id);
            conn.close();
        }
        self.tabs.retain(|t| t.id != id);
        if self.active_tab == Some(id) {
            self.active_tab = self.tabs.last().map(|t| t.id);
        }
    }

    /// A batch close's anchor tab takes over focus when the tab that WAS
    /// active got closed along the way — `close_tab`'s own fallback picks
    /// the last survivor, which a right-close can make the wrong pick.
    fn refocus_batch_close(&mut self, anchor: u64, was_active: Option<u64>) {
        let active_gone =
            was_active.is_some_and(|a| !self.tabs.iter().any(|t| t.id == a));
        if active_gone && self.tabs.iter().any(|t| t.id == anchor) {
            self.active_tab = Some(anchor);
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

    /// Re-incubate a dead tab's backend (POST /api/instances, the resume
    /// form): the desktop-stamped call reuses the workspace's live serve
    /// bridge or headless-spawns one (server ADR-0010 amendment), and
    /// [`LoopEvent::TabReincubated`] re-points the tab at the answer.
    fn spawn_reincubate(&mut self, tab: u64, workspace_path: String, session_id: Option<String>) {
        let hub = self.hub_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            // No client-side deadline: incubation is slow by design (the
            // resume flow's contract).
            let result = hub
                .create_instance(&workspace_path, session_id.as_deref())
                .await;
            let _ = tx.send(LoopEvent::TabReincubated { tab, result });
        });
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

    /// The shared retirement of one conversation — the hub answered our own
    /// close, or the bridge broadcast `$/zcode/session_closed` (another
    /// client retired it). Tombstone the row against discovery, drop it from
    /// the instance list, close every tab holding it, and drop a serve
    /// bridge whose LAST advertised session just closed (self-exit,
    /// ADR-0006/0016). Without the tombstone the row vanished and
    /// RESURRECTED on the next 15s poll, then lingered (2026-10-22); without
    /// the tab close the reconnect ladder would try to resume a
    /// deliberately-closed session (2026-10-23).
    fn retire_closed_session(&mut self, instance_id: &str, session_id: &str, now: u64) {
        self.closed_sessions
            .insert((instance_id.to_string(), session_id.to_string()), now);
        for inst in &mut self.instances {
            if inst.id == instance_id {
                inst.sessions.retain(|s| s.session_id != session_id);
            }
        }
        let holders: Vec<u64> = self
            .tabs
            .iter()
            .filter(|t| {
                t.core.active_session_id.as_deref() == Some(session_id)
                    || t.target.as_deref() == Some(session_id)
            })
            .map(|t| t.id)
            .collect();
        for id in holders {
            self.close_tab(id);
        }
        let last_session_gone = self.instances.iter().any(|i| {
            i.id == instance_id && i.origin == InstanceOrigin::Serve && i.sessions.is_empty()
        });
        if last_session_gone {
            self.instances.retain(|i| i.id != instance_id);
        }
        self.toast.notify(
            now,
            // The TS store sets the notice KEY (appStore.ts:2011) and the UI
            // localizes; resolving here covers the desktop's raw-string
            // toast/error slots equally.
            &rust_i18n::t!("notice.sessionClosed"),
        );
    }

    /// Hide tombstoned closes from an incoming discovery list (see
    /// [`Self::closed_sessions`]). Rows the hub no longer carries stay gone;
    /// a tombstone's window lapsing lets the row back in.
    fn filter_closed_sessions(&self, mut list: Vec<HubInstance>, now: u64) -> Vec<HubInstance> {
        if self.closed_sessions.is_empty() {
            return list;
        }
        for inst in &mut list {
            if !self.closed_sessions.keys().any(|(i, _)| i == &inst.id) {
                continue;
            }
            inst.sessions.retain(|s| {
                !self
                    .closed_sessions
                    .get(&(inst.id.clone(), s.session_id.clone()))
                    .is_some_and(|at| now.saturating_sub(*at) < CLOSE_TOMBSTONE_MS)
            });
        }
        list
    }

    fn on_discovery(&mut self, result: Result<Vec<HubInstance>, HubApiError>, now: u64) {
        match result {
            Ok(list) => {
                traceln!("[driver] discovery: {} instance(s)", list.len());
                // Expired close-tombstones go first, then hide the rest from
                // the incoming list — the hub's answer is the last heartbeat's
                // snapshot and may still carry rows this client retired.
                self.closed_sessions
                    .retain(|_, at| now.saturating_sub(*at) < CLOSE_TOMBSTONE_MS);
                let list = self.filter_closed_sessions(list, now);
                // Remote-open watch (2026-10-24, amending the 2026-10-22
                // quiet rule): a session that first appears in a LATER
                // snapshot — a phone create, a remote resume, any client's
                // new conversation — pops an ATTACHING tab. The attached
                // client is what keeps the hosting serve bridge alive: its
                // idle self-exit only counts idle time with zero clients
                // attached, so without a tab a remotely-opened session died
                // ~10min after the remote client walked away. Cold start
                // stays quiet (the 2026-10-22 half): the first snapshot is
                // the baseline. Sessions a tab already holds (our own
                // attach/resume in flight) are not remote opens.
                let arrivals: Vec<(String, String, String)> = if self.discovery_baselined {
                    list.iter()
                        .flat_map(|inst| inst.sessions.iter().map(move |s| (inst, s)))
                        .filter(|(_, s)| !self.seen_sessions.contains(&s.session_id))
                        .filter(|(_, s)| {
                            !self.tabs.iter().any(|t| {
                                t.target.as_deref() == Some(s.session_id.as_str())
                                    || t.core.active_session_id.as_deref()
                                        == Some(s.session_id.as_str())
                            })
                        })
                        .map(|(inst, s)| {
                            (
                                inst.id.clone(),
                                s.session_id.clone(),
                                inst.workspace
                                    .clone()
                                    .filter(|w| !w.is_empty())
                                    .unwrap_or_else(|| "/".to_string()),
                            )
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                for inst in &list {
                    for s in &inst.sessions {
                        self.seen_sessions.insert(s.session_id.clone());
                    }
                }
                self.discovery_baselined = true;
                self.instances = list;
                for (instance_id, sid, cwd) in arrivals {
                    traceln!(
                        "[driver] remote-opened session {sid} — attaching a tab (holds the serve bridge open)"
                    );
                    self.open_tab(instance_id, Some(sid), cwd);
                }
                self.discovery_note = None;
                self.next_discovery_at = now + DISCOVERY_REFRESH_MS;
                self.discovery_probe = false;
                if self.phase == Phase::Discovering || self.phase == Phase::Reconnecting {
                    self.phase = Phase::Connected;
                }
                // The hub just answered — seed the status bar's quota dock
                // line. Refires on later ticks only while nothing has landed
                // (a sticky line needs no poll; turn ends refresh it).
                if self.quota_dock.is_none() {
                    self.spawn_quota_dock();
                }
                // Tabs otherwise come only from explicit gestures (+,
                // sidebar row, history) — the 2026-10-22 rule minus the
                // remote-open exception above. The landing note still covers
                // the truly empty hub: no tab open and no session row
                // anywhere to click.
                if self.tabs.is_empty() && !self.instances.iter().any(|i| !i.sessions.is_empty()) {
                    self.discovery_note = Some(
                        "no open session on the hub yet — start one from the editor, the \
                          + button, or pick one from history below"
                            .to_string(),
                    );
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

    // ---- session row actions (batch G-light) ----

    /// appStore `renameSession`: the hub's HTTP rename. Default 10s deadline
    /// like every TS hub call.
    fn spawn_session_rename(&mut self, instance_id: String, session_id: String, title: String) {
        let hub = self.hub_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub.rename_session(&instance_id, &session_id, &title).await;
            let _ = tx.send(LoopEvent::SessionRenamed {
                instance_id,
                session_id,
                title,
                result,
            });
        });
    }

    /// appStore `closeRemoteSession`: retire one conversation from discovery.
    /// The desktop's End session is an explicit terminate gesture, so the
    /// close is forced — a running turn is cancelled (stop pair + bounded
    /// settle) instead of refusing with 409 (ADR-0006 amendment; the TS
    /// client's plain close keeps the refuse-when-running contract).
    fn spawn_session_close(&mut self, instance_id: String, session_id: String) {
        let hub = self.hub_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub.close_session(&instance_id, &session_id, true).await;
            let _ = tx.send(LoopEvent::SessionClosed {
                instance_id,
                session_id,
                result,
            });
        });
    }

    /// The history row's tombstone delete, addressed to the instance that
    /// served the listing (TS `removeRow` → `deleteSession`).
    fn spawn_history_delete(&mut self, instance_id: String, session_id: String) {
        let hub = self.hub_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub.delete_session(&instance_id, &session_id).await;
            let _ = tx.send(LoopEvent::HistorySessionDeleted { session_id, result });
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
    /// `path`. Every read bumps `seq`, so a late response from a superseded
    /// navigation (the transport is socket-per-request; completion order is
    /// unbounded) is dropped instead of painting rows under the NEW path.
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
        self.files.seq += 1;
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
            image: None,
            loading: kind != FileKind::Binary,
            error: None,
        });
        match kind {
            FileKind::Text => self.spawn_files_text(),
            // Batch K: the image preview loads whole-file via the token-query
            // URL (fs_file_url shape); a failure leaves the placeholder.
            FileKind::Image => self.spawn_files_image(),
            FileKind::Binary => {}
        }
    }

    /// The viewer's whole-file load (F3): stitch consecutive server line
    /// windows from line 1 until the TS viewer's MAX_LINES budget is full
    /// or the file ends. The bridge clamps each window silently
    /// (SERVER_LINE_WINDOW), so one shot at the full budget would quietly
    /// stop at the cap.
    fn spawn_files_text(&mut self) {
        if !matches!(
            self.phase,
            Phase::Discovering | Phase::Connected | Phase::Reconnecting
        ) {
            return;
        }
        if let Some(v) = self.files.viewing.as_mut() {
            v.loading = true;
            v.error = None;
            v.first_line = None;
            v.text = None;
        }
        // One generation per view: a navigation or a newer view bumps seq,
        // so only windows of THIS load append.
        self.files.seq += 1;
        self.spawn_files_text_window(1);
    }

    /// Request one window of the current viewer load, starting at `start`.
    fn spawn_files_text_window(&mut self, start: u64) {
        let Some((instance_id, session_id)) = self.files.owner.clone() else {
            if let Some(v) = self.files.viewing.as_mut() {
                v.error = Some("no active session — open one first".to_string());
            }
            return;
        };
        let Some(path) = self.files.viewing.as_ref().map(|v| v.path.clone()) else {
            return;
        };
        let seq = self.files.seq;
        let hub = self.hub_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub
                .fs_file_text(
                    &instance_id,
                    &session_id,
                    &path,
                    start,
                    viewer_window(start),
                )
                .await;
            let _ = tx.send(LoopEvent::FilesText { seq, result });
        });
    }

    /// A viewer window settled: append it to the accumulated text and keep
    /// loading while another full window fits the budget. The pair is
    /// (first_line, text) with the server's trailing newline already
    /// stripped (protocol::hub_client), so stitching re-adds the join.
    fn on_files_text(&mut self, seq: u64, result: Result<(u64, String), HubApiError>) {
        if seq != self.files.seq {
            return;
        }
        let Some(v) = self.files.viewing.as_mut() else {
            return;
        };
        let mut next_window: Option<u64> = None;
        match result {
            Ok((first_line, text)) => {
                if v.first_line.is_none() {
                    v.first_line = Some(first_line);
                }
                let received = if text.is_empty() {
                    0
                } else {
                    text.split('\n').count() as u64
                };
                if received > 0 {
                    match v.text.as_mut() {
                        Some(prev) => {
                            prev.push('\n');
                            prev.push_str(&text);
                        }
                        None => v.text = Some(text),
                    }
                }
                v.error = None;
                match viewer_next_window(first_line, received) {
                    Some(next) => next_window = Some(next),
                    None => v.loading = false,
                }
            }
            Err(e) => {
                v.loading = false;
                v.error = Some(format!("file: {e}"));
            }
        }
        if let Some(next) = next_window {
            self.spawn_files_text_window(next);
        }
    }

    /// The image viewer's whole-file fetch (batch K): one generation per
    /// view, like the text stitcher — a navigation or a newer view bumps seq
    /// so only THIS load's bytes land.
    fn spawn_files_image(&mut self) {
        if !matches!(
            self.phase,
            Phase::Discovering | Phase::Connected | Phase::Reconnecting
        ) {
            return;
        }
        if let Some(v) = self.files.viewing.as_mut() {
            v.loading = true;
            v.error = None;
            v.image = None;
        }
        let Some((instance_id, session_id)) = self.files.owner.clone() else {
            if let Some(v) = self.files.viewing.as_mut() {
                v.loading = false;
                v.error = Some("no active session — open one first".to_string());
            }
            return;
        };
        let Some(path) = self.files.viewing.as_ref().map(|v| v.path.clone()) else {
            return;
        };
        self.files.seq += 1;
        let seq = self.files.seq;
        let hub = self.hub_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub
                .fs_file_bytes(&instance_id, &session_id, &path)
                .await;
            let _ = tx.send(LoopEvent::FilesImage { seq, result });
        });
    }

    /// The image fetch settled: decode into a gpui raster on success; a
    /// transport error OR an unrasterizable payload (svg, broken bytes)
    /// becomes the viewer's placeholder story.
    fn on_files_image(&mut self, seq: u64, result: Result<Vec<u8>, HubApiError>) {
        if seq != self.files.seq {
            return;
        }
        let Some(v) = self.files.viewing.as_mut() else {
            return;
        };
        v.loading = false;
        match result {
            Ok(bytes) => match decode_preview_image(&bytes) {
                Some(image) => {
                    v.image = Some(Arc::new(image));
                    v.error = None;
                }
                None => {
                    v.error =
                        Some("cannot display this image format (svg and exotic rasters unsupported)"
                            .to_string());
                }
            },
            Err(e) => {
                v.error = Some(describe_files_error(&e));
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

    // ---- ZCode config sections (batch I: skills / mcp / hooks / agents /
    // backups) ----

    /// The capability probe for one section page (the verdict half of its
    /// TS mount pair). No-op before a hub is dialable.
    fn spawn_config_section_probe(&mut self, page: ConfigPageKind) {
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
            let _ = tx.send(LoopEvent::ConfigSectionProbe { page, result });
        });
    }

    /// One section read (TS `loadConfigSection(section)`): runs on every
    /// page open and after every write from the section.
    fn spawn_config_section_load(&mut self, page: ConfigPageKind) {
        if !matches!(
            self.phase,
            Phase::Discovering | Phase::Connected | Phase::Reconnecting
        ) {
            return;
        }
        let panel = &mut self.sections[page.index()];
        panel.loading = true;
        panel.error = None;
        let hub = self.settings_client();
        let tx = self.loop_tx.clone();
        let channel = self.update_channel.clone();
        tokio::spawn(async move {
            let result = match page {
                ConfigPageKind::Skills => hub.settings_skills().await,
                ConfigPageKind::Mcp => hub.settings_mcp().await,
                ConfigPageKind::Hooks => hub.settings_hooks().await,
                ConfigPageKind::Agents => hub.settings_agents().await,
                ConfigPageKind::Backups => hub.settings_backups().await,
                // The check names the channel (TS `settingsAppUpdate(
                // updateChannel)`).
                ConfigPageKind::AppUpdate => hub.settings_app_update(Some(&channel)).await,
            };
            let _ = tx.send(LoopEvent::ConfigSectionLoaded { page, result });
        });
    }

    /// The section probe settled — same verdict handling as
    /// [`Self::on_config_probe`], scoped to the asking page.
    fn on_config_section_probe(
        &mut self,
        page: ConfigPageKind,
        result: Result<(), SettingsApiError>,
    ) {
        let panel = &mut self.sections[page.index()];
        match result {
            Ok(()) => panel.supported = Some(true),
            Err(SettingsApiError::Http { status: 404, .. }) => {
                // Bridge < 0.47.0: the capability card, and the section read
                // behind it cannot work either.
                panel.supported = Some(false);
                panel.loading = false;
            }
            Err(_) => {
                // A real failure belongs to the section read's error story;
                // the verdict stays unknown so a later probe can still run.
            }
        }
    }

    /// The section read settled (the `on_models_loaded` doctrine): Ok keeps
    /// the raw `Value` (the screens decode through config_decode at paint
    /// time), a 404 retires the page's capability verdict, anything else is
    /// the error card.
    fn on_config_section_loaded(
        &mut self,
        page: ConfigPageKind,
        result: Result<Value, SettingsApiError>,
    ) {
        let panel = &mut self.sections[page.index()];
        panel.loading = false;
        match result {
            Ok(v) => {
                panel.payload = Some(v);
                panel.error = None;
            }
            Err(SettingsApiError::Http { status: 404, .. }) => {
                panel.supported = Some(false);
            }
            Err(e) => {
                panel.error = Some(e.to_string());
            }
        }
    }

    /// POST /api/upgrade settled — the TS SettingsPanel note: restarted onto
    /// the disk version, or already the latest; a failure shows its message.
    /// The respawn itself needs no handling here: discovery's poll and the
    /// tabs' reconnect ride it out.
    fn on_hub_upgraded(&mut self, result: Result<HubUpgradeResult, SettingsApiError>) {
        self.hub_upgrade_busy = false;
        self.hub_upgrade_note_until = self.clock.now_ms() + HUB_UPGRADE_NOTE_MS;
        self.hub_upgrade_note = Some(match result {
            Ok(r) if r.restarting => crate::i18n::fill(
                &t!("panel.upgradeRestarted"),
                &[("version", r.disk_version.as_deref().unwrap_or(""))],
            ),
            Ok(r) => crate::i18n::fill(
                &t!("panel.upgradeLatest"),
                &[("version", r.running_version.as_str())],
            ),
            Err(e) => e.to_string(),
        });
    }

    /// POST …/app-update/install settled (ADR 0009). The POST's body is
    /// discarded — the install state lives in the check GET's
    /// `appUpdate.install` block, so the settle immediately polls (a
    /// blocking-POST bridge answers with the final stage already in place;
    /// a background-download one shows progress the tick then refreshes).
    /// A transport failure becomes the failed stage: the screen must show
    /// WHICH step gave up.
    fn on_app_update_installed(&mut self, result: Result<(), SettingsApiError>) {
        self.sections[ConfigPageKind::AppUpdate.index()].busy = false;
        match result {
            Ok(()) => self.spawn_app_update_poll(),
            Err(e) => {
                self.sections[ConfigPageKind::AppUpdate.index()].install =
                    Some(json!({ "stage": "failed", "error": e.to_string() }));
            }
        }
    }

    /// The App Update progress poll (TS `pollAppUpdate`): the same GET the
    /// check runs, named for the same channel. Silent by design — it never
    /// touches the section payload or its loading/error story; a failure
    /// waits for the next tick.
    fn spawn_app_update_poll(&mut self) {
        if !matches!(
            self.phase,
            Phase::Discovering | Phase::Connected | Phase::Reconnecting
        ) {
            return;
        }
        let hub = self.settings_client();
        let tx = self.loop_tx.clone();
        let channel = self.update_channel.clone();
        tokio::spawn(async move {
            let result = hub.settings_app_update(Some(&channel)).await;
            let _ = tx.send(LoopEvent::AppUpdatePolled(result));
        });
    }

    /// The install poll settled: adopt the `appUpdate.install` block when
    /// the answer carries one (TS sets `appUpdateInstall` only then), leave
    /// the last verdict otherwise.
    fn on_app_update_polled(&mut self, result: Result<Value, SettingsApiError>) {
        let block = result
            .ok()
            .and_then(|v| v.get("appUpdate").and_then(|a| a.get("install")).cloned())
            .filter(|b| b.is_object());
        if let Some(block) = block {
            self.sections[ConfigPageKind::AppUpdate.index()].install = Some(block);
        }
    }

    /// One model-connectivity probe settled. The verdict replaces whatever the
    /// same row showed before; a probe for a model the user has since moved
    /// away from still lands in the map (keyed by provider/model), so returning
    /// to the row shows its real answer instead of a stale one.
    fn on_model_test_settled(
        &mut self,
        provider_id: String,
        model_id: String,
        result: Result<protocol::settings::ConnectivityResult, SettingsApiError>,
    ) {
        self.model_test_pending = None;
        let verdict = match result {
            Ok(r) => r,
            Err(e) => protocol::settings::ConnectivityResult {
                success: false,
                message: Some(e.to_string()),
                ..Default::default()
            },
        };
        self.model_tests
            .insert((provider_id, model_id), verdict);
    }

    /// One write through the applyConfigWrite path (label, write, reload):
    /// the future is boxed by the spawn because each route returns its own
    /// concrete future type.
    fn spawn_config_write<F>(
        &mut self,
        page: ConfigPageKind,
        label: &'static str,
        reload: &'static [ConfigPageKind],
        fut: F,
    ) where
        F: std::future::Future<Output = Result<WriteEffect, SettingsApiError>> + Send + 'static,
    {
        self.sections[page.index()].busy = true;
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = fut.await;
            let _ = tx.send(LoopEvent::ConfigWriteSettled {
                page,
                label,
                reload,
                result,
            });
        });
    }

    /// One cross-client pending-restart probe per config entry: a
    /// needs-restart write another client made never reached this app's
    /// `pending_restart` flag, and the restart banner is the only surface
    /// that says "a change is not applied yet". Skipped while the flag is
    /// already armed (the probe can only set it, never clear).
    fn maybe_spawn_pending_restart_check(&mut self) {
        if self.pending_restart {
            return;
        }
        if let Some(instance) = self.active_instance_id() {
            let hub = self.settings_client();
            let tx = self.loop_tx.clone();
            tokio::spawn(async move {
                let result = hub.settings_pending_restart(&instance).await;
                let _ = tx.send(LoopEvent::PendingRestartChecked(result));
            });
        }
    }

    /// The write settled — the applyConfigWrite outcome port (the
    /// `on_model_upsert` doctrine): ok reloads the sections the write
    /// touched, a needs-restart effect ARMS the restart affordance with its
    /// notice, a refusal surfaces its BODY, and the form-close result goes
    /// to the view.
    fn on_config_write_settled(
        &mut self,
        page: ConfigPageKind,
        label: &'static str,
        reload: &[ConfigPageKind],
        result: Result<WriteEffect, SettingsApiError>,
    ) {
        self.sections[page.index()].busy = false;
        let ok = match result {
            Ok(w) if w.ok => {
                if w.effect == Some(Effect::NeedsRestart) {
                    self.pending_restart = true;
                    self.config_notice = Some(NOTICE_NEEDS_RESTART.to_string());
                }
                for r in reload {
                    self.spawn_config_section_load(*r);
                }
                // A restore rewrites the WHOLE config document, so the
                // models page's snapshot is stale too (TS BackupsPage
                // reloads models alongside the four sections).
                if page == ConfigPageKind::Backups {
                    self.spawn_models_load();
                }
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
        let _ = self.ev_tx.send(RuntimeEvent::ConfigWriteResult { ok });
    }

    // ---- Workflows page + gate (batch J) ----

    /// The active tab's instance id — the workflows page's LAST-RESORT
    /// action address (TS `instanceId`) and the gate probe's target.
    fn active_instance_id(&self) -> Option<String> {
        self.active_tab
            .and_then(|id| self.tab(id))
            .map(|t| t.instance_id.clone())
    }

    /// The active tab's (instance, session) pair — the third session
    /// ladder's poll scope. `None` while no tab holds an attached session
    /// (there is nothing to ask conversationRuns about).
    fn active_session_pair(&self) -> Option<(String, String)> {
        self.active_tab
            .and_then(|id| self.tab(id))
            .and_then(|t| t
                .core
                .active_session_id
                .clone()
                .map(|sid| (t.instance_id.clone(), sid)))
    }

    /// The launch-memory joins for the run rows the open page paints
    /// (batch M, TS `lookupLaunch` per row): run id → the session/instance
    /// this app's own start minted. Scoped to the visible rows so the
    /// snapshot carries a handful of entries, never the whole 200-deep
    /// table.
    fn workflow_launch_joins(&self) -> HashMap<String, WorkflowLaunchRef> {
        let page_open = self.workflows.open;
        let rows: Vec<&str> = if !page_open {
            Vec::new()
        } else {
            let mut rows = Vec::new();
            if let Some(o) = &self.workflows.overview {
                rows.extend(o.active_runs.iter().map(|r| r.run_id.as_str()));
                rows.extend(o.recent_runs.iter().map(|r| r.run_id.as_str()));
            }
            if let Some(d) = &self.workflows.detail {
                if let Some(runs) = &d.runs {
                    rows.extend(runs.iter().map(|r| r.run_id.as_str()));
                }
            }
            rows
        };
        rows.into_iter()
            .filter_map(|run_id| {
                self.launches.lookup(run_id).map(|l| {
                    (
                        run_id.to_string(),
                        WorkflowLaunchRef {
                            instance_id: l.instance_id.clone(),
                            acp_session_id: l.acp_session_id.clone(),
                        },
                    )
                })
            })
            .collect()
    }

    /// Anything flying anywhere (TS WorkflowsPage `anyRunning`): the badges'
    /// union across the overview's last-runs and the active-run zone drives
    /// the poll cadence.
    fn workflows_any_active(&self) -> bool {
        self.workflows.overview.as_ref().is_some_and(|o| {
            o.groups.iter().any(|g| {
                g.last_runs
                    .values()
                    .any(|r| r.status == "pending" || r.status == "running")
            }) || o
                .active_runs
                .iter()
                .any(|r| r.status == "pending" || r.status == "running")
        })
    }

    /// One overview read (TS `loadWorkflowHub`): sets the spinner, schedules
    /// the NEXT poll (the adaptive interval's tick lands here — the cadence
    /// follows what the last answer showed), and fires.
    fn spawn_workflow_overview(&mut self, now: u64) {
        if !matches!(
            self.phase,
            Phase::Discovering | Phase::Connected | Phase::Reconnecting
        ) {
            return;
        }
        self.workflows.loading = true;
        let live = self.workflows_any_active();
        self.workflows.next_poll_at =
            now + if live { WORKFLOWS_POLL_LIVE_MS } else { WORKFLOWS_POLL_IDLE_MS };
        let hub = self.settings_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub.workflow_overview().await;
            let _ = tx.send(LoopEvent::WorkflowOverview(result));
        });
    }

    /// The overview settled: success replaces the snapshot (the app renders
    /// the hub's answer as-is — no client-side merging) and re-arms the NEXT
    /// poll off the fresh answer's liveness (the TS effect re-runs when
    /// `anyRunning` flips — an open that lands on live runs must not wait
    /// out the idle interval the pre-read state picked). A failure keeps
    /// the last snapshot, except the FIRST load, which still needs a story.
    /// A 404 is the old-hub window (bridge < 0.60.0) — the actionable
    /// "restart the hub" error, not a dead spinner.
    fn on_workflow_overview(&mut self, result: Result<WorkflowOverviewResponse, SettingsApiError>) {
        self.workflows.loading = false;
        match result {
            Ok(overview) => {
                self.workflows.overview = Some(overview);
                self.workflows.error = None;
                let live = self.workflows_any_active();
                self.workflows.next_poll_at = self.clock.now_ms()
                    + if live {
                        WORKFLOWS_POLL_LIVE_MS
                    } else {
                        WORKFLOWS_POLL_IDLE_MS
                    };
            }
            Err(e) if e.status() == Some(404) => {
                if self.workflows.overview.is_none() {
                    self.workflows.error = Some(
                        "this hub predates the workflow-overview route — restart the hub \
                         (POST /api/upgrade) to pick up the newer bridge"
                            .to_string(),
                    );
                }
            }
            Err(e) => {
                if self.workflows.overview.is_none() {
                    self.workflows.error = Some(e.to_string());
                }
            }
        }
    }

    /// The gate probe (`/settings/all`'s workflow block on the active tab's
    /// instance — the only honest source; the machine-level mount always
    /// reports `{available:false}`).
    fn spawn_workflow_gate_probe(&mut self) {
        if !matches!(
            self.phase,
            Phase::Discovering | Phase::Connected | Phase::Reconnecting
        ) {
            return;
        }
        let Some(instance_id) = self.active_instance_id() else {
            return;
        };
        self.workflows.gate_probe_instance = Some(instance_id.clone());
        let hub = self.settings_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub
                .instance_settings_all(&instance_id)
                .await
                // The per-instance mount always answers the verdict shape; a
                // degrade block (machine-level shape) carries no verdict.
                .map(|all| all.workflow.and_then(|w| w.verdict()));
            let _ = tx.send(LoopEvent::WorkflowGateProbed { instance_id, result });
        });
    }

    /// The third session-resolution ladder's read (TS ActiveRunsBlock's
    /// `load`): the attached session's conversationRuns, polled 5s while
    /// live rows are on screen. Silent by design — a transient failure just
    /// keeps the previous map (stale keys are run-id-exact, so a kept entry
    /// can only act on a run that was in this session).
    fn spawn_workflow_session_runs(&mut self, pair: (String, String)) {
        if !matches!(
            self.phase,
            Phase::Discovering | Phase::Connected | Phase::Reconnecting
        ) {
            return;
        }
        let hub = self.settings_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let (instance_id, session_id) = &pair;
            let result = hub.conversation_runs(instance_id, session_id, None).await;
            let _ = tx.send(LoopEvent::WorkflowSessionRuns { pair, result });
        });
    }

    /// The sessionRuns read settled: guarded by the pair (an in-flight read
    /// from a previous instance/session drops); success REPLACES the map
    /// (run ids are exact, a vanished run must leave it), failure keeps it.
    fn on_workflow_session_runs(
        &mut self,
        pair: (String, String),
        result: Result<ConversationRunsResponse, SettingsApiError>,
    ) {
        if self.workflows.session_runs_pair.as_ref() != Some(&pair) {
            return;
        }
        if let Ok(res) = result {
            let (_, session_id) = &pair;
            self.workflows.session_runs = fold_session_runs(&res.runs, session_id);
        }
    }

    /// The probe settled: only the current active instance's answer may
    /// land — an in-flight probe from a pre-switch instance would pin a
    /// wrong verdict, and every re-probe path requires `gate.is_none()`,
    /// so a wrong answer would stick for good. A missing block (an older
    /// bridge) or a failure keeps the verdict unknown (fail-closed reads
    /// "auto" in the switch row).
    fn on_workflow_gate_probed(
        &mut self,
        instance_id: String,
        result: Result<Option<WorkflowGateBlock>, SettingsApiError>,
    ) {
        if self.active_instance_id().as_deref() != Some(instance_id.as_str()) {
            return;
        }
        if let Ok(Some(gate)) = result {
            self.workflows.gate = Some(gate);
        }
    }

    /// PUT /settings/workflow-gate: the write is busy-guarded at the
    /// command; the answer's effective verdict IS the next read.
    fn on_workflow_gate_set(
        &mut self,
        result: Result<WorkflowGateSetResponse, SettingsApiError>,
    ) {
        self.workflows.gate_busy = false;
        match result {
            Ok(res) if res.ok => {
                self.workflows.gate = Some(res.gate);
                self.config_notice = Some("workflow switch updated".to_string());
            }
            Ok(_) => {
                self.config_notice = Some("workflow gate write failed: refused".to_string());
            }
            Err(e) => {
                self.config_notice = Some(format!("workflow gate write failed: {e}"));
            }
        }
    }

    /// Open one workflow (TS WorkflowDetail's `load`): the detail read plus
    /// the journal history, on the group's instance.
    fn spawn_workflow_detail_loads(&mut self) {
        if !matches!(
            self.phase,
            Phase::Discovering | Phase::Connected | Phase::Reconnecting
        ) {
            return;
        }
        let Some(d) = &self.workflows.detail else {
            return;
        };
        let (seq, scope, name, instance_id) =
            (d.seq, d.scope, d.name.clone(), d.instance_id.clone());
        self.workflows.detail.as_mut().unwrap().loading = true;
        let hub = self.settings_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub.workflow_get(&instance_id, scope, &name).await;
            let _ = tx.send(LoopEvent::WorkflowDetailLoaded { seq, result });
        });
        self.spawn_workflow_runs_load();
    }

    /// The workflow's journal history (the runs list).
    fn spawn_workflow_runs_load(&mut self) {
        if !matches!(
            self.phase,
            Phase::Discovering | Phase::Connected | Phase::Reconnecting
        ) {
            return;
        }
        let Some(d) = &self.workflows.detail else {
            return;
        };
        let (seq, scope, name, instance_id) =
            (d.seq, d.scope, d.name.clone(), d.instance_id.clone());
        let hub = self.settings_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub.workflow_runs_history(&instance_id, Some(scope), Some(&name), None).await;
            let _ = tx.send(LoopEvent::WorkflowRunsLoaded { seq, result });
        });
    }

    /// The detail read settled (seq-guarded: a late answer from a closed or
    /// replaced page drops).
    fn on_workflow_detail_loaded(
        &mut self,
        seq: u64,
        result: Result<WorkflowDetailResponse, SettingsApiError>,
    ) {
        let Some(d) = &mut self.workflows.detail else {
            return;
        };
        if d.seq != seq {
            return;
        }
        d.loading = false;
        match result {
            Ok(detail) => d.detail = Some(detail),
            Err(_) => {
                // A failed read keeps the old payload — the runs list below
                // owns its own error story; the TS toast said the rest.
            }
        }
    }

    /// The runs read settled: only a LANDED answer updates the list (a
    /// failure keeps `None` — never dressed up as "no runs recorded yet"),
    /// and the resumable-verdict join re-reads for the sessions the rows
    /// can act on (this app's launches carry no memory here — the
    /// bridge-joined `acpSessionId` is the actionable alias, bridge 0.57).
    fn on_workflow_runs_loaded(
        &mut self,
        seq: u64,
        result: Result<WorkflowRunsHistoryResponse, SettingsApiError>,
    ) {
        let Some(d) = &mut self.workflows.detail else {
            return;
        };
        if d.seq != seq {
            return;
        }
        if let Ok(runs) = result {
            d.runs = Some(runs.runs.clone());
            // The resumable-verdict ladder collects every session a row can
            // act on: this app's launch memory FIRST (batch M — a row the
            // bridge never aliased still resolves), then the bridge-joined
            // `acpSessionId` (bridge 0.57).
            let sessions: Vec<String> = {
                let mut seen = std::collections::HashSet::new();
                runs.runs
                    .iter()
                    .filter_map(|r| {
                        self.launches
                            .lookup(&r.run_id)
                            .map(|l| l.acp_session_id.clone())
                            .or_else(|| r.acp_session_id.clone())
                    })
                    .filter(|sid| seen.insert(sid.clone()))
                    .collect()
            };
            if !sessions.is_empty() {
                let instance_id = d.instance_id.clone();
                let hub = self.settings_client();
                let tx = self.loop_tx.clone();
                tokio::spawn(async move {
                    let mut out = Vec::new();
                    for sid in &sessions {
                        if let Ok(res) = hub.conversation_runs(&instance_id, sid, None).await {
                            out.push((sid.clone(), res.runs));
                        }
                    }
                    let _ = tx.send(LoopEvent::WorkflowDetailSummaries {
                        seq,
                        result: Ok(out),
                    });
                });
            }
        }
    }

    /// The summaries join settled (seq-guarded): replace the whole map — a
    /// session that disappeared from the rows must not keep a stale
    /// verdict.
    fn on_workflow_detail_summaries(
        &mut self,
        seq: u64,
        result: Result<Vec<(String, Vec<ConversationRunSummary>)>, SettingsApiError>,
    ) {
        let Some(d) = &mut self.workflows.detail else {
            return;
        };
        if d.seq != seq {
            return;
        }
        if let Ok(list) = result {
            d.summaries = list.into_iter().collect();
        }
    }

    /// POST …/start settled: on success the page closes and the returned
    /// `acpSessionId` opens as a tab (the TS closeConfig + openSession
    /// pair), and the minted runId → session join lands in the launch
    /// memory (the only place it will ever be readable — history rows
    /// carry the BACKEND session id). A failure re-opens nothing — the
    /// notice owns the story and the form stays gone (the TS toast path).
    fn on_workflow_started(
        &mut self,
        scope: WorkflowScope,
        name: String,
        instance_id: String,
        result: Result<WorkflowStartResponse, SettingsApiError>,
    ) {
        self.workflows.busy = false;
        match result {
            Ok(res) => {
                if let Some(run_id) = res.run_id.clone() {
                    let launch = protocol::workflow::WorkflowLaunch {
                        run_id,
                        acp_session_id: res.acp_session_id.clone(),
                        instance_id: instance_id.clone(),
                        name: name.clone(),
                        scope,
                        at: 0,
                    };
                    if let Err(e) = self
                        .launches
                        .remember(launch, self.clock.now_ms(), &launches_path())
                    {
                        traceln!("[driver] launch memory save failed: {e}");
                    }
                }
                self.workflows.open = false;
                self.workflows.detail = None;
                self.open_session_by_id(res.acp_session_id, instance_id);
            }
            Err(e) => {
                self.config_notice =
                    Some(format!("start workflow \"{name}\" ({scope:?}) failed: {e}"));
            }
        }
    }

    /// PUT …/meta settled (seq-guarded): a success reloads the whole detail
    /// (TS `if (res) void load()`); a failure explains itself in the notice
    /// — the sheet already closed on submit, like the amend form.
    fn on_workflow_meta_updated(
        &mut self,
        seq: u64,
        result: Result<Value, SettingsApiError>,
    ) {
        let Some(d) = &mut self.workflows.detail else {
            return;
        };
        if d.seq != seq {
            return;
        }
        d.busy = false;
        match result {
            Ok(_) => self.spawn_workflow_detail_loads(),
            Err(e) => {
                self.config_notice = Some(format!("save description failed: {e}"));
            }
        }
    }

    /// POST …/move settled (batch M): on success the page backs out to the
    /// overview (TS `onBack`) — the workflow now lives under the project's
    /// group, so the detail's scope is stale by construction. Seq-guarded:
    /// a late answer from a replaced page only reports, never navigates.
    fn on_workflow_moved(&mut self, seq: u64, result: Result<Value, SettingsApiError>) {
        self.workflows.busy = false;
        match result {
            Ok(_) if self.workflows.detail.as_ref().is_some_and(|d| d.seq == seq) => {
                self.workflows.detail = None;
                self.workflows.workflow_seq += 1;
                let now = self.clock.now_ms();
                self.spawn_workflow_overview(now);
            }
            Ok(_) => {}
            Err(e) => {
                self.config_notice = Some(format!("move workflow failed: {e}"));
            }
        }
    }

    /// DELETE …/workflows/{scope}/{name} settled (batch M): on success the
    /// page backs out to the overview (TS `remove` → `onBack`). Same
    /// seq-guard as the move.
    fn on_workflow_deleted(&mut self, seq: u64, result: Result<Value, SettingsApiError>) {
        self.workflows.busy = false;
        match result {
            Ok(_) if self.workflows.detail.as_ref().is_some_and(|d| d.seq == seq) => {
                self.workflows.detail = None;
                self.workflows.workflow_seq += 1;
                let now = self.clock.now_ms();
                self.spawn_workflow_overview(now);
            }
            Ok(_) => {}
            Err(e) => {
                self.config_notice = Some(format!("delete workflow failed: {e}"));
            }
        }
    }

    /// The "create via conversation" prompt settled: stage it as the
    /// composer draft (never auto-send), close the page, and say so when no
    /// session is open to review it (TS `createViaChat`).
    fn on_workflow_create_prompt(
        &mut self,
        result: Result<WorkflowCreatePromptResponse, SettingsApiError>,
    ) {
        self.workflows.busy = false;
        match result {
            Ok(res) => {
                self.workflows.open = false;
                if self.active_tab.is_none() {
                    let now = self.clock.now_ms();
                    self.toast.notify(
                        now,
                        "prompt staged — open a session to review and send it",
                    );
                }
                let _ = self.ev_tx.send(RuntimeEvent::ComposerPrefill(PromptDraft {
                    text: res.prompt,
                    images: Vec::new(),
                }));
            }
            Err(e) => {
                self.config_notice = Some(format!("load create prompt failed: {e}"));
            }
        }
    }

    /// POST …/resume settled: on success open the run's session (the TS
    /// closeConfig + openSession pair) and refresh the lists the row lives
    /// in.
    fn on_workflow_run_resumed(
        &mut self,
        session_id: String,
        instance_id: String,
        result: Result<Value, SettingsApiError>,
    ) {
        self.workflows.busy = false;
        match result {
            Ok(_) => {
                // Resume success leaves the workflows surface entirely (TS
                // closes the config page and opens the session) — drop the
                // run sub-page too, or reopening Workflows would render the
                // stale run instead of the overview (CloseWorkflows parity).
                self.workflows.open = false;
                self.workflows.detail = None;
                self.workflows.run = None;
                self.workflows.workflow_seq += 1;
                self.open_session_by_id(session_id, instance_id);
            }
            Err(e) => {
                self.config_notice = Some(format!("resume workflow run failed: {e}"));
            }
        }
    }

    /// POST …/stop settled: refresh the open run view (summary + events) —
    /// 409 (already settled) refreshes too; the row just settles.
    fn on_workflow_run_stopped(&mut self, result: Result<Value, SettingsApiError>) {
        self.workflows.busy = false;
        if let Err(e) = result {
            self.config_notice = Some(format!("stop workflow run failed: {e}"));
        }
        self.spawn_workflow_run_reads(true);
    }

    /// POST …/dismiss settled: the row must leave every list — reload the
    /// overview (page-level lists) and the open detail's runs.
    fn on_workflow_run_dismissed(
        &mut self,
        run_id: String,
        result: Result<DismissRunResponse, SettingsApiError>,
    ) {
        self.workflows.busy = false;
        if let Err(e) = result {
            self.config_notice = Some(format!("dismiss run {run_id} failed: {e}"));
        }
        if self.workflows.open {
            let now = self.clock.now_ms();
            self.spawn_workflow_overview(now);
        }
        if self.workflows.detail.is_some() {
            self.spawn_workflow_runs_load();
        }
    }

    /// POST …/dismiss-batch settled (the clear-finished): the hidden rows
    /// must leave the open detail's runs (TS `load()`); the page-level
    /// lists catch up on the next overview poll, as in TS.
    fn on_workflow_runs_dismissed(&mut self, result: Result<DismissRunsResponse, SettingsApiError>) {
        self.workflows.busy = false;
        if let Err(e) = result {
            self.config_notice = Some(format!("clear finished runs failed: {e}"));
        }
        if self.workflows.detail.is_some() {
            self.spawn_workflow_runs_load();
        }
    }

    /// The run view's reads (TS `refreshSummary` + `fetchEvents`): the
    /// conversation summary and the journal page past `after` (`None` =
    /// the head). One summary + one events read per call.
    fn spawn_workflow_run_reads(&mut self, initial: bool) {
        if !matches!(
            self.phase,
            Phase::Discovering | Phase::Connected | Phase::Reconnecting
        ) {
            return;
        }
        let Some(r) = &self.workflows.run else {
            return;
        };
        let (seq, instance_id, session_id, run_id) =
            (r.seq, r.instance_id.clone(), r.session_id.clone(), r.run_id.clone());
        // The head read is also what a poll with nothing held falls back to
        // (TS `fetchEvents(events === null, …)` — an appender with no floor
        // must not silently store nothing).
        let initial = initial || r.events.is_none();
        let after = if initial {
            None
        } else {
            Some(max_run_sequence(r.events.as_deref().unwrap_or(&[])))
        };
        // One journal read at a time (TS `eventsInFlight`): the tick and the
        // "Load more" both gate on this flag, so a manual append cannot race
        // the 3s poll into reading the SAME cursor twice. The flag clears
        // when this read lands (success or failure).
        self.workflows.run.as_mut().unwrap().events_busy = true;
        let hub = self.settings_client();
        let hub2 = self.settings_client();
        let tx = self.loop_tx.clone();
        let tx2 = self.loop_tx.clone();
        let instance_id2 = instance_id.clone();
        let session_id2 = session_id.clone();
        tokio::spawn(async move {
            let result = hub.conversation_runs(&instance_id, &session_id, None).await;
            let _ = tx.send(LoopEvent::WorkflowRunSummary { seq, result });
        });
        tokio::spawn(async move {
            // Newest-first reads want the journal's tail (TS `fetchEvents`'s
            // drain loop): pull up to RUN_EVENT_DRAIN_PAGES pages per call so
            // a long settled journal does not open on its OLDEST page with
            // the ending hidden behind a button. `events_busy` stays raised
            // across the whole drain — the pages land as one.
            let result = 'drain: {
                let mut held: Vec<WorkflowRunEvent> = Vec::new();
                let mut more = false;
                let mut cursor = after;
                for _ in 0..RUN_EVENT_DRAIN_PAGES {
                    match hub2
                        .run_events(&instance_id2, &session_id2, &run_id, cursor)
                        .await
                    {
                        Ok(res) => {
                            more = res.has_more.unwrap_or(false);
                            let next = res.events.iter().map(|e| e.sequence).max();
                            held = merge_run_events(held, res.events);
                            if !more {
                                break;
                            }
                            cursor = Some(match (cursor, next) {
                                (Some(c), Some(n)) => c.max(n),
                                (Some(c), None) => c,
                                (None, Some(n)) => n,
                                // An empty page that still claims has_more:
                                // keep the incoming cursor and stop climbing.
                                (None, None) => 0,
                            });
                        }
                        // A failed page with earlier pages already in hand
                        // keeps them (TS `if (!res) break`): only a total
                        // failure surfaces as an error.
                        Err(err) => {
                            if held.is_empty() {
                                break 'drain Err(err);
                            }
                            break;
                        }
                    }
                }
                Ok(WorkflowRunEventsResponse {
                    ok: true,
                    events: held,
                    has_more: Some(more),
                })
            };
            let _ = tx2.send(LoopEvent::WorkflowRunEvents { seq, initial, result });
        });
    }

    /// The summary read settled (seq-guarded): pick this run's row; two
    /// consecutive failures retire the poll (read-only degradation).
    fn on_workflow_run_summary(
        &mut self,
        seq: u64,
        result: Result<ConversationRunsResponse, SettingsApiError>,
    ) {
        let Some(r) = &mut self.workflows.run else {
            return;
        };
        if r.seq != seq {
            return;
        }
        match result {
            Ok(res) => {
                r.fail = 0;
                r.summary = res.runs.into_iter().find(|s| s.run_id == r.run_id);
            }
            Err(_) => {
                r.fail += 1;
                if r.fail >= 2 {
                    r.session_dead = true;
                }
            }
        }
    }

    /// The journal read settled (seq-guarded): the head replaces; an append
    /// page is sequence-filtered regardless — a cursor the server could not
    /// honor answers with the FULL journal, which must never re-append.
    fn on_workflow_run_events(
        &mut self,
        seq: u64,
        initial: bool,
        result: Result<WorkflowRunEventsResponse, SettingsApiError>,
    ) {
        let Some(r) = &mut self.workflows.run else {
            return;
        };
        if r.seq != seq {
            return;
        }
        r.events_busy = false;
        let Ok(res) = result else {
            return;
        };
        r.has_more = res.has_more.unwrap_or(false);
        if initial {
            r.events = Some(res.events);
        } else if let Some(held) = r.events.take() {
            r.events = Some(merge_run_events(held, res.events));
        }
    }

    /// The amendment settled (seq-guarded — a stale answer must not re-point
    /// whatever run page is open now): on success re-point the run view when
    /// the amendment superseded the flying run (the NEW id continues the
    /// work — its journal cursor starts over, so the reads reset), then
    /// reload.
    fn on_workflow_run_amended(
        &mut self,
        seq: u64,
        result: Result<AmendRunSettingsResponse, SettingsApiError>,
    ) {
        if !self
            .workflows
            .run
            .as_ref()
            .is_some_and(|r| r.seq == seq)
        {
            return;
        }
        match result {
            Ok(res) => {
                let mut repointed = false;
                if let Some(r) = &mut self.workflows.run {
                    r.amend_busy = false;
                    repointed = res.run_id != r.run_id;
                    r.run_id = res.run_id;
                    if repointed {
                        // The new run's journal is a fresh sequence space —
                        // the old cursor must not survive (the TS
                        // key={runId} remount), and neither do the lazy
                        // tabs, the open viewer, or the expanded node.
                        r.events = None;
                        r.has_more = false;
                        r.summary = None;
                        r.reset_tabs();
                    }
                }
                if repointed {
                    if let Some(r) = &mut self.workflows.run {
                        r.seq = self.workflows.workflow_seq;
                    }
                    self.workflows.workflow_seq += 1;
                }
                self.config_notice = Some(if repointed {
                    "run settings updated — the amendment superseded the run; the view re-pointed to the new run".to_string()
                } else {
                    "run settings updated".to_string()
                });
                self.spawn_workflow_run_reads(true);
            }
            Err(e) => {
                if let Some(r) = &mut self.workflows.run {
                    r.amend_busy = false;
                }
                self.config_notice = Some(format!("amend run settings failed: {e}"));
            }
        }
    }

    // ---- the run page's lazy tabs + viewer (batch L) ----

    /// Whether the hub is reachable for a run-page read (the same phase
    /// gate the journal reads use).
    fn workflow_reads_allowed(&self) -> bool {
        matches!(
            self.phase,
            Phase::Discovering | Phase::Connected | Phase::Reconnecting
        )
    }

    /// The artifacts tab's lazy read (TS `useEffect([tab])`): one fetch on
    /// first open; a failed read leaves `None` so re-opening retries.
    fn spawn_workflow_run_artifacts(&mut self) {
        if !self.workflow_reads_allowed() {
            return;
        }
        let Some(r) = &mut self.workflows.run else {
            return;
        };
        if r.artifacts.is_some() || r.artifacts_busy {
            return;
        }
        r.artifacts_busy = true;
        let (seq, instance_id, session_id, run_id) =
            (r.seq, r.instance_id.clone(), r.session_id.clone(), r.run_id.clone());
        let hub = self.settings_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub.run_artifacts(&instance_id, &session_id, &run_id).await;
            let _ = tx.send(LoopEvent::WorkflowRunArtifacts { seq, result });
        });
    }

    /// The workspace tab's lazy read (same contract as the artifacts).
    fn spawn_workflow_run_workspace(&mut self) {
        if !self.workflow_reads_allowed() {
            return;
        }
        let Some(r) = &mut self.workflows.run else {
            return;
        };
        if r.nodes.is_some() || r.nodes_busy {
            return;
        }
        r.nodes_busy = true;
        let (seq, instance_id, session_id, run_id) =
            (r.seq, r.instance_id.clone(), r.session_id.clone(), r.run_id.clone());
        let hub = self.settings_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub.run_workspace(&instance_id, &session_id, &run_id).await;
            let _ = tx.send(LoopEvent::WorkflowRunWorkspace { seq, result });
        });
    }

    fn on_workflow_run_artifacts(
        &mut self,
        seq: u64,
        result: Result<RunArtifactsResponse, SettingsApiError>,
    ) {
        let Some(r) = &mut self.workflows.run else {
            return;
        };
        if r.seq != seq {
            return;
        }
        r.artifacts_busy = false;
        match result {
            Ok(res) => {
                // The route hands raw values; one that does not even carry
                // an id cannot be listed (TS would render an idless row —
                // nothing to open). Permissive on everything else.
                r.artifacts = Some(
                    res.artifacts
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|v| serde_json::from_value::<WorkflowArtifact>(v).ok())
                        .collect(),
                );
            }
            Err(e) => {
                self.config_notice = Some(format!("load artifacts failed: {e}"));
            }
        }
    }

    fn on_workflow_run_workspace(
        &mut self,
        seq: u64,
        result: Result<WorkflowWorkspaceResponse, SettingsApiError>,
    ) {
        let Some(r) = &mut self.workflows.run else {
            return;
        };
        if r.seq != seq {
            return;
        }
        r.nodes_busy = false;
        match result {
            Ok(res) => r.nodes = Some(res.nodes),
            Err(e) => {
                self.config_notice = Some(format!("load workspace failed: {e}"));
            }
        }
    }

    /// One workspace row's result read (TS `loadNode`): the LATEST asked
    /// row wins — `node_busy` names it, a late answer for another row is
    /// dropped.
    fn spawn_workflow_run_node(&mut self, key: String, site_id: String, ordinal: u64) {
        if !self.workflow_reads_allowed() {
            return;
        }
        let Some(r) = &mut self.workflows.run else {
            return;
        };
        r.node_busy = Some(key.clone());
        let (seq, instance_id, session_id, run_id) =
            (r.seq, r.instance_id.clone(), r.session_id.clone(), r.run_id.clone());
        let hub = self.settings_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub
                .run_node_result(&instance_id, &session_id, &run_id, &site_id, ordinal)
                .await;
            let _ = tx.send(LoopEvent::WorkflowRunNodeResult { seq, key, result });
        });
    }

    fn on_workflow_run_node_result(
        &mut self,
        seq: u64,
        key: String,
        result: Result<WorkflowNodeResultResponse, SettingsApiError>,
    ) {
        let Some(r) = &mut self.workflows.run else {
            return;
        };
        if r.seq != seq || r.node_busy.as_deref() != Some(key.as_str()) {
            return;
        }
        r.node_busy = None;
        match result {
            Ok(res) => {
                // TS: the error wins over the result when the backend sent
                // one; a missing both renders as an empty scalar.
                let body = res
                    .error
                    .filter(|e| !e.is_null())
                    .or(res.result)
                    .unwrap_or(Value::Null);
                r.node_result = Some(WorkflowNodeResultView {
                    key,
                    status: res.status,
                    body,
                    truncated: res.truncated.unwrap_or(false),
                });
            }
            Err(e) => {
                self.config_notice = Some(format!("load node result failed: {e}"));
            }
        }
    }

    /// Opens the viewer for one listed artifact (TS `setOpenArtifact`) and
    /// starts its first read by kind: markdown/file stream text chunks,
    /// the four preset kinds page their inventory, anything else has
    /// nothing to fetch (its metadata card renders from the row).
    fn open_workflow_artifact(&mut self, artifact_id: &str) {
        let Some(r) = &mut self.workflows.run else {
            return;
        };
        let Some(artifact) = r
            .artifacts
            .as_ref()
            .and_then(|list| list.iter().find(|a| a.id == artifact_id))
            .cloned()
        else {
            return;
        };
        self.workflows.workflow_seq += 1;
        let kind = artifact.kind.clone();
        r.artifact = Some(WorkflowArtifactViewer {
            seq: self.workflows.workflow_seq,
            artifact,
            busy: false,
            bytes: Vec::new(),
            text: None,
            binary: false,
            total_bytes: None,
            next_offset: None,
            items: None,
            items_has_more: false,
            items_cursor: None,
        });
        if kind == "markdown" || kind == "file" {
            self.spawn_workflow_artifact_chunk(0, false);
        } else if artifact_is_preset(&kind) {
            self.spawn_workflow_artifact_items(true);
        }
    }

    /// One text chunk of the open viewer (TS `loadChunk`): `READ_CHUNK`
    /// bytes from `offset` of the artifact's latest version.
    fn spawn_workflow_artifact_chunk(&mut self, offset: u64, append: bool) {
        if !self.workflow_reads_allowed() {
            return;
        }
        let Some(r) = &mut self.workflows.run else {
            return;
        };
        let Some(v) = &mut r.artifact else {
            return;
        };
        if v.busy {
            return;
        }
        v.busy = true;
        let (seq, artifact_id, version) = (v.seq, v.artifact.id.clone(), latest_version(&v.artifact));
        let (instance_id, session_id, run_id) =
            (r.instance_id.clone(), r.session_id.clone(), r.run_id.clone());
        let hub = self.settings_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub
                .run_artifact_read(
                    &instance_id,
                    &session_id,
                    &run_id,
                    &artifact_id,
                    version,
                    offset,
                    Some(READ_CHUNK),
                )
                .await;
            let _ = tx.send(LoopEvent::WorkflowRunArtifactChunk { seq, append, result });
        });
    }

    fn on_workflow_run_artifact_chunk(
        &mut self,
        seq: u64,
        append: bool,
        result: Result<WorkflowArtifactReadResponse, SettingsApiError>,
    ) {
        let Some(v) = self
            .workflows
            .run
            .as_mut()
            .and_then(|r| r.artifact.as_mut())
            .filter(|v| v.seq == seq)
        else {
            return;
        };
        v.busy = false;
        let res = match result {
            Ok(res) => res,
            Err(e) => {
                self.config_notice = Some(format!("read artifact failed: {e}"));
                return;
            }
        };
        v.total_bytes = res.total_bytes;
        v.next_offset = res.next_offset;
        match decode_base64(&res.data_base64) {
            Some(chunk) => {
                if !append {
                    v.bytes.clear();
                }
                v.bytes.extend_from_slice(&chunk);
                let text = String::from_utf8_lossy(&v.bytes).into_owned();
                // The verdict reads the WHOLE text once the chunks stop
                // coming (TS: the effect on `nextOffset === null`); it
                // never flips back.
                if v.next_offset.is_none() && binary_verdict(&text) {
                    v.binary = true;
                }
                v.text = Some(Arc::from(text));
            }
            None => {
                // TS `atob` threw: the payload is not text at all.
                v.binary = true;
                v.text = None;
                v.bytes.clear();
            }
        }
    }

    /// One inventory page of the open viewer (TS `loadData`): the head, or
    /// the page past the last sequence held.
    fn spawn_workflow_artifact_items(&mut self, initial: bool) {
        if !self.workflow_reads_allowed() {
            return;
        }
        let Some(r) = &mut self.workflows.run else {
            return;
        };
        let Some(v) = &mut r.artifact else {
            return;
        };
        if v.busy {
            return;
        }
        v.busy = true;
        let after = if initial { None } else { v.items_cursor };
        let (seq, artifact_id) = (v.seq, v.artifact.id.clone());
        let (instance_id, session_id, run_id) =
            (r.instance_id.clone(), r.session_id.clone(), r.run_id.clone());
        let hub = self.settings_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub
                .run_artifact_data(&instance_id, &session_id, &run_id, &artifact_id, after, None)
                .await;
            let _ = tx.send(LoopEvent::WorkflowRunArtifactItems { seq, initial, result });
        });
    }

    fn on_workflow_run_artifact_items(
        &mut self,
        seq: u64,
        initial: bool,
        result: Result<WorkflowArtifactItemsResponse, SettingsApiError>,
    ) {
        let Some(v) = self
            .workflows
            .run
            .as_mut()
            .and_then(|r| r.artifact.as_mut())
            .filter(|v| v.seq == seq)
        else {
            return;
        };
        v.busy = false;
        let res = match result {
            Ok(res) => res,
            Err(e) => {
                self.config_notice = Some(format!("load artifact items failed: {e}"));
                return;
            }
        };
        if let Some(last) = res.items.last().and_then(|it| it.sequence) {
            v.items_cursor = Some(last);
        }
        let mapped = map_artifact_items(res.items);
        v.items = Some(match v.items.take() {
            // Same defensive dedupe as the journal poll: an unadvancing
            // cursor answers with the same page — keep only new sequences.
            Some(held) if !initial => merge_artifact_items(held, mapped),
            _ => mapped,
        });
        v.items_has_more = res.has_more.unwrap_or(false);
    }

    /// The viewer's "Load more": the next text chunk or the next page.
    fn workflow_artifact_load_more(&mut self) {
        let Some(v) = self.workflows.run.as_ref().and_then(|r| r.artifact.as_ref()) else {
            return;
        };
        if v.busy {
            return;
        }
        let kind = v.artifact.kind.as_str();
        if kind == "markdown" || kind == "file" {
            if let Some(offset) = v.next_offset {
                self.spawn_workflow_artifact_chunk(offset, true);
            }
        } else if artifact_is_preset(kind) && v.items_has_more {
            self.spawn_workflow_artifact_items(false);
        }
    }

    /// The OpenSession handler's body (appStore `openSession`): a tab
    /// already holding the session activates; otherwise a new tab attaches.
    /// Shared by the discovery rows and the workflow start/resume paths.
    fn open_session_by_id(&mut self, session_id: String, instance_id: String) {
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
    /// `/api/quota` answers the RAW combined payload (the settings mount's
    /// `/settings/quota` wraps it in `{ok, quota}` — parsing that bare
    /// yielded "no data" on every successful fetch).
    ///
    /// The status bar's dock line rides along: it is the same account quota
    /// in one line, so a read that repaints the drawer or page (a manual
    /// refresh, a spent reset card) must not leave the resident line stale.
    /// The two are separate hub endpoints with separate caches, so the dock
    /// needs its own request — `spawn_quota_dock` dedupes an in-flight one.
    fn spawn_quota_usage(&mut self) {
        if !matches!(
            self.phase,
            Phase::Discovering | Phase::Connected | Phase::Reconnecting
        ) {
            return;
        }
        self.spawn_quota_dock();
        let hub = self.settings_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            // parseUsageStats: a 2xx body that does not fit the combined
            // shape reads as no data — NOT as a failed fetch.
            let result = match hub.quota().await {
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

    /// The title-bar quota dock line (TS `refreshQuotaDock`, ADR-0021):
    /// pulled when the hub first answers, at every turn end (usage moved —
    /// the bridge force-refreshes its cache exactly then, so the read is
    /// cheap), and alongside every quota usage read (`spawn_quota_usage`).
    /// Failures stay silent and sticky.
    fn spawn_quota_dock(&mut self) {
        if !matches!(
            self.phase,
            Phase::Discovering | Phase::Connected | Phase::Reconnecting
        ) {
            return;
        }
        if self.quota_dock_loading {
            return;
        }
        self.quota_dock_loading = true;
        let gen = self.quota_dock_gen;
        let hub = self.hub_client();
        let tx = self.loop_tx.clone();
        tokio::spawn(async move {
            let result = hub.quota_dock().await;
            let _ = tx.send(LoopEvent::QuotaDock { gen, result });
        });
    }

    /// Sticky-on-failure: a transient hub hiccup must not blank the resident
    /// line; only a successful `None` (no quota data) clears it. A result
    /// from the PREVIOUS lifecycle (a hub switch raced the read) drops
    /// whole — it must neither paint the old hub's line nor disturb the new
    /// lifecycle's in-flight bookkeeping.
    fn on_quota_dock(&mut self, gen: u64, result: Result<Option<String>, HubApiError>) {
        if gen != self.quota_dock_gen {
            return;
        }
        self.quota_dock_loading = false;
        if let Ok(line) = result {
            self.quota_dock = line;
        }
    }

    /// Persist one session's readouts after its turn settled (batch T — the
    /// first-open seed source, the TS `writeSessionCache` readout half). A
    /// failed write logs and keeps the in-memory map; the next settle
    /// retries.
    fn record_readouts(&mut self, session_id: &str) {
        let readout = self
            .tabs
            .iter()
            .find(|t| t.core.active_session_id.as_deref() == Some(session_id))
            .map(|t| SessionReadout {
                usage: t.core.usage,
                cache_hit: t.core.cache_hit,
                turn_stats: t.core.turn_stats,
            });
        let Some(readout) = readout else {
            return;
        };
        self.readouts.record(session_id, readout);
        if let Err(e) = self.readouts.save(&readouts_path()) {
            traceln!("[driver] readout store write failed: {e}");
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
        // The hub-upgrade note's 5s timer (TS SidePanel's effect).
        if self.hub_upgrade_note.is_some() && now >= self.hub_upgrade_note_until {
            self.hub_upgrade_note = None;
        }
        // The System Status page's 5s poll (TS `setInterval(loadSystemStats,
        // POLL_MS)`) — live only while the page is open.
        if self.system.open && now >= self.system.next_poll_at {
            self.spawn_system_stats(now);
        }
        // The App Update progress poll (TS AppUpdatePage's 2s interval —
        // live while the install flies). Wider than TS's `busy`-derived
        // interval on purpose: the POST itself can block for the whole
        // download, so the poll also runs while it is in flight, and a page
        // re-opened mid-download resumes polling instead of showing a
        // frozen verdict.
        let au = &self.sections[ConfigPageKind::AppUpdate.index()];
        let au_flying = au.open
            && (au.busy
                || au.install
                    .as_ref()
                    .and_then(|i| i.get("stage"))
                    .and_then(|s| s.as_str())
                    .is_some_and(|s| s == "downloading" || s == "installing"));
        if au_flying && now >= self.next_app_update_poll_at {
            self.next_app_update_poll_at = now + APP_UPDATE_POLL_MS;
            self.spawn_app_update_poll();
        }
        // The Workflows page's adaptive overview poll (TS WorkflowsPage
        // 5s-live / 30s-idle): a journal row can be terminal-and-wrong, so
        // the poll is NOT gated on "something already looks live".
        if self.workflows.open && now >= self.workflows.next_poll_at {
            self.spawn_workflow_overview(now);
        }
        // The gate probe's retry rule (batch M's seed generalized to TS
        // ZCodeConfigScreen's `instanceId` effect dep): the gated config
        // entry and the session manage row need the verdict to render at
        // all, so while it is unknown a CHANGE of the active instance
        // re-probes — the first live instance included (`gate_probe_instance`
        // starts None). A probe that failed for THIS instance is not
        // hammered on a timer; the next config-dock / workflows-page open
        // retries it.
        if self.workflows.gate.is_none()
            && matches!(
                self.phase,
                Phase::Discovering | Phase::Connected | Phase::Reconnecting
            )
            && self.active_instance_id() != self.workflows.gate_probe_instance
        {
            self.spawn_workflow_gate_probe();
        }
        // The third session-resolution ladder (TS ActiveRunsBlock): the
        // attached session's conversationRuns, polled 5s while the page
        // shows live rows — a run neither launch memory nor the bridge join
        // could place is still actionable if it flies in the CURRENT
        // session. Any dropped condition (page closed, a detail/run
        // sub-page replaces the overview where the block lives, zone
        // empty, no session) clears the map, and a pair switch drops it
        // FIRST so an in-flight answer from the old pair cannot land (TS's
        // epoch ref).
        let live_rows = self
            .workflows
            .overview
            .as_ref()
            .is_some_and(|o| !o.active_runs.is_empty());
        let session_pair = self.active_session_pair();
        match session_pair.filter(|_| {
            self.workflows.open
                && self.workflows.detail.is_none()
                && self.workflows.run.is_none()
                && live_rows
        }) {
            Some(pair) => {
                if self.workflows.session_runs_pair.as_ref() != Some(&pair) {
                    self.workflows.session_runs.clear();
                    self.workflows.session_runs_pair = Some(pair.clone());
                    self.workflows.next_session_runs_poll_at = 0;
                }
                if now >= self.workflows.next_session_runs_poll_at {
                    self.workflows.next_session_runs_poll_at = now + WORKFLOW_SESSION_RUNS_POLL_MS;
                    self.spawn_workflow_session_runs(pair);
                }
            }
            None => {
                if self.workflows.session_runs_pair.is_some() {
                    self.workflows.session_runs.clear();
                    self.workflows.session_runs_pair = None;
                }
            }
        }
        // One workflow's runs-history poll (TS WorkflowDetail's 5s tick,
        // `skipDetail` mode): armed only while its newest run flies — meta
        // is static between runs, so only history + summaries refresh.
        let detail_poll_due = self.workflows.detail.as_ref().is_some_and(|d| {
            !d.busy
                && d.runs
                    .as_ref()
                    .is_some_and(|runs| runs.iter().any(|r| crate::workflows_page::run_is_active(&r.status)))
                && now >= d.next_runs_poll_at
        });
        if detail_poll_due {
            if let Some(d) = self.workflows.detail.as_mut() {
                d.next_runs_poll_at = now + WORKFLOW_DETAIL_POLL_MS;
            }
            self.spawn_workflow_runs_load();
        }
        // The run view's 3s journal poll (TS WorkflowRunDetail): live while
        // the run is unsettled or its status is still unknown; two failed
        // ticks retire it (session_dead) and the read-only notice stays.
        let run_poll_due = self.workflows.run.as_ref().is_some_and(|r| {
            let active = !r.session_dead
                && r.summary
                    .as_ref()
                    .map(|s| s.status == "running" || s.status == "pending")
                    .unwrap_or(true);
            active && now >= r.next_poll_at && !r.events_busy
        });
        if run_poll_due {
            if let Some(r) = self.workflows.run.as_mut() {
                r.next_poll_at = now + WORKFLOW_RUN_POLL_MS;
            }
            self.spawn_workflow_run_reads(false);
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

    /// Batch H1 persistence sync (the TS `setQueue` → `savePending` seam):
    /// fold every tab's live queues into the persisted map — live queues
    /// overwrite, an attached session with no queue drops its entry, and
    /// sessions no tab holds keep their drafts until the next attach. Runs
    /// after every driver-loop turn; queue mutations only happen inside
    /// command/loop handlers, so one diff per turn catches them all, and a
    /// quit (which never drains queues) leaves the store current.
    fn sync_pending(&mut self) {
        let live: Vec<(Option<&str>, &HashMap<String, Vec<PromptDraft>>)> = self
            .tabs
            .iter()
            .map(|t| (t.core.active_session_id.as_deref(), &t.core.pending_prompts))
            .collect();
        if let Some(next) = merge_pending_queues(self.pending.snapshot(), &live) {
            if let Err(e) = self.pending.adopt(next) {
                traceln!("[driver] pending queue save failed: {e}");
            }
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
            LoopEvent::TabReincubated { tab, result } => match result {
                Ok(created) => {
                    // Re-point and reconnect: the tab keeps its target
                    // session (the attach replays it over the new bridge).
                    // The same id back (a re-registering bridge) reconnects
                    // just the same — only without the toast.
                    let outcome = self.tab_mut(tab).map(|t| {
                        let was = t.instance_id.clone();
                        t.instance_id = created.id.clone();
                        t.reconnect_attempts = 0;
                        t.retry_at = None;
                        was != created.id
                    });
                    if outcome.is_some() {
                        if outcome == Some(true) {
                            self.toast.notify(now, "backend restarted — resuming the session");
                            traceln!(
                                "[driver] tab {tab} re-pointed at instance {} (reused: {})",
                                created.id,
                                created.reused
                            );
                        }
                        self.spawn_connect(tab);
                    }
                }
                Err(e) => {
                    // The revival itself failed (unknown project, hub
                    // refusing): parking in Failed is more honest than an
                    // endless retry ladder against a dead id.
                    traceln!("[driver] tab {tab} re-incubation failed: {e}");
                    if let Some(t) = self.tab_mut(tab) {
                        t.state = TabPhase::Failed(format!("backend restart failed: {e}"));
                    }
                }
            },
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
                // Turn end = quota refresh point (TS runPrompt's finally; the
                // bridge force-refreshes its dock cache at the same moment).
                if !running {
                    self.spawn_quota_dock();
                    // And the readout persistence point (batch T): the
                    // just-folded usage/cache/tps become the next first-open
                    // seed for this session.
                    self.record_readouts(&session_id);
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
                        Ok(value) => {
                            t.core.on_prompt_result(now, &req, &value);
                            // The desktop-driven settle path folds the turn
                            // usage here (the turnState end may have ridden
                            // its own broadcast earlier) — persist whichever
                            // landed last.
                            self.record_readouts(&req.session_id);
                        }
                        Err(e) => t.core.on_prompt_failed(now, Some(&req), &e),
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
                        // The FIRST page names the serving instance — the
                        // delete target for every row of the view (load-more
                        // pages never re-capture it, TS dialog parity).
                        if more {
                            sel.rows.extend(page.sessions);
                        } else {
                            sel.instance_id = page.instance.id;
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
            // ---- session row actions (batch G-light) ----
            LoopEvent::SessionRenamed {
                instance_id,
                session_id,
                title,
                result,
            } => match result {
                Ok(()) => {
                    // Optimistic local rewrite (appStore renameSession's
                    // instances patch); the bridge's session_info_update
                    // broadcast lands over the tabs' sockets as well.
                    for inst in &mut self.instances {
                        if inst.id == instance_id {
                            for sess in &mut inst.sessions {
                                if sess.session_id == session_id {
                                    sess.title = Some(title.clone());
                                }
                            }
                        }
                    }
                    self.toast.notify(now, "Session renamed");
                }
                Err(e) => {
                    self.toast.notify(now, &format!("rename session failed: {e}"));
                }
            },
            LoopEvent::SessionClosed {
                instance_id,
                session_id,
                result,
            } => match result {
                Ok(()) => self.retire_closed_session(&instance_id, &session_id, now),
                Err(e) => {
                    let msg = if e.status == Some(409) {
                        // A turn is in flight — stop it first (ADR-0006).
                        rust_i18n::t!("notice.sessionRunning").into_owned()
                    } else {
                        format!("close session failed: {e}")
                    };
                    self.toast.notify(now, &msg);
                }
            },
            LoopEvent::SessionClosedRemotely { tab: _, session_id } => {
                // The bridge's `$/zcode/session_closed` broadcast (martty's
                // quit-cleanly contract): someone retired this conversation.
                // Retire locally exactly like an own close — tab, row,
                // tombstone — so the reconnect ladder never sees the gone
                // session and the re-incubator never resumes a deliberately
                // closed conversation (a serve bridge self-exits right after
                // this broadcast; without the retirement the revive path
                // pulled the session back up, 2026-10-23). The hub delivers
                // one copy per session alias to EVERY attached socket, so
                // the delivering tab is just where the copy landed: the
                // holder's own connection may be mid-reconnect at that
                // moment (its ladder is precisely what must not resurrect
                // the session) and a sibling tab's socket gets the copy —
                // look the holder up across every tab.
                let holder = self.tabs.iter().find(|t| {
                    t.core.active_session_id.as_deref() == Some(session_id.as_str())
                        || t.target.as_deref() == Some(session_id.as_str())
                });
                if let Some(t) = holder {
                    let instance_id = t.instance_id.clone();
                    self.retire_closed_session(&instance_id, &session_id, now);
                }
            }
            LoopEvent::HistorySessionDeleted { session_id, result } => {
                // Clear the in-flight marker first (one mutable pass; the
                // error slot is written again below on failure).
                if let Some(sel) = self.history.selected.as_mut() {
                    sel.deleting = None;
                }
                match result {
                    Ok(()) => {
                        // The tombstone hides the conversation from EVERY
                        // listing: drop it from the open page AND the
                        // sidebar's discovery rows, so it never resurrects
                        // for a refresh cycle (bytes are kept — a new session
                        // in the project records a fresh row).
                        if let Some(sel) = self.history.selected.as_mut() {
                            sel.error = None;
                            sel.rows.retain(|r| r.session_id != session_id);
                        }
                        for inst in &mut self.instances {
                            inst.sessions.retain(|s| s.session_id != session_id);
                        }
                        self.toast.notify(now, &rust_i18n::t!("notice.sessionDeleted"));
                    }
                    Err(e) => {
                        let msg = if e.status == Some(409) {
                            // Live or a turn in flight (ADR-0031): ending the
                            // live copy is the end action's job.
                            rust_i18n::t!("notice.sessionDeleteLive").into_owned()
                        } else {
                            format!("delete session failed: {e}")
                        };
                        if let Some(sel) = self.history.selected.as_mut() {
                            sel.error = Some(msg.clone());
                        }
                        self.toast.notify(now, &msg);
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
                            rust_i18n::t!("projectDialog.tooOld").into_owned()
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
                            rust_i18n::t!("projectDialog.tooOld").into_owned()
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
                            rust_i18n::t!("notice.projectUnknown").into_owned()
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
            LoopEvent::QuotaDock { gen, result } => self.on_quota_dock(gen, result),
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
            LoopEvent::FilesImage { seq, result } => self.on_files_image(seq, result),
            // ---- ZCode config sections (batch I) ----
            LoopEvent::ConfigSectionProbe { page, result } => {
                self.on_config_section_probe(page, result);
            }
            LoopEvent::ConfigSectionLoaded { page, result } => {
                self.on_config_section_loaded(page, result);
            }
            LoopEvent::ConfigWriteSettled {
                page,
                label,
                reload,
                result,
            } => self.on_config_write_settled(page, label, reload, result),
            LoopEvent::PendingRestartChecked(result) => {
                // A silent arm by design: the banner it raises IS the
                // notice. A failure is dropped the same way — the next
                // config entry re-probes.
                if let Ok(status) = result {
                    if status.pending_restart && !self.pending_restart {
                        self.pending_restart = true;
                        self.config_notice = Some(NOTICE_NEEDS_RESTART.to_string());
                    }
                }
            }
            LoopEvent::ModelTestSettled {
                provider_id,
                model_id,
                result,
            } => self.on_model_test_settled(provider_id, model_id, result),
            LoopEvent::HubUpgraded(result) => self.on_hub_upgraded(result),
            LoopEvent::AppUpdateInstalled(result) => self.on_app_update_installed(result),
            LoopEvent::AppUpdatePolled(result) => self.on_app_update_polled(result),
            // ---- Workflows page + gate (batch J) ----
            LoopEvent::WorkflowOverview(result) => self.on_workflow_overview(result),
            LoopEvent::WorkflowGateProbed { instance_id, result } => {
                self.on_workflow_gate_probed(instance_id, result)
            }
            LoopEvent::WorkflowGateSet(result) => self.on_workflow_gate_set(result),
            LoopEvent::WorkflowDetailLoaded { seq, result } => {
                self.on_workflow_detail_loaded(seq, result);
            }
            LoopEvent::WorkflowRunsLoaded { seq, result } => {
                self.on_workflow_runs_loaded(seq, result);
            }
            LoopEvent::WorkflowDetailSummaries { seq, result } => {
                self.on_workflow_detail_summaries(seq, result);
            }
            LoopEvent::WorkflowStarted {
                scope,
                name,
                instance_id,
                result,
            } => self.on_workflow_started(scope, name, instance_id, result),
            LoopEvent::WorkflowMetaUpdated { seq, result } => {
                self.on_workflow_meta_updated(seq, result);
            }
            LoopEvent::WorkflowMoved { seq, result } => {
                self.on_workflow_moved(seq, result);
            }
            LoopEvent::WorkflowDeleted { seq, result } => {
                self.on_workflow_deleted(seq, result);
            }
            LoopEvent::WorkflowCreatePrompt(result) => self.on_workflow_create_prompt(result),
            LoopEvent::WorkflowRunResumed {
                session_id,
                instance_id,
                result,
            } => self.on_workflow_run_resumed(session_id, instance_id, result),
            LoopEvent::WorkflowRunStopped(result) => self.on_workflow_run_stopped(result),
            LoopEvent::WorkflowRunDismissed { run_id, result } => {
                self.on_workflow_run_dismissed(run_id, result);
            }
            LoopEvent::WorkflowRunsDismissed(result) => {
                self.on_workflow_runs_dismissed(result);
            }
            LoopEvent::WorkflowSessionRuns { pair, result } => {
                self.on_workflow_session_runs(pair, result);
            }
            LoopEvent::WorkflowRunSummary { seq, result } => {
                self.on_workflow_run_summary(seq, result);
            }
            LoopEvent::WorkflowRunEvents { seq, initial, result } => {
                self.on_workflow_run_events(seq, initial, result);
            }
            LoopEvent::WorkflowRunAmended { seq, result } => {
                self.on_workflow_run_amended(seq, result)
            }
            LoopEvent::WorkflowRunArtifacts { seq, result } => {
                self.on_workflow_run_artifacts(seq, result);
            }
            LoopEvent::WorkflowRunWorkspace { seq, result } => {
                self.on_workflow_run_workspace(seq, result);
            }
            LoopEvent::WorkflowRunNodeResult { seq, key, result } => {
                self.on_workflow_run_node_result(seq, key, result);
            }
            LoopEvent::WorkflowRunArtifactChunk { seq, append, result } => {
                self.on_workflow_run_artifact_chunk(seq, append, result);
            }
            LoopEvent::WorkflowRunArtifactItems { seq, initial, result } => {
                self.on_workflow_run_artifact_items(seq, initial, result);
            }
        }
    }

    /// Initialize landed for a tab: the Desktop Presence identity is on the
    /// wire (clientInfo rides `initialize`, protocol::acp_client) — attach.
    fn on_connect_result(&mut self, tab: u64, result: Result<AcpInitializeResult, String>, now: u64) {
        // Collect the spawn inputs before the mutable tab borrow.
        let mut fresh_session: Option<(AcpConnection, String)> = None;
        // Dead-backend revival candidate (see the Err arm): the (instance,
        // cwd, session) the re-incubation would target. Decided after the
        // tab borrow — the discovery list read needs `&self`.
        let mut revive: Option<(String, String, Option<String>)> = None;
        // Batch H1 restore-on-attach: this tab is about to attach a session
        // that may have persisted pending drafts. Read the seed before the
        // mutable borrow; seed ONLY when the core holds no live queue for
        // the session (a reconnect keeps its in-memory queue — re-seeding
        // would duplicate every draft).
        let attach_sid = self.tab(tab).and_then(|t| {
            t.core.active_session_id.clone().or_else(|| t.target.clone())
        });
        // Batch T readout restore: a cold process holds no in-memory
        // snapshot for the session, so load_session below just blanked the
        // readouts — the persisted last-known values re-seed them (read
        // before the mutable tab borrow, like the queue seed below).
        let readout_seed = attach_sid
            .as_deref()
            .and_then(|sid| self.readouts.get(sid));
        let seed: Option<(String, Vec<PromptDraft>)> = attach_sid
            .filter(|sid| {
                self.tab(tab)
                    .map(|t| !t.core.pending_prompts.contains_key(sid))
                    .unwrap_or(false)
            })
            .and_then(|sid| {
                self.pending.queue(&sid).map(|q| (sid, q.to_vec()))
            });
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
                        Some(sid) => {
                            // The restored drafts ride the queue; the load's
                            // settle flushes them exactly like drafts queued
                            // before a reconnect (SessionCore::flush_pending).
                            if let Some((seed_sid, queue)) = &seed {
                                t.core
                                    .pending_prompts
                                    .insert(seed_sid.clone(), queue.clone());
                                traceln!(
                                    "[driver] tab {tab} restored {} persisted pending draft(s) for {seed_sid}",
                                    queue.len()
                                );
                            }
                            t.core.load_session(&sid);
                            // None-gated seeding: a warm in-memory snapshot
                            // already restored fresher readouts inside
                            // load_session, and the load result's usage
                            // still lands on top of whatever this painted.
                            if let Some(r) = readout_seed {
                                let core = &mut t.core;
                                if core.usage.is_none() {
                                    core.usage = r.usage;
                                }
                                if core.cache_hit.is_none() {
                                    core.cache_hit = r.cache_hit;
                                }
                                if core.turn_stats.is_none() {
                                    core.turn_stats = r.turn_stats;
                                }
                                traceln!(
                                    "[driver] tab {tab} restored persisted readouts for {sid}"
                                );
                            }
                        }
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
                    // Revival candidate: the tab holds a session target, this
                    // failure is not the first, and the cooldown allows
                    // another attempt. `ever_connected` is deliberately NOT
                    // required — a tab opened right before a hub restart has
                    // never connected, yet a hub restart that wipes the
                    // registry (a busy serve bridge can stay unregistered for
                    // minutes mid-turn) leaves it dialing a corpse id forever
                    // (observed 2026-10-21: a 2.5min "unknown instance"
                    // ladder until the user closed and reopened the tab).
                    // The instance-gone check (the actual trigger) runs after
                    // the borrow — a bridge still in discovery gets the plain
                    // retry ladder.
                    if t.reconnect_attempts >= TAB_REINCUBATE_ATTEMPTS
                        && t
                            .reincubated_at
                            .map(|at| now >= at + TAB_REINCUBATE_COOLDOWN_MS)
                            .unwrap_or(true)
                    {
                        let sid = t
                            .core
                            .active_session_id
                            .clone()
                            .or_else(|| t.target.clone());
                        if sid.is_some() {
                            revive = Some((t.instance_id.clone(), t.cwd.clone(), sid));
                        }
                    }
                }
            }
        }
        if let Some((instance_id, cwd, sid)) = revive {
            // The instance left discovery: retrying its dead id can never
            // succeed. Re-incubate instead — the desktop-stamped POST
            // (server ADR-0010 amendment) reuses the workspace's live serve
            // bridge or headless-spawns one, and TabReincubated re-points
            // the tab there. A discovery snapshot up to 15s stale only
            // delays this to the next retry. A live close-tombstone vetoes
            // the revive: the ladder can burn out right as the remote-close
            // broadcast lands, and a resume POST cannot be recalled —
            // resuming a retired session is the resurrection this gate
            // exists to prevent.
            let tombstoned = sid.as_ref().is_some_and(|sid| {
                self.closed_sessions
                    .get(&(instance_id.clone(), sid.clone()))
                    .is_some_and(|at| now.saturating_sub(*at) < CLOSE_TOMBSTONE_MS)
            });
            if !tombstoned && !self.instances.iter().any(|i| i.id == instance_id) {
                if let Some(t) = self.tab_mut(tab) {
                    t.reincubated_at = Some(now);
                }
                traceln!(
                    "[driver] tab {tab}: instance {instance_id} left discovery — re-incubating its backend"
                );
                self.spawn_reincubate(tab, cwd, sid);
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
                // The queued drafts belong to the previous hub's sessions
                // (TS switchServer/disconnectHub `savePending({})`).
                self.pending.reset();
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
                // Saving activates — the app dials a different hub next
                // round, so its drafts start clean (same TS rule).
                self.pending.reset();
                return Flow::Restart;
            }
            UiCommand::RemoveServer { id } => {
                let was_active = self.book.active_id.as_deref() == Some(id.as_str());
                self.book.remove(&id);
                if let Err(e) = self.book.save(&protocol::profiles::servers_path()) {
                    traceln!("[driver] server book save failed: {e}");
                }
                if was_active {
                    // Falls back to the local hub — a hub change all the
                    // same (TS deleteServer's `savePending({})`).
                    self.pending.reset();
                    return Flow::Restart;
                }
            }
            UiCommand::UpdateServer {
                id,
                name,
                hub_url,
                token,
            } => {
                // TS saveServer: replace by id without touching active_id;
                // only an edit of the ACTIVE server reconnects.
                let was_active = self.book.active_id.as_deref() == Some(id.as_str());
                if let Some(i) = self.book.servers.iter().position(|s| s.id == id) {
                    let hub_url = hub_url.trim().trim_end_matches('/').to_string();
                    self.book.servers[i] = protocol::hub::SavedServer {
                        id,
                        name: name.trim().to_string(),
                        hub_url,
                        token,
                    };
                    if let Err(e) = self.book.save(&protocol::profiles::servers_path()) {
                        traceln!("[driver] server book save failed: {e}");
                    }
                    if was_active {
                        // The live dial still points at the old URL/token —
                        // restart so the edit takes effect now (TS routes
                        // through switchServer for the same reason).
                        self.pending.reset();
                        return Flow::Restart;
                    }
                }
            }
            UiCommand::UpgradeHub => {
                if !self.hub_upgrade_busy
                    && matches!(
                        self.phase,
                        Phase::Discovering | Phase::Connected | Phase::Reconnecting
                    )
                {
                    self.hub_upgrade_busy = true;
                    self.hub_upgrade_note = None;
                    let hub = self.settings_client();
                    let tx = self.loop_tx.clone();
                    tokio::spawn(async move {
                        let result = hub.upgrade_hub().await;
                        let _ = tx.send(LoopEvent::HubUpgraded(result));
                    });
                }
            }
            UiCommand::SetUpdateChannel { channel } => {
                if self.update_channel != channel {
                    crate::ui_prefs::store_update_channel(&channel);
                    self.update_channel = channel;
                    // TS configSlice `setUpdateChannel`: the open page
                    // refetches so it never shows the other stream's verdict
                    // (and an install never pairs stable's version with
                    // `channel: preview`, which the route refuses).
                    if self.sections[ConfigPageKind::AppUpdate.index()].open {
                        self.spawn_config_section_load(ConfigPageKind::AppUpdate);
                    }
                }
            }
            UiCommand::InstallAppUpdate {
                version,
                url,
                channel,
            } => {
                let panel = &mut self.sections[ConfigPageKind::AppUpdate.index()];
                if !panel.busy
                    && matches!(
                        self.phase,
                        Phase::Discovering | Phase::Connected | Phase::Reconnecting
                    )
                {
                    panel.busy = true;
                    // A fresh install clears the last outcome so the poll's
                    // in-flight progress (the payload's `install` block)
                    // paints instead of a stale verdict.
                    panel.install = None;
                    self.next_app_update_poll_at = now + APP_UPDATE_POLL_MS;
                    let hub = self.settings_client();
                    let tx = self.loop_tx.clone();
                    tokio::spawn(async move {
                        let result = hub
                            .install_app_update(&version, &url, Some(&channel))
                            .await
                            .map(|_| ());
                        let _ = tx.send(LoopEvent::AppUpdateInstalled(result));
                    });
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
            UiCommand::CancelOwnTurns => {
                for t in &self.tabs {
                    if let (true, Some(sid), Some(conn)) = (
                        &t.core.local_prompt_active,
                        &t.core.active_session_id,
                        &t.conn,
                    ) {
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
            UiCommand::MoveTab { id, before_id } => {
                // A drop on the dragged tab's own slot is a no-op; without
                // the guard the target lookup would fail after the remove
                // and the tab would fall through to the END slot.
                if before_id == Some(id) {
                    return Flow::Continue;
                }
                if let Some(at) = self.tabs.iter().position(|t| t.id == id) {
                    let tab = self.tabs.remove(at);
                    match before_id.and_then(|b| self.tabs.iter().position(|t| t.id == b)) {
                        Some(to) => self.tabs.insert(to, tab),
                        None => self.tabs.push(tab),
                    }
                }
            }
            UiCommand::CloseOtherTabs { id } => {
                // A vanished anchor (closed between the render that built the
                // menu and the click) must not close the whole strip.
                if self.tabs.iter().any(|t| t.id == id) {
                    let was_active = self.active_tab;
                    let others: Vec<u64> =
                        self.tabs.iter().filter(|t| t.id != id).map(|t| t.id).collect();
                    for other in others {
                        self.close_tab(other);
                    }
                    self.refocus_batch_close(id, was_active);
                }
            }
            UiCommand::CloseRightTabs { id } => {
                let was_active = self.active_tab;
                let right: Vec<u64> = match self.tabs.iter().position(|t| t.id == id) {
                    Some(at) => self.tabs[at + 1..].iter().map(|t| t.id).collect(),
                    None => Vec::new(),
                };
                for rid in right {
                    self.close_tab(rid);
                }
                self.refocus_batch_close(id, was_active);
            }
            UiCommand::OpenSession {
                instance_id,
                session_id,
            } => {
                // A tab already holding this session just activates
                // (appStore openSession's same-session branch).
                self.open_session_by_id(session_id, instance_id);
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
            UiCommand::RenameSession {
                instance_id,
                session_id,
                title,
            } => {
                // appStore renameSession: trim, cap at 80, no-op when empty.
                // The cap counts chars (TS slices UTF-16 code units); both
                // spell "a title line", never a wire-format constraint.
                let title = title.trim().chars().take(80).collect::<String>();
                if title.is_empty() {
                    return Flow::Continue;
                }
                self.spawn_session_rename(instance_id, session_id, title);
            }
            UiCommand::CloseSession {
                instance_id,
                session_id,
            } => {
                self.spawn_session_close(instance_id, session_id);
            }
            UiCommand::DeleteHistorySession { session_id } => {
                let Some(sel) = self.history.selected.as_mut() else {
                    return Flow::Continue;
                };
                // One delete at a time, and only once the first page named
                // the serving instance (the TS `instanceId` guard).
                if sel.deleting.is_some() || sel.instance_id.is_empty() {
                    return Flow::Continue;
                }
                sel.deleting = Some(session_id.clone());
                sel.error = None;
                // The tombstoned session's persisted readouts go with it —
                // a deleted id never re-attaches, but a stale file entry
                // outlives the session it described (batch T).
                self.readouts.remove(&session_id);
                if let Err(e) = self.readouts.save(&readouts_path()) {
                    traceln!("[driver] readout store write failed: {e}");
                }
                let instance = sel.instance_id.clone();
                self.spawn_history_delete(instance, session_id);
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
                // Cross-client pending-restart probe (see
                // maybe_spawn_pending_restart_check).
                self.maybe_spawn_pending_restart_check();
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
            UiCommand::RemoveModel {
                provider_id,
                model_id,
            } => {
                if !self.models.busy {
                    self.models.busy = true;
                    let hub = self.settings_client();
                    let tx = self.loop_tx.clone();
                    tokio::spawn(async move {
                        let result = hub.remove_model(&provider_id, &model_id).await;
                        let _ = tx.send(LoopEvent::ModelUpsert {
                            adding: false,
                            result,
                        });
                    });
                }
            }
            UiCommand::UpdateProvider {
                provider_id,
                enabled,
                provider_name,
                base_url,
                api_type,
                api_key,
            } => {
                if !self.models.busy {
                    self.models.busy = true;
                    let hub = self.settings_client();
                    let tx = self.loop_tx.clone();
                    tokio::spawn(async move {
                        let mut body = serde_json::Map::new();
                        if let Some(v) = enabled {
                            body.insert("enabled".into(), serde_json::json!(v));
                        }
                        if let Some(v) = provider_name {
                            body.insert("providerName".into(), serde_json::json!(v));
                        }
                        if let Some(v) = base_url {
                            body.insert("baseUrl".into(), serde_json::json!(v));
                        }
                        if let Some(v) = api_type {
                            body.insert("apiType".into(), serde_json::json!(v));
                        }
                        if let Some(v) = api_key {
                            body.insert("apiKey".into(), serde_json::json!(v));
                        }
                        let result = hub
                            .update_provider(&provider_id, serde_json::Value::Object(body))
                            .await;
                        let _ = tx.send(LoopEvent::ModelUpsert {
                            adding: false,
                            result,
                        });
                    });
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
            // ---- ZCode config sections (batch I) ----
            UiCommand::OpenConfigPage { page } => {
                self.sections[page.index()].open = true;
                // The TS mount pair, on EVERY open (not only the first): a
                // reopened page refreshes over its surviving payload.
                if self.sections[page.index()].supported.is_none() {
                    self.spawn_config_section_probe(page);
                }
                self.spawn_config_section_load(page);
                // The Agents page's picker reads the models section (TS
                // AgentsPage's `loadConfigSection("models")` effect); the
                // entry screen's snapshot may not have carried it.
                if page == ConfigPageKind::Agents && self.models.models.is_none() {
                    self.spawn_models_load();
                }
                // Cross-client pending-restart probe (see
                // maybe_spawn_pending_restart_check).
                self.maybe_spawn_pending_restart_check();
            }
            UiCommand::CloseConfigPage { page } => {
                self.sections[page.index()].open = false;
            }
            UiCommand::RefreshConfigPage { page } => {
                self.spawn_config_section_load(page);
            }
            UiCommand::SetSkillEnabled { path, enable } => {
                // One write at a time per page (the switches disable while
                // busy; this is the same guard on the driver side).
                if !self.sections[ConfigPageKind::Skills.index()].busy {
                    let hub = self.settings_client();
                    let label: &'static str =
                        if enable { "enable skill" } else { "disable skill" };
                    self.spawn_config_write(
                        ConfigPageKind::Skills,
                        label,
                        &[ConfigPageKind::Skills],
                        async move { hub.set_skill_enabled(&path, enable).await },
                    );
                }
            }
            UiCommand::CopySkillToUser { path } => {
                if !self.sections[ConfigPageKind::Skills.index()].busy {
                    let hub = self.settings_client();
                    self.spawn_config_write(
                        ConfigPageKind::Skills,
                        "copy skill",
                        &[ConfigPageKind::Skills],
                        async move { hub.copy_skill_to_user(&path).await },
                    );
                }
            }
            UiCommand::DeleteSkill { path } => {
                if !self.sections[ConfigPageKind::Skills.index()].busy {
                    let hub = self.settings_client();
                    self.spawn_config_write(
                        ConfigPageKind::Skills,
                        "delete skill",
                        &[ConfigPageKind::Skills],
                        async move { hub.delete_skill(&path).await },
                    );
                }
            }
            UiCommand::UpsertMcpServer { adding, name, body } => {
                if !self.sections[ConfigPageKind::Mcp.index()].busy {
                    let hub = self.settings_client();
                    let label: &'static str =
                        if adding { "add MCP server" } else { "update MCP server" };
                    self.spawn_config_write(
                        ConfigPageKind::Mcp,
                        label,
                        &[ConfigPageKind::Mcp],
                        async move { hub.upsert_mcp_server(&name, body).await },
                    );
                }
            }
            UiCommand::SetMcpEnabled { name, enable } => {
                if !self.sections[ConfigPageKind::Mcp.index()].busy {
                    let hub = self.settings_client();
                    let label: &'static str =
                        if enable { "enable MCP server" } else { "disable MCP server" };
                    self.spawn_config_write(
                        ConfigPageKind::Mcp,
                        label,
                        &[ConfigPageKind::Mcp],
                        async move { hub.set_mcp_enabled(&name, enable).await },
                    );
                }
            }
            UiCommand::DeleteMcpServer { name } => {
                if !self.sections[ConfigPageKind::Mcp.index()].busy {
                    let hub = self.settings_client();
                    self.spawn_config_write(
                        ConfigPageKind::Mcp,
                        "delete MCP server",
                        &[ConfigPageKind::Mcp],
                        async move { hub.remove_mcp_server(&name).await },
                    );
                }
            }
            UiCommand::SetHooksEnabled { enabled } => {
                if !self.sections[ConfigPageKind::Hooks.index()].busy {
                    let hub = self.settings_client();
                    let label: &'static str =
                        if enabled { "enable hooks" } else { "disable hooks" };
                    self.spawn_config_write(
                        ConfigPageKind::Hooks,
                        label,
                        &[ConfigPageKind::Hooks],
                        async move { hub.set_hooks_enabled(enabled).await },
                    );
                }
            }
            UiCommand::UpdateHookEntry {
                event,
                matcher_index,
                hook_index,
                patch,
            } => {
                if !self.sections[ConfigPageKind::Hooks.index()].busy {
                    let hub = self.settings_client();
                    self.spawn_config_write(
                        ConfigPageKind::Hooks,
                        "update hook",
                        &[ConfigPageKind::Hooks],
                        async move {
                            hub.update_hook(&event, matcher_index, hook_index, patch)
                                .await
                        },
                    );
                }
            }
            UiCommand::SetAgentEnabled { name, enable } => {
                if !self.sections[ConfigPageKind::Agents.index()].busy {
                    let hub = self.settings_client();
                    let label: &'static str =
                        if enable { "enable subagent" } else { "disable subagent" };
                    self.spawn_config_write(
                        ConfigPageKind::Agents,
                        label,
                        &[ConfigPageKind::Agents],
                        async move { hub.set_agent_enabled(&name, enable).await },
                    );
                }
            }
            UiCommand::UpsertAgent { name, body } => {
                if !self.sections[ConfigPageKind::Agents.index()].busy {
                    let hub = self.settings_client();
                    self.spawn_config_write(
                        ConfigPageKind::Agents,
                        "update subagent",
                        &[ConfigPageKind::Agents],
                        async move { hub.upsert_agent(&name, body).await },
                    );
                }
            }
            UiCommand::DeleteAgent { name } => {
                if !self.sections[ConfigPageKind::Agents.index()].busy {
                    let hub = self.settings_client();
                    self.spawn_config_write(
                        ConfigPageKind::Agents,
                        "delete subagent",
                        &[ConfigPageKind::Agents],
                        async move { hub.delete_agent(&name).await },
                    );
                }
            }
            UiCommand::SetBuiltInAgentModel { name, selection } => {
                if !self.sections[ConfigPageKind::Agents.index()].busy {
                    let hub = self.settings_client();
                    // Clearing sends providerId+modelId as null TOGETHER — the
                    // route's documented clear shape, and the only way to
                    // restore inheritance.
                    let body = match selection {
                        Some(s) => protocol::settings::AgentUpsert {
                            provider_id: Some(Some(s.provider_id)),
                            model_id: Some(Some(s.model_id)),
                            reasoning_level: s.reasoning_level,
                            ..Default::default()
                        },
                        None => protocol::settings::AgentUpsert {
                            provider_id: Some(None),
                            model_id: Some(None),
                            ..Default::default()
                        },
                    };
                    self.spawn_config_write(
                        ConfigPageKind::Agents,
                        "set built-in agent model",
                        &[ConfigPageKind::Agents],
                        async move { hub.upsert_agent(&name, body).await },
                    );
                }
            }
            UiCommand::TestModelConnectivity {
                provider_id,
                model_id,
            } => {
                let hub = self.settings_client();
                let tx = self.loop_tx.clone();
                self.model_test_pending = Some((provider_id.clone(), model_id.clone()));
                tokio::spawn(async move {
                    let result = hub.test_model_connectivity(&provider_id, &model_id).await;
                    let _ = tx.send(LoopEvent::ModelTestSettled {
                        provider_id,
                        model_id,
                        result,
                    });
                });
            }
            UiCommand::RestoreBackup { file, path } => {
                if !self.sections[ConfigPageKind::Backups.index()].busy {
                    let hub = self.settings_client();
                    self.spawn_config_write(
                        ConfigPageKind::Backups,
                        "restore backup",
                        &[
                            ConfigPageKind::Skills,
                            ConfigPageKind::Mcp,
                            ConfigPageKind::Hooks,
                            ConfigPageKind::Agents,
                        ],
                        async move { hub.restore_backup(&file, &path).await },
                    );
                }
            }
            // ---- Workflows page + gate (batch J) ----
            UiCommand::OpenWorkflows => {
                self.workflows.open = true;
                // The TS mount pair: the overview read on EVERY open (the
                // poll re-arms from its answer) plus the gate probe while
                // its verdict is unknown.
                let now = self.clock.now_ms();
                self.spawn_workflow_overview(now);
                if self.workflows.gate.is_none() {
                    self.spawn_workflow_gate_probe();
                }
            }
            UiCommand::CloseWorkflows => {
                self.workflows.open = false;
                self.workflows.detail = None;
                self.workflows.run = None;
                self.workflows.workflow_seq += 1;
            }
            UiCommand::RefreshWorkflows => {
                let now = self.clock.now_ms();
                self.spawn_workflow_overview(now);
            }
            UiCommand::OpenWorkflowDetail {
                scope,
                name,
                instance_id,
            } => {
                self.workflows.workflow_seq += 1;
                self.workflows.detail = Some(WorkflowDetailPanel {
                    seq: self.workflows.workflow_seq,
                    scope,
                    name,
                    instance_id,
                    loading: false,
                    detail: None,
                    runs: None,
                    summaries: HashMap::new(),
                    busy: false,
                    next_runs_poll_at: 0,
                });
                self.spawn_workflow_detail_loads();
            }
            UiCommand::CloseWorkflowDetail => {
                self.workflows.detail = None;
                self.workflows.workflow_seq += 1;
                // The overview re-reads on back (TS WorkflowDetail's
                // onBack): badges follow whatever the run just did.
                let now = self.clock.now_ms();
                self.spawn_workflow_overview(now);
            }
            UiCommand::StartWorkflow {
                scope,
                name,
                instance_id,
                args,
            } => {
                if !self.workflows.busy {
                    self.workflows.busy = true;
                    let hub = self.settings_client();
                    let tx = self.loop_tx.clone();
                    tokio::spawn(async move {
                        let result = hub
                            .workflow_start(&instance_id, scope, &name, args, None)
                            .await;
                        let _ = tx.send(LoopEvent::WorkflowStarted {
                            scope,
                            name,
                            instance_id,
                            result,
                        });
                    });
                }
            }
            UiCommand::WorkflowPromoteGlobal { name } => {
                // Promote-to-global rides the chat-prefill path (the
                // desktop's launcher does exactly this — no dedicated
                // REST); the prompt mirrors the TS app's wording.
                let prompt = format!(
                    "Please promote the saved workflow \"{name}\" from this project's scope to \
                     the global scope: move the file under ~/.zcode/workflows/ keeping its \
                     script, description, whenToUse and args declaration unchanged, then remove \
                     the project copy. If it is inherently bound to this project and cannot be \
                     generalized meaningfully, say why and stop without saving."
                );
                self.workflows.open = false;
                self.workflows.detail = None;
                if self.active_tab.is_none() {
                    let now = self.clock.now_ms();
                    self.toast.notify(
                        now,
                        "prompt staged — open a session to review and send it",
                    );
                }
                let _ = self.ev_tx.send(RuntimeEvent::ComposerPrefill(PromptDraft {
                    text: prompt,
                    images: Vec::new(),
                }));
            }
            UiCommand::UpdateWorkflowMeta {
                scope,
                name,
                instance_id,
                meta,
            } => {
                // Only the OPEN page's write rides (a stale form's submit
                // after a back must not fire).
                if let Some(d) = &self.workflows.detail {
                    if !d.busy {
                        let seq = d.seq;
                        self.workflows.detail.as_mut().unwrap().busy = true;
                        let hub = self.settings_client();
                        let tx = self.loop_tx.clone();
                        tokio::spawn(async move {
                            let result =
                                hub.workflow_update_meta(&instance_id, scope, &name, meta).await;
                            let _ = tx.send(LoopEvent::WorkflowMetaUpdated { seq, result });
                        });
                    }
                }
            }
            UiCommand::MoveWorkflowToProject {
                scope,
                name,
                instance_id,
            } => {
                // The button lives on the open detail page — no page, no
                // write (and the seq guard below drops late answers).
                if let Some(d) = &self.workflows.detail {
                    if !d.busy && !self.workflows.busy {
                        self.workflows.busy = true;
                        let seq = d.seq;
                        let hub = self.settings_client();
                        let tx = self.loop_tx.clone();
                        tokio::spawn(async move {
                            let result = hub.workflow_move(&instance_id, scope, &name).await;
                            let _ = tx.send(LoopEvent::WorkflowMoved { seq, result });
                        });
                    }
                }
            }
            UiCommand::DeleteWorkflow {
                scope,
                name,
                instance_id,
            } => {
                if let Some(d) = &self.workflows.detail {
                    if !d.busy && !self.workflows.busy {
                        self.workflows.busy = true;
                        let seq = d.seq;
                        let hub = self.settings_client();
                        let tx = self.loop_tx.clone();
                        tokio::spawn(async move {
                            let result = hub.workflow_delete(&instance_id, scope, &name).await;
                            let _ = tx.send(LoopEvent::WorkflowDeleted { seq, result });
                        });
                    }
                }
            }
            UiCommand::WorkflowCreateViaChat { instance_id } => {
                if !self.workflows.busy {
                    self.workflows.busy = true;
                    let hub = self.settings_client();
                    let tx = self.loop_tx.clone();
                    tokio::spawn(async move {
                        let result = hub.workflow_create_prompt(&instance_id, None).await;
                        let _ = tx.send(LoopEvent::WorkflowCreatePrompt(result));
                    });
                }
            }
            UiCommand::WorkflowResumeRun {
                run_id,
                session_id,
                name,
                instance_id,
            } => {
                if !self.workflows.busy {
                    self.workflows.busy = true;
                    let hub = self.settings_client();
                    let tx = self.loop_tx.clone();
                    tokio::spawn(async move {
                        let result = hub.workflow_resume(&instance_id, &run_id, &session_id, name.as_deref()).await;
                        let _ = tx.send(LoopEvent::WorkflowRunResumed {
                            session_id,
                            instance_id,
                            result,
                        });
                    });
                }
            }
            UiCommand::WorkflowStopRun {
                run_id,
                session_id,
                instance_id,
            } => {
                if !self.workflows.busy {
                    self.workflows.busy = true;
                    let hub = self.settings_client();
                    let tx = self.loop_tx.clone();
                    // The fallback rung: the CONNECTED tab's socket — the
                    // ACP `session/cancelBackgroundTask` route only reaches
                    // the bridge the socket is dialled into (TS `acp`).
                    let acp = self
                        .active_tab
                        .and_then(|id| self.tab(id))
                        .filter(|t| t.state == TabPhase::Connected)
                        .and_then(|t| t.conn.clone());
                    tokio::spawn(async move {
                        // Hub first (reaches ANY instance); `None` skips the
                        // rung entirely. A failed hub stop falls through to
                        // the ACP socket rather than reporting defeat —
                        // EXCEPT a 409 (already settled): nothing left to
                        // cancel, and the settle refresh below takes over.
                        let mut result = match instance_id.as_deref() {
                            Some(iid) if !iid.is_empty() => {
                                hub.workflow_stop(iid, &run_id, &session_id).await
                            }
                            _ => Err(SettingsApiError::Network(
                                "no instance owns this run".to_string(),
                            )),
                        };
                        let settled = matches!(
                            result,
                            Err(ref e) if e.status() == Some(409)
                        );
                        if result.is_err() && !settled {
                            if let Some(conn) = acp {
                                let params = serde_json::json!({
                                    "sessionId": session_id,
                                    "taskId": run_id,
                                });
                                match conn.request("session/cancelBackgroundTask", params).await {
                                    // TS reads `cancelled === true` as the
                                    // only success shape; anything else
                                    // keeps the hub error (or names it).
                                    Ok(v) if v.get("cancelled") == Some(&Value::Bool(true)) => {
                                        result = Ok(v);
                                    }
                                    Ok(_) => {
                                        if result.is_err() {
                                            result = Err(SettingsApiError::Unexpected(
                                                "run not found or already settled".to_string(),
                                            ));
                                        }
                                    }
                                    Err(e) => {
                                        if result.is_err() {
                                            result = Err(SettingsApiError::Unexpected(format!(
                                                "acp cancel: {e}"
                                            )));
                                        }
                                    }
                                }
                            }
                        }
                        let _ = tx.send(LoopEvent::WorkflowRunStopped(result));
                    });
                }
            }
            UiCommand::DismissWorkflowRun {
                instance_id,
                run_id,
            } => {
                if !self.workflows.busy {
                    self.workflows.busy = true;
                    let hub = self.settings_client();
                    let tx = self.loop_tx.clone();
                    tokio::spawn(async move {
                        let result = hub.dismiss_workflow_run(&instance_id, &run_id).await;
                        let _ = tx.send(LoopEvent::WorkflowRunDismissed { run_id, result });
                    });
                }
            }
            UiCommand::DismissWorkflowRuns {
                instance_id,
                run_ids,
            } => {
                if !self.workflows.busy {
                    self.workflows.busy = true;
                    let hub = self.settings_client();
                    let tx = self.loop_tx.clone();
                    tokio::spawn(async move {
                        let result = hub.dismiss_workflow_runs(&instance_id, &run_ids).await;
                        let _ = tx.send(LoopEvent::WorkflowRunsDismissed(result));
                    });
                }
            }
            UiCommand::OpenWorkflowRun {
                instance_id,
                session_id,
                run_id,
                name,
            } => {
                self.workflows.workflow_seq += 1;
                self.workflows.run = Some(WorkflowRunPanel {
                    seq: self.workflows.workflow_seq,
                    instance_id,
                    session_id,
                    run_id,
                    name,
                    // The first poll tick comes one interval after the mount
                    // (TS setInterval) — the open's own head read covers now.
                    next_poll_at: self.clock.now_ms() + WORKFLOW_RUN_POLL_MS,
                    ..WorkflowRunPanel::default()
                });
                self.spawn_workflow_run_reads(true);
            }
            UiCommand::WorkflowRunLoadMore => {
                if self
                    .workflows
                    .run
                    .as_ref()
                    .is_some_and(|r| r.has_more && !r.events_busy)
                {
                    self.spawn_workflow_run_reads(false);
                }
            }
            UiCommand::WorkflowRunRetryEvents => {
                if self
                    .workflows
                    .run
                    .as_ref()
                    .is_some_and(|r| r.events.is_none() && !r.events_busy)
                {
                    self.spawn_workflow_run_reads(true);
                }
            }
            UiCommand::WorkflowRunSelectTab { tab } => {
                if let Some(r) = self.workflows.run.as_mut() {
                    r.tab = tab;
                }
                match tab {
                    WorkflowRunTab::Events => {}
                    WorkflowRunTab::Artifacts => self.spawn_workflow_run_artifacts(),
                    WorkflowRunTab::Workspace => self.spawn_workflow_run_workspace(),
                }
            }
            UiCommand::WorkflowRunToggleNode {
                key,
                site_id,
                ordinal,
            } => {
                let open = self
                    .workflows
                    .run
                    .as_ref()
                    .and_then(|r| r.node_result.as_ref())
                    .is_some_and(|n| n.key == key);
                if open {
                    // Tap again collapses (TS `loadNode`'s first branch).
                    if let Some(r) = self.workflows.run.as_mut() {
                        r.node_result = None;
                    }
                } else {
                    self.spawn_workflow_run_node(key, site_id, ordinal);
                }
            }
            UiCommand::WorkflowRunOpenArtifact { artifact_id } => {
                self.open_workflow_artifact(&artifact_id);
            }
            UiCommand::WorkflowRunCloseArtifact => {
                if let Some(r) = self.workflows.run.as_mut() {
                    r.artifact = None;
                }
            }
            UiCommand::WorkflowRunArtifactLoadMore => self.workflow_artifact_load_more(),
            UiCommand::CloseWorkflowRun => {
                self.workflows.run = None;
                self.workflows.workflow_seq += 1;
            }
            UiCommand::AmendWorkflowRun {
                instance_id,
                session_id,
                run_id,
                body,
            } => {
                if self.workflows.run.as_ref().is_some_and(|r| !r.amend_busy) {
                    let seq = self.workflows.run.as_ref().unwrap().seq;
                    self.workflows.run.as_mut().unwrap().amend_busy = true;
                    let hub = self.settings_client();
                    let tx = self.loop_tx.clone();
                    tokio::spawn(async move {
                        let result =
                            hub.amend_run_settings(&instance_id, &run_id, &session_id, body).await;
                        let _ = tx.send(LoopEvent::WorkflowRunAmended { seq, result });
                    });
                }
            }
            UiCommand::SetWorkflowGate { instance_id, mode } => {
                if !self.workflows.gate_busy {
                    self.workflows.gate_busy = true;
                    let hub = self.settings_client();
                    let tx = self.loop_tx.clone();
                    tokio::spawn(async move {
                        let result = hub.workflow_gate_set(&instance_id, mode).await;
                        let _ = tx.send(LoopEvent::WorkflowGateSet(result));
                    });
                }
            }
            UiCommand::ProbeWorkflowGate => {
                if self.workflows.gate.is_none() {
                    self.spawn_workflow_gate_probe();
                }
            }
        }
        Flow::Continue
    }

    // ---- snapshot ----

    /// Distinct sessions whose Session Activity reads `running` across all
    /// tabs, plus each tab's optimistic local turn — the status bar's
    /// number. The quit dialog gates on the NARROWER own-turn count
    /// instead (see [`WorkbenchSnapshot::own_running_session_count`]).
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
        // The just-finished join (TS SessionList's finishedAt rung) and the
        // "has any broadcast activity" marker (the restRunning fallback only
        // paints rows with NO entry at all — a known-idle broadcast wins).
        let mut finished_at: HashMap<&str, u64> = HashMap::new();
        let mut activity_known: HashSet<&str> = HashSet::new();
        for tab in &self.tabs {
            for (sid, act) in &tab.core.session_states {
                activity_known.insert(sid.as_str());
                if let Some(at) = act.finished_at {
                    finished_at.insert(sid.as_str(), at);
                }
            }
        }
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
                    finished_at: finished_at.get(sess.session_id.as_str()).copied(),
                    rest_running: sess.status.as_deref() == Some("running"),
                    activity_known: activity_known.contains(sess.session_id.as_str()),
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
            available_commands: tab.core.available_commands.clone(),
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
            // Titles repaint on rename (batch G-light) — length only, the
            // cheap idiom used everywhere else in this signature.
            mix(row.title.clone().map(|t| t.len() as u64).unwrap_or(u64::MAX), &mut sig);
            mix(row.updated_at.unwrap_or(0), &mut sig);
            mix(row.running as u64, &mut sig);
            mix(row.awaiting as u64, &mut sig);
            mix(row.finished_at.unwrap_or(0), &mut sig);
            mix(row.rest_running as u64, &mut sig);
            mix(row.activity_known as u64, &mut sig);
            mix(row.open_tab.unwrap_or(0), &mut sig);
        }
        for tab in &self.tabs {
            mix(tab.id, &mut sig);
            // The tab label's project half (TabSummary.workspace); length
            // only, the cheap idiom used everywhere else in this signature.
            mix(tab.cwd.len() as u64, &mut sig);
            mix(messages_signature(&tab.core.messages), &mut sig);
            mix(tab.core.is_running as u64, &mut sig);
            // The quit dialog's own-task count rides this flag — a prompt
            // settling or starting must re-emit.
            mix(tab.core.local_prompt_active as u64, &mut sig);
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
            // Slash commands (batch K): the composer's "/" menu contents —
            // an overwrite landing must re-emit even between empty states.
            mix(tab.core.available_commands.len() as u64, &mut sig);
            for cmd in &tab.core.available_commands {
                mix(cmd.name.len() as u64, &mut sig);
                mix(
                    cmd.description.as_ref().map(|d| d.len() as u64).unwrap_or(u64::MAX),
                    &mut sig,
                );
            }
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
        // The gate sheet's write target and its need-instance warning repaint
        // when the active tab (and with it the active instance) changes.
        mix(
            self.active_instance_id().map(|i| i.len() as u64).unwrap_or(u64::MAX),
            &mut sig,
        );
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
            mix(sel.deleting.is_some() as u64, &mut sig);
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
        // The five ZCode-config section pages (batch I): open/close, the
        // state flags, and a content signature of each payload.
        for page in ConfigPageKind::ALL {
            let panel = &self.sections[page.index()];
            mix(panel.open as u64, &mut sig);
            mix(
                panel.supported.map(|s| s as u64).unwrap_or(u64::MAX),
                &mut sig,
            );
            mix(panel.loading as u64, &mut sig);
            mix(panel.busy as u64, &mut sig);
            mix(panel.error.is_some() as u64, &mut sig);
            mix(
                panel
                    .payload
                    .as_ref()
                    .map(|p| fold_str(&p.to_string()))
                    .unwrap_or(u64::MAX),
                &mut sig,
            );
            // App Update's install block (progress bytes + stage moves).
            mix(
                panel
                    .install
                    .as_ref()
                    .map(|i| fold_str(&i.to_string()))
                    .unwrap_or(u64::MAX),
                &mut sig,
            );
        }
        // App Update's channel + the settings drawer's hub-upgrade check.
        mix(fold_str(&self.update_channel), &mut sig);
        // The status bar's quota dock line (ADR-0021).
        mix(
            self.quota_dock.as_ref().map(|n| fold_str(n)).unwrap_or(u64::MAX),
            &mut sig,
        );
        mix(self.hub_upgrade_busy as u64, &mut sig);
        mix(
            self.hub_upgrade_note
                .as_ref()
                .map(|n| fold_str(n))
                .unwrap_or(u64::MAX),
            &mut sig,
        );
        // The Workflows page + gate (batch J): open/close, the state flags,
        // the gate verdict's identity (position + mode), and content
        // signatures of the overview / detail / run payloads — every field
        // the page paints.
        let wf = &self.workflows;
        mix(wf.open as u64, &mut sig);
        mix(wf.loading as u64, &mut sig);
        mix(wf.error.is_some() as u64, &mut sig);
        mix(wf.busy as u64, &mut sig);
        mix(wf.workflow_seq, &mut sig);
        mix(
            wf.overview
                .as_ref()
                .map(workflow_overview_signature)
                .unwrap_or(u64::MAX),
            &mut sig,
        );
        mix(
            wf.gate
                .as_ref()
                .map(|g| {
                    fold_str(&g.source)
                        ^ fold_str(&g.mode)
                        ^ (g.enabled as u64).rotate_left(8)
                        ^ g.r#override.map(|o| o as u64).unwrap_or(7).rotate_left(16)
                })
                .unwrap_or(u64::MAX),
            &mut sig,
        );
        mix(wf.gate_busy as u64, &mut sig);
        // The launch-memory joins (batch M): a freshly remembered launch
        // makes rows actionable — the paint must follow the table. The fold
        // is XOR-commutative on purpose: a HashMap's iteration order is
        // arbitrary, and a multiplicative fold would jitter the signature
        // (spurious repaints) across identical maps.
        {
            let joins = self.workflow_launch_joins();
            mix(joins.len() as u64, &mut sig);
            let mut s: u64 = 0;
            for (run_id, l) in &joins {
                s ^= fold_str(run_id)
                    ^ fold_str(&l.instance_id).rotate_left(8)
                    ^ fold_str(&l.acp_session_id).rotate_left(16);
            }
            mix(s, &mut sig);
        }
        // The third ladder's map: a fresh conversationRuns answer must
        // repaint the active rows it just made actionable. Same
        // XOR-commutative fold as the launch joins (HashMap order is
        // arbitrary).
        {
            mix(self.workflows.session_runs.len() as u64, &mut sig);
            let mut s: u64 = 0;
            for (run_id, sid) in &self.workflows.session_runs {
                s ^= fold_str(run_id) ^ fold_str(sid).rotate_left(8);
            }
            mix(s, &mut sig);
        }
        if let Some(d) = &wf.detail {
            mix(d.seq, &mut sig);
            mix(fold_str(&d.name), &mut sig);
            mix((d.scope == WorkflowScope::Global) as u64, &mut sig);
            mix(fold_str(&d.instance_id), &mut sig);
            mix(d.loading as u64, &mut sig);
            mix(d.busy as u64, &mut sig);
            mix(
                d.detail
                    .as_ref()
                    .map(|x| fold_str(&serde_json::to_string(x).unwrap_or_default()))
                    .unwrap_or(u64::MAX),
                &mut sig,
            );
            mix(
                d.runs
                    .as_ref()
                    .map(|rows| {
                        let mut s: u64 = rows.len() as u64;
                        for r in rows {
                            s = s.wrapping_mul(31).wrapping_add(fold_str(&r.run_id));
                            s = s.wrapping_mul(31).wrapping_add(fold_str(&r.status));
                            s = s.wrapping_mul(31).wrapping_add(r.updated_at.unwrap_or(0));
                        }
                        s
                    })
                    .unwrap_or(u64::MAX),
                &mut sig,
            );
            for (sid, list) in &d.summaries {
                mix(fold_str(sid), &mut sig);
                for sum in list {
                    mix(fold_str(&sum.run_id) ^ (sum.resumable as u64).rotate_left(20), &mut sig);
                }
            }
        }
        if let Some(r) = &wf.run {
            mix(r.seq, &mut sig);
            mix(fold_str(&r.run_id), &mut sig);
            mix(fold_str(&r.session_id), &mut sig);
            mix(r.events_busy as u64, &mut sig);
            mix(r.has_more as u64, &mut sig);
            mix(r.session_dead as u64, &mut sig);
            mix(r.amend_busy as u64, &mut sig);
            mix(
                r.summary
                    .as_ref()
                    .map(|s| fold_str(&s.status) ^ s.updated_at.unwrap_or(0).rotate_left(12))
                    .unwrap_or(u64::MAX),
                &mut sig,
            );
            mix(
                r.events
                    .as_ref()
                    .map(|events| {
                        let mut s: u64 = events.len() as u64;
                        for e in events {
                            s = s.wrapping_mul(31).wrapping_add(e.sequence);
                        }
                        s
                    })
                    .unwrap_or(u64::MAX),
                &mut sig,
            );
            // The lazy tabs + viewer (batch L): every painted field.
            mix(r.tab as u64, &mut sig);
            mix(r.artifacts_busy as u64, &mut sig);
            mix(
                r.artifacts
                    .as_ref()
                    .map(|list| {
                        let mut s: u64 = list.len() as u64;
                        for a in list {
                            s = s.wrapping_mul(31).wrapping_add(fold_str(&a.id));
                            s = s.wrapping_mul(31).wrapping_add(
                                a.versions.as_ref().map(|v| v.len() as u64).unwrap_or(0),
                            );
                            s = s.wrapping_mul(31).wrapping_add(a.item_count.unwrap_or(0));
                        }
                        s
                    })
                    .unwrap_or(u64::MAX),
                &mut sig,
            );
            mix(r.nodes_busy as u64, &mut sig);
            mix(
                r.nodes
                    .as_ref()
                    .map(|list| {
                        let mut s: u64 = list.len() as u64;
                        for n in list {
                            s = s.wrapping_mul(31).wrapping_add(fold_str(
                                n.status.as_deref().unwrap_or(""),
                            ));
                            s = s.wrapping_mul(31).wrapping_add(n.updated_at.unwrap_or(0));
                        }
                        s
                    })
                    .unwrap_or(u64::MAX),
                &mut sig,
            );
            mix(
                r.node_busy.as_deref().map(fold_str).unwrap_or(u64::MAX),
                &mut sig,
            );
            mix(
                r.node_result
                    .as_ref()
                    .map(|n| fold_str(&n.key) ^ fold_str(&n.body.to_string()).rotate_left(7))
                    .unwrap_or(u64::MAX),
                &mut sig,
            );
            if let Some(v) = &r.artifact {
                mix(v.seq, &mut sig);
                mix(v.busy as u64, &mut sig);
                mix(v.binary as u64, &mut sig);
                mix(v.text.as_ref().map(|t| t.len() as u64).unwrap_or(u64::MAX), &mut sig);
                mix(v.next_offset.unwrap_or(u64::MAX), &mut sig);
                mix(v.total_bytes.unwrap_or(u64::MAX), &mut sig);
                mix(v.items_has_more as u64, &mut sig);
                mix(
                    v.items
                        .as_ref()
                        .map(|items| {
                            let mut s: u64 = items.len() as u64;
                            for it in items {
                                s = s.wrapping_mul(31).wrapping_add(it.sequence);
                            }
                            s
                        })
                        .unwrap_or(u64::MAX),
                    &mut sig,
                );
            } else {
                mix(u64::MAX - 1, &mut sig);
            }
        }
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
        let own_running_session_count = self
            .tabs
            .iter()
            .filter(|t| t.core.local_prompt_active)
            .count();
        let awaiting = self.awaiting_sessions();
        let sessions = self.session_rows(&running, &awaiting);
        let tabs: Vec<TabSummary> = self
            .tabs
            .iter()
            .map(|t| TabSummary {
                id: t.id,
                session_id: t.core.active_session_id.clone().or_else(|| t.target.clone()),
                workspace: (!t.cwd.is_empty()).then(|| t.cwd.clone()),
                session_title: t.core.active_session_id.as_deref().and_then(|sid| {
                    sessions
                        .iter()
                        .find(|row| row.session_id == sid)
                        .and_then(|row| row.title.clone())
                        .filter(|title| !title.trim().is_empty())
                }),
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
                deleting: sel.deleting.clone(),
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
            own_running_session_count,
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
            quota_page: self.quota.open.then(|| QuotaPageView {                supported: self.quota.supported,
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
            quota_card: QuotaCardView {
                usage: self.quota.usage.clone(),
                quota_unavailable: self.quota.quota_unavailable,
                usage_stats_at: self.quota.usage_stats_at,
            },
            quota_dock: self.quota_dock.clone(),
            config_notice: self.config_notice.clone(),
            pending_restart: self.pending_restart,
            model_tests: self.model_tests.clone(),
            model_test_pending: self.model_test_pending.clone(),
            workflows_page: self.workflows.open.then(|| WorkflowsPageView {
                loading: self.workflows.loading,
                error: self.workflows.error.clone(),
                overview: self.workflows.overview.clone(),
                detail: self.workflows.detail.as_ref().map(|d| WorkflowDetailView {
                    scope: d.scope,
                    name: d.name.clone(),
                    instance_id: d.instance_id.clone(),
                    loading: d.loading,
                    detail: d.detail.clone(),
                    runs: d.runs.clone(),
                    summaries: d.summaries.clone(),
                }),
                run: self.workflows.run.as_ref().map(|r| {
                    let active = !r.session_dead
                        && r.summary
                            .as_ref()
                            .map(|s| s.status == "running" || s.status == "pending")
                            .unwrap_or(true);
                    WorkflowRunView {
                        instance_id: r.instance_id.clone(),
                        session_id: r.session_id.clone(),
                        run_id: r.run_id.clone(),
                        name: r.name.clone(),
                        summary: r.summary.clone(),
                        events: r.events.clone(),
                        events_busy: r.events_busy,
                        has_more: r.has_more,
                        session_dead: r.session_dead,
                        active,
                        amend_busy: r.amend_busy,
                        tab: r.tab,
                        artifacts: r.artifacts.clone(),
                        artifacts_busy: r.artifacts_busy,
                        nodes: r.nodes.clone(),
                        nodes_busy: r.nodes_busy,
                        node_result: r.node_result.clone(),
                        node_busy: r.node_busy.clone(),
                        artifact: r.artifact.as_ref().map(WorkflowArtifactViewer::view),
                    }
                }),
                busy: self.workflows.busy,
                action_instance: self.active_instance_id(),
                launches: self.workflow_launch_joins(),
                session_runs: self.workflows.session_runs.clone(),
            }),
            workflow_gate: self.workflows.gate.clone(),
            workflow_gate_busy: self.workflows.gate_busy,
            active_instance: self.active_instance_id(),
            files: self.files.view(),
            config_section_page: ConfigPageKind::ALL
                .iter()
                .find(|page| self.sections[page.index()].open)
                .map(|page| {
                    let panel = &self.sections[page.index()];
                    ConfigSectionPageView {
                        kind: *page,
                        supported: panel.supported,
                        loading: panel.loading,
                        error: panel.error.clone(),
                        payload: panel.payload.clone(),
                        busy: panel.busy,
                        // The Agents page's picker decodes the models
                        // payload; every other page leaves it None.
                        models: (*page == ConfigPageKind::Agents)
                            .then(|| self.models.models.clone())
                            .flatten(),
                        update_channel: self.update_channel.clone(),
                        install: panel.install.clone(),
                    }
                }),
            hub_upgrade_busy: self.hub_upgrade_busy,
            hub_upgrade_note: self.hub_upgrade_note.clone(),
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
    // The decoded raster's identity (batch K): the placeholder → image swap
    // must re-emit even with every other field unchanged.
    sig = sig
        .wrapping_mul(31)
        .wrapping_add(v.image.as_ref().map(|i| i.id).unwrap_or(u64::MAX));
    sig = sig.wrapping_mul(31).wrapping_add(v.loading as u64);
    sig = sig.wrapping_mul(31).wrapping_add(v.error.is_some() as u64);
    sig
}

/// Sniffs the fetched bytes and wraps them for `img()` — the file viewer's
/// decode step (batch K). Vector formats and unknown payloads return None
/// (the viewer shows its placeholder instead); gpui decodes lazily at paint.
fn decode_preview_image(bytes: &[u8]) -> Option<Image> {
    let mime = protocol::image::sniff_mime(bytes)?;
    // from_mime_type maps mimes gpui cannot rasterize too (svg) — exclude
    // the vector form explicitly, mirroring the attachment echo's rule.
    if mime == "image/svg+xml" {
        return None;
    }
    let format = ImageFormat::from_mime_type(mime)?;
    Some(Image::from_bytes(format, bytes.to_vec()))
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

/// Content signature of the workflow overview for the fingerprint: group
/// identity, the per-name last-run badges, and both run zones (row
/// identity, status, stamps — the fields the page's rows paint).
fn workflow_overview_signature(overview: &WorkflowOverviewResponse) -> u64 {
    let mut sig: u64 = 11;
    let mix = |v: u64, sig: &mut u64| {
        *sig = sig.wrapping_mul(31).wrapping_add(v);
    };
    let row_sig = |r: &WorkflowRunRow, sig: &mut u64| {
        mix(fold_str(&r.run_id), sig);
        mix(fold_str(&r.status), sig);
        mix(r.updated_at.unwrap_or(0), sig);
    };
    for g in &overview.groups {
        mix(fold_str(&g.instance_id), &mut sig);
        mix(fold_str(&g.workspace), &mut sig);
        mix((g.scope == WorkflowScope::Global) as u64, &mut sig);
        mix(g.error.is_some() as u64, &mut sig);
        mix(g.workflows.len() as u64, &mut sig);
        for w in &g.workflows {
            mix(fold_str(&w.name), &mut sig);
        }
        for (name, last) in &g.last_runs {
            mix(fold_str(name), &mut sig);
            row_sig(last, &mut sig);
        }
    }
    mix(overview.active_runs.len() as u64, &mut sig);
    for r in &overview.active_runs {
        row_sig(r, &mut sig);
    }
    mix(overview.recent_runs.len() as u64, &mut sig);
    for r in &overview.recent_runs {
        row_sig(r, &mut sig);
    }
    sig
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

/// Content signature of the quota payload: every provider section (the plan
/// page paints GLM, the side panels' card paints all of them — a same-count
/// reload with a changed percent in any section must re-emit).
fn quota_payload_signature(usage: &Option<AccountUsageStats>) -> u64 {
    let Some(u) = usage else {
        return u64::MAX;
    };
    quota_str_sig(&serde_json::to_string(u).unwrap_or_default())
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
        for page in ConfigPageKind::ALL {
            d.sections[page.index()].reset_data();
        }
        d.workflows.reset_data();
        // The hub-upgrade verdict names the OLD hub (TS: component state
        // that dies with the panel) — it must not greet the next one.
        d.hub_upgrade_busy = false;
        d.hub_upgrade_note = None;
        // The quota dock line is the OLD hub's account (ADR-0021); the next
        // hub's discovery re-seeds it. The generation bump drops any read
        // still in flight for the old hub (the TS mid-flight `profile`
        // guard).
        d.quota_dock = None;
        d.quota_dock_loading = false;
        d.quota_dock_gen += 1;
        // Session Files belong to a (hub, instance, session) triple — a new
        // lifecycle means a new machine's paths; nothing survives.
        d.files.reset();
        // Close tombstones name the OLD hub's instance ids; the new hub's
        // rows owe them nothing.
        d.closed_sessions.clear();
        // The remote-open watch's memory names the OLD hub's sessions too:
        // without this reset, the new hub's first snapshot would treat every
        // row the old hub never carried as a mid-run arrival and pop tabs
        // for all of them — the mass-open the cold-start baseline exists to
        // prevent. The next successful discovery re-baselines quietly.
        d.seen_sessions.clear();
        d.discovery_baselined = false;
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
                // The outcome is a bootstrap diagnostic, not a status-bar
                // fact: it stays in the trace (the bar used to read "hub
                // already listening" for the whole session with nothing to
                // act on).
                traceln!("[driver] hub bootstrap: {outcome:?}");
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
        // events. Discovery rides the same loop; replay (a fresh
        // `session/load`) is each tab's catch-up.
        d.phase = Phase::Discovering;
        d.next_discovery_at = 0; // fire on the first tick
        // A config page the user left open through the re-bootstrap re-seeds
        // itself against the new hub (its data did not survive the switch).
        // The Agents page's picker reads the models payload, so EITHER page
        // being open re-seeds that read.
        if d.models.open || d.sections[ConfigPageKind::Agents.index()].open {
            d.spawn_config_probe();
            d.spawn_models_load();
        }
        // The five config-section pages (batch I) re-seed the same way: a
        // page left open through the switch lost its payload (reset_data),
        // and without these reads it would paint the empty-state card for
        // the new hub until a manual refresh.
        for page in ConfigPageKind::ALL {
            if d.sections[page.index()].open {
                if d.sections[page.index()].supported.is_none() {
                    d.spawn_config_section_probe(page);
                }
                d.spawn_config_section_load(page);
            }
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
            d.sync_pending();
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

#[cfg(test)]
mod tests {
    // Plain #[test] with explicit imports — this file has no kit glob, but
    // the sibling test modules' explicit-import shape is the convention.
    use super::{fold_session_runs, max_run_sequence, merge_run_events, LoopEvent};
    use protocol::hub::{HubInstance, HubSessionInfo, InstanceOrigin};
    use protocol::hub_client::HubApiError;
    use protocol::workflow::WorkflowRunEvent;
    use tokio::sync::mpsc::unbounded_channel;

    fn run_event(sequence: u64) -> WorkflowRunEvent {
        WorkflowRunEvent {
            sequence,
            r#type: "log".to_string(),
            payload: Default::default(),
            truncated: None,
        }
    }

    /// L4's full-replay dedupe (TS `fetchEvents`): a degenerate cursor
    /// makes the server answer with the WHOLE journal — only sequences past
    /// the highest one held may append, and the floor is the MAX seen, not
    /// the last element (a clipped page can end on a lower sequence).
    #[test]
    fn journal_append_filters_replayed_prefix() {
        let held = vec![run_event(1), run_event(3), run_event(2)];
        assert_eq!(max_run_sequence(&held), 3, "the cursor is the max, not the tail");
        let replayed_full = vec![run_event(1), run_event(2), run_event(3), run_event(4), run_event(5)];
        let merged = merge_run_events(held, replayed_full);
        assert_eq!(
            merged.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            vec![1, 3, 2, 4, 5]
        );
        // An empty hold appends everything (the floor is 0).
        let merged = merge_run_events(Vec::new(), vec![run_event(1)]);
        assert_eq!(merged.len(), 1);
    }

    fn session(session_id: &str, updated_at: Option<u64>) -> HubSessionInfo {
        HubSessionInfo {
            session_id: session_id.to_string(),
            title: None,
            updated_at,
            status: None,
        }
    }

    fn instance(id: &str, workspace: Option<&str>, sessions: Vec<HubSessionInfo>) -> HubInstance {
        HubInstance {
            id: id.to_string(),
            port: Some(4900),
            pid: Some(42),
            started_at: Some(1),
            workspace: workspace.map(str::to_string),
            origin: InstanceOrigin::Editor,
            sessions,
            extra: Default::default(),
        }
    }

    fn driver() -> super::Driver {
        let (loop_tx, _loop_rx) = unbounded_channel();
        let (_cmd_tx, cmd_rx) = unbounded_channel();
        let (ev_tx, _ev_rx) = unbounded_channel();
        super::Driver::new(
            cmd_rx,
            ev_tx,
            std::sync::Arc::new(protocol::ws::TungsteniteWsFactory),
            std::sync::Arc::new(protocol::clock::TokioClock),
            loop_tx,
        )
    }

    /// A serve bridge self-exits (~1.5s) once its LAST advertised session is
    /// closed (ADR-0006/0016): the instance row must drop at once so no
    /// surface dials a corpse until the hub's prune. An editor bridge (no
    /// self-exit) keeps its row and lets the heartbeat list realign.
    #[test]
    fn closing_the_last_session_drops_a_serve_instance_but_keeps_an_editor_one() {
        let mut d = driver();
        let mut serve = instance("i-serve", Some("/w"), vec![session("s1", None)]);
        serve.origin = InstanceOrigin::Serve;
        d.instances.push(serve);
        d.instances.push(instance("i-edit", Some("/e"), vec![session("s2", None)]));

        d.handle_loop_event(
            LoopEvent::SessionClosed {
                instance_id: "i-serve".to_string(),
                session_id: "s1".to_string(),
                result: Ok(()),
            },
            0,
        );
        assert!(!d.instances.iter().any(|i| i.id == "i-serve"));

        d.handle_loop_event(
            LoopEvent::SessionClosed {
                instance_id: "i-edit".to_string(),
                session_id: "s2".to_string(),
                result: Ok(()),
            },
            0,
        );
        assert!(
            d.instances.iter().any(|i| i.id == "i-edit" && i.sessions.is_empty()),
            "editor bridges stay for the heartbeat realign"
        );
    }

    /// A closed session stays gone across discovery refreshes: the hub serves
    /// the last heartbeat's list, which still carries the row (and a bridge
    /// can re-advertise a retired conversation), so the poll that REPLACES
    /// `instances` resurrected the row — gone, back, then lingering. The
    /// tombstone hides it for CLOSE_TOMBSTONE_MS; the window lapsing lets a
    /// genuinely reused session back in (ADR-0006's re-arm) — and under the
    /// remote-open watch that re-admission is a mid-run arrival, so it pops
    /// an attaching tab like any other remote open.
    #[tokio::test]
    async fn a_closed_session_is_not_resurrected_by_a_discovery_refresh() {
        let mut d = driver();
        d.handle_loop_event(
            LoopEvent::SessionClosed {
                instance_id: "i-edit".to_string(),
                session_id: "s1".to_string(),
                result: Ok(()),
            },
            1_000,
        );

        // The next poll still carries the row (stale heartbeat).
        d.on_discovery(
            Ok(vec![
                instance("i-edit", Some("/e"), vec![session("s1", None), session("s2", None)]),
            ]),
            5_000,
        );
        assert!(
            !d.instances.iter().any(|i| i
                .sessions
                .iter()
                .any(|s| s.session_id == "s1")),
            "the closed row stays hidden inside the window"
        );
        assert!(
            d.instances.iter().any(|i| i
                .sessions
                .iter()
                .any(|s| s.session_id == "s2")),
            "the sibling row is untouched"
        );

        // Past the window the same list re-admits the row (it was genuinely
        // used again server-side).
        d.on_discovery(
            Ok(vec![instance("i-edit", Some("/e"), vec![session("s1", None)])]),
            1_000 + super::CLOSE_TOMBSTONE_MS + 1,
        );
        assert!(
            d.instances.iter().any(|i| i
                .sessions
                .iter()
                .any(|s| s.session_id == "s1")),
            "an expired tombstone re-admits the row"
        );
        // The re-admitted row is a never-seen mid-run arrival: the remote-open
        // watch attaches to it (someone genuinely reused the conversation).
        assert_eq!(
            d.tabs.len(),
            1,
            "the re-admitted session pops an attaching tab"
        );
        assert_eq!(d.tabs[0].target.as_deref(), Some("s1"));
    }

    /// The bridge's `$/zcode/session_closed` broadcast (another client
    /// retired the conversation through the hub): the holding tab closes and
    /// the row tombstones — the same retirement an own close performs — so
    /// the reconnect ladder never sees the gone session and the re-incubator
    /// never pulls it back up. A copy addressing a session no tab holds is
    /// ignored, and a stale event for the already-closed tab is a no-op.
    #[tokio::test]
    async fn a_remote_session_close_retires_the_holding_tab() {
        let mut d = driver();
        let mut serve =
            instance("i-serve", Some("/w"), vec![session("s1", None), session("s2", None)]);
        serve.origin = InstanceOrigin::Serve;
        d.instances.push(serve);
        let tab = d.open_tab("i-serve".to_string(), Some("s1".to_string()), "/w".to_string());
        assert_eq!(d.tabs.len(), 1);

        // Another session on the same bridge: the copy is ignored.
        d.handle_loop_event(
            LoopEvent::SessionClosedRemotely { tab, session_id: "s2".to_string() },
            1_000,
        );
        assert_eq!(d.tabs.len(), 1, "an unrelated close keeps the tab");
        assert!(d.closed_sessions.is_empty());

        // The held session: tab gone, row tombstoned and dropped from the
        // instance; the instance itself stays — its sibling s2 is still
        // advertised, so no self-exit is coming.
        d.handle_loop_event(
            LoopEvent::SessionClosedRemotely { tab, session_id: "s1".to_string() },
            2_000,
        );
        assert!(d.tabs.is_empty(), "the retirement closes the holding tab");
        assert!(
            d.closed_sessions
                .contains_key(&("i-serve".to_string(), "s1".to_string())),
            "the row tombstones against the next discovery poll"
        );
        let serve = d
            .instances
            .iter()
            .find(|i| i.id == "i-serve")
            .expect("a bridge with a live sibling session keeps its row");
        assert_eq!(
            serve.sessions.iter().map(|s| s.session_id.as_str()).collect::<Vec<_>>(),
            vec!["s2"]
        );

        // A stale event for the now-closed tab is a no-op.
        d.handle_loop_event(
            LoopEvent::SessionClosedRemotely { tab, session_id: "s1".to_string() },
            3_000,
        );
    }

    /// The broadcast copy is socket-agnostic: the hub hands EVERY attached
    /// client connection one copy per session alias, and the holder's own
    /// connection may be down at that moment (mid-reconnect — the state
    /// whose ladder must not revive the session). A copy that lands on a
    /// sibling tab's socket therefore still retires the holder.
    #[tokio::test]
    async fn a_remote_close_landing_on_a_sibling_socket_still_retires_the_holder() {
        let mut d = driver();
        let mut serve =
            instance("i-serve", Some("/w"), vec![session("s1", None), session("s2", None)]);
        serve.origin = InstanceOrigin::Serve;
        d.instances.push(serve);
        let tab1 = d.open_tab("i-serve".to_string(), Some("s1".to_string()), "/w".to_string());
        let tab2 = d.open_tab("i-serve".to_string(), Some("s2".to_string()), "/w".to_string());
        assert_eq!(d.tabs.len(), 2);

        // The s1 copy arrives on tab2's socket (tab1's connection is the
        // one that would be down here): tab1 retires anyway.
        d.handle_loop_event(
            LoopEvent::SessionClosedRemotely { tab: tab2, session_id: "s1".to_string() },
            1_000,
        );
        assert_eq!(
            d.tabs.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![tab2],
            "the holder retires regardless of the delivering socket; the sibling stays"
        );
        assert!(
            d.closed_sessions
                .contains_key(&("i-serve".to_string(), "s1".to_string())),
            "the row tombstones against the next discovery poll"
        );
    }

    /// A live close-tombstone vetoes the re-incubation: a ladder that burns
    /// out right as the remote close lands must not POST a resume for the
    /// retired session — a resume cannot be recalled once sent, and the
    /// server honors it (the resurrection this prevents).
    #[tokio::test]
    async fn a_live_close_tombstone_vetoes_re_incubation() {
        let mut d = driver();
        let tab = d.open_tab("i-dead".to_string(), Some("sess_1".to_string()), "/w".to_string());
        d.closed_sessions
            .insert(("i-dead".to_string(), "sess_1".to_string()), 25_000);

        for attempt in 0..super::TAB_REINCUBATE_ATTEMPTS {
            d.handle_loop_event(
                LoopEvent::ConnectResult {
                    tab,
                    result: Err("dial failed".to_string()),
                },
                attempt as u64 * 10_000,
            );
        }
        assert!(
            d.tabs.iter().all(|t| t.reincubated_at.is_none()),
            "a tombstoned session must not revive"
        );
    }

    /// Remote-opened sessions pop a tab (2026-10-24, amending the
    /// 2026-10-22 quiet rule): a session that first appears in a LATER
    /// snapshot — a phone create, a remote resume — gets an attaching tab,
    /// because the attached client is what holds the hosting serve bridge
    /// open (its idle self-exit only counts idle time with no clients). The
    /// cold-start baseline stays quiet; a tombstone-lapsed re-admission of
    /// an already-seen id never pops.
    #[tokio::test]
    async fn a_remotely_opened_session_pops_a_tab_once_baselined() {
        let mut d = driver();

        // Cold start: everything advertised at boot is the baseline — quiet.
        d.on_discovery(Ok(vec![instance("i", Some("/w"), vec![session("s0", None)])]), 1_000);
        assert!(d.tabs.is_empty(), "cold start opens nothing");

        // A remote create/resume lands next to s0 — one attaching tab,
        // active, on the right instance.
        d.on_discovery(
            Ok(vec![instance("i", Some("/w"), vec![
                session("s0", None),
                session("s_new", None),
            ])]),
            16_000,
        );
        assert_eq!(d.tabs.len(), 1);
        assert_eq!(d.tabs[0].target.as_deref(), Some("s_new"));
        assert_eq!(d.tabs[0].instance_id, "i");
        assert_eq!(d.active_tab, Some(d.tabs[0].id));

        // Plain re-advertisement of the same ids never pops again.
        d.on_discovery(
            Ok(vec![instance("i", Some("/w"), vec![
                session("s0", None),
                session("s_new", None),
            ])]),
            31_000,
        );
        assert_eq!(d.tabs.len(), 1, "a seen id re-advertised is not an opening");

        // The remote close retires the tab; a stubborn row riding past the
        // tombstone window back into the list is a REAPPEARANCE — no pop.
        let tab_id = d.tabs[0].id;
        d.handle_loop_event(
            LoopEvent::SessionClosedRemotely { tab: tab_id, session_id: "s_new".to_string() },
            32_000,
        );
        assert!(d.tabs.is_empty(), "the retirement closed the holding tab");
        d.on_discovery(
            Ok(vec![instance("i", Some("/w"), vec![
                session("s0", None),
                session("s_new", None),
            ])]),
            32_000 + super::CLOSE_TOMBSTONE_MS + 1,
        );
        assert!(d.tabs.is_empty(), "an already-seen id never re-pops");
    }

    /// A session a tab already holds is not a remote open: the desktop's own
    /// attach/resume in flight (the tab exists, discovery has not advertised
    /// the id before) must not pop — or activate — a second tab.
    #[tokio::test]
    async fn a_session_a_tab_already_holds_is_not_a_remote_open() {
        let mut d = driver();
        d.on_discovery(Ok(vec![instance("i", Some("/w"), vec![session("s0", None)])]), 1_000);
        let held = d.open_tab("i".to_string(), Some("s_mine".to_string()), "/w".to_string());

        d.on_discovery(
            Ok(vec![instance("i", Some("/w"), vec![
                session("s0", None),
                session("s_mine", None),
                session("s_other", None),
            ])]),
            16_000,
        );
        assert_eq!(d.tabs.len(), 2, "only the genuinely remote s_other pops");
        assert!(
            d.tabs.iter().any(|t| t.id == held && t.target.as_deref() == Some("s_mine")),
            "the held tab is untouched"
        );
        assert!(d.tabs.iter().any(|t| t.target.as_deref() == Some("s_other")));
    }

    /// A tab the user closed is not a remote open: closing a tab whose
    /// session discovery has not advertised yet (our own fresh conversation
    /// inside the heartbeat + poll lag) must not pop the tab back open when
    /// the id first appears.
    #[tokio::test]
    async fn closing_a_tab_marks_its_session_seen_not_remote_opened() {
        let mut d = driver();
        d.on_discovery(Ok(vec![instance("i", Some("/w"), vec![session("s0", None)])]), 1_000);
        let tab = d.open_tab("i".to_string(), Some("s_mine".to_string()), "/w".to_string());
        d.close_tab(tab);
        assert!(d.tabs.is_empty());

        d.on_discovery(
            Ok(vec![instance(
                "i",
                Some("/w"),
                vec![session("s0", None), session("s_mine", None)],
            )]),
            16_000,
        );
        assert!(d.tabs.is_empty(), "the closed tab's session never pops back");
    }

    /// One tab per session: a second open of the same target activates the
    /// existing tab instead of stacking a duplicate — including when the two
    /// opens name DIFFERENT instances (the session moved bridges, or a
    /// history resume raced a discovery row). An attaching tab counts; its
    /// target IS the session.
    #[tokio::test]
    async fn opening_the_same_session_twice_activates_one_tab() {
        let mut d = driver();
        let first = d.open_tab("i1".to_string(), Some("s1".to_string()), "/w".to_string());
        let second = d.open_tab("i2".to_string(), Some("s1".to_string()), "/w".to_string());
        assert_eq!(first, second, "the duplicate open must reuse the tab");
        assert_eq!(d.tabs.len(), 1);
        assert_eq!(d.active_tab, Some(first));
    }

    /// Drag-reorder: drop-before-target inserts at the target's slot, a
    /// drop on the strip's tail moves to the end, and a drop on the
    /// dragged tab's own slot leaves the order untouched (the remove-first
    /// insert would otherwise push it to the end).
    #[tokio::test]
    async fn move_tab_reorders_the_strip() {
        let mut d = driver();
        let a = d.open_tab("i1".to_string(), Some("s1".to_string()), "/w".to_string());
        let b = d.open_tab("i2".to_string(), Some("s2".to_string()), "/w".to_string());
        let c = d.open_tab("i3".to_string(), Some("s3".to_string()), "/w".to_string());
        let ids = |d: &super::Driver| d.tabs.iter().map(|t| t.id).collect::<Vec<_>>();

        d.handle_cmd(
            super::UiCommand::MoveTab { id: c, before_id: Some(a) },
            0,
        );
        assert_eq!(ids(&d), vec![c, a, b], "drop on a lands before a");

        d.handle_cmd(
            super::UiCommand::MoveTab { id: a, before_id: None },
            0,
        );
        assert_eq!(ids(&d), vec![c, b, a], "tail drop moves to the end");

        d.handle_cmd(
            super::UiCommand::MoveTab { id: b, before_id: Some(b) },
            0,
        );
        assert_eq!(ids(&d), vec![c, b, a], "a self-drop is a no-op");

        d.handle_cmd(
            super::UiCommand::MoveTab { id: b, before_id: Some(999) },
            0,
        );
        assert_eq!(ids(&d), vec![c, a, b], "a vanished target falls to the end");
    }

    /// The tab context menu's batch closes: "close others" keeps only the
    /// anchor and focuses it; "close right" trims the tail and hands focus
    /// to the anchor only when the active tab was among the closed.
    #[tokio::test]
    async fn close_other_and_close_right_batch_close() {
        let mut d = driver();
        let a = d.open_tab("i1".to_string(), Some("s1".to_string()), "/w".to_string());
        let b = d.open_tab("i2".to_string(), Some("s2".to_string()), "/w".to_string());
        let c = d.open_tab("i3".to_string(), Some("s3".to_string()), "/w".to_string());
        let e = d.open_tab("i4".to_string(), Some("s4".to_string()), "/w".to_string());
        let ids = |d: &super::Driver| d.tabs.iter().map(|t| t.id).collect::<Vec<_>>();

        d.handle_cmd(super::UiCommand::CloseOtherTabs { id: b }, 0);
        assert_eq!(ids(&d), vec![b]);
        assert_eq!(d.active_tab, Some(b), "close-others focuses the anchor");

        let mut d = driver();
        let a = d.open_tab("i1".to_string(), Some("s1".to_string()), "/w".to_string());
        let b = d.open_tab("i2".to_string(), Some("s2".to_string()), "/w".to_string());
        let _c = d.open_tab("i3".to_string(), Some("s3".to_string()), "/w".to_string());
        let _e = d.open_tab("i4".to_string(), Some("s4".to_string()), "/w".to_string());
        // Active is the LAST tab (open_tab activates); closing right of b
        // kills it, so the anchor takes focus.
        d.handle_cmd(super::UiCommand::CloseRightTabs { id: b }, 0);
        assert_eq!(ids(&d), vec![a, b]);
        assert_eq!(d.active_tab, Some(b), "the anchor takes over when the active closed");

        // Active left of the anchor stays active; a right close with no
        // tabs to the right changes nothing.
        d.handle_cmd(super::UiCommand::SelectTab { id: a }, 0);
        d.handle_cmd(super::UiCommand::CloseRightTabs { id: b }, 0);
        assert_eq!(ids(&d), vec![a, b]);
        assert_eq!(d.active_tab, Some(a), "a surviving active keeps focus");
    }

    /// The tab strip's project identity (the CLI composer-cap shape): a
    /// tab's summary carries the workspace path it was opened for, so the
    /// strip can prefix the project name and the hover can show the full
    /// path. A root-only fallback workspace ("/") is suppressed.
    #[tokio::test]
    async fn tab_summaries_carry_the_workspace_path() {
        let (loop_tx, _loop_rx) = unbounded_channel();
        let (_cmd_tx, cmd_rx) = unbounded_channel();
        let (ev_tx, mut ev_rx) = unbounded_channel();
        let mut d = super::Driver::new(
            cmd_rx,
            ev_tx,
            std::sync::Arc::new(protocol::ws::TungsteniteWsFactory),
            std::sync::Arc::new(protocol::clock::TokioClock),
            loop_tx,
        );
        let tab = d.open_tab(
            "i1".to_string(),
            Some("s1".to_string()),
            "/Users/w/demo-project".to_string(),
        );
        d.open_tab("i2".to_string(), Some("s2".to_string()), "/".to_string());
        d.emit_if_changed();
        let Ok(super::RuntimeEvent::Snapshot(snap)) = ev_rx.try_recv() else {
            panic!("expected a snapshot after emit_if_changed");
        };
        let first = snap.tabs.iter().find(|t| t.id == tab).unwrap();
        assert_eq!(first.workspace.as_deref(), Some("/Users/w/demo-project"));
        let root_only = snap.tabs.iter().find(|t| t.session_id.as_deref() == Some("s2")).unwrap();
        assert_eq!(
            root_only.workspace.as_deref(),
            Some("/"),
            "the path travels verbatim; the ROOT-only filter lives in the strip"
        );
    }

    /// The quit dialog gates on turns THIS app started only: a prompt we
    /// sent (local_prompt_active) counts, a broadcast-only running turn
    /// another client started does not, and the prompt's own settle drops
    /// the count again.
    #[tokio::test]
    async fn own_running_turns_count_only_prompts_this_app_sent() {
        let (loop_tx, _loop_rx) = unbounded_channel();
        let (_cmd_tx, cmd_rx) = unbounded_channel();
        let (ev_tx, mut ev_rx) = unbounded_channel();
        let mut d = super::Driver::new(
            cmd_rx,
            ev_tx,
            std::sync::Arc::new(protocol::ws::TungsteniteWsFactory),
            std::sync::Arc::new(protocol::clock::TokioClock),
            loop_tx,
        );
        // The "other" tab: a turn the CLI started — broadcast activity
        // only, never a prompt of ours.
        d.open_tab("i2".to_string(), Some("s2".to_string()), "/w".to_string());
        let t = d.tabs.iter_mut().find(|t| t.target.as_deref() == Some("s2")).unwrap();
        t.core.session_states.insert(
            "s2".to_string(),
            protocol::session::SessionActivity {
                running: true,
                ..Default::default()
            },
        );
        // The "own" tab: attached + open, then we send a prompt into it.
        let own = d.open_tab("i1".to_string(), Some("s1".to_string()), "/w".to_string());
        let t = d.tabs.iter_mut().find(|t| t.id == own).unwrap();
        t.core.conn_open = true;
        t.core.active_session_id = Some("s1".to_string());
        d.handle_cmd(
            super::UiCommand::SendPrompt {
                text: "hi".to_string(),
                images: vec![],
            },
            0,
        );
        d.emit_if_changed();
        let Ok(super::RuntimeEvent::Snapshot(snap)) = ev_rx.try_recv() else {
            panic!("expected a snapshot after emit_if_changed");
        };
        assert_eq!(snap.running_session_count, 2, "both turns run");
        assert_eq!(snap.own_running_session_count, 1, "only OUR prompt counts");

        // The prompt response settles our turn: the own count drops while
        // the other client's turn keeps running.
        let wire = d
            .tabs
            .iter_mut()
            .find(|t| t.id == own)
            .unwrap()
            .core
            .take_wire()
            .pop()
            .expect("the prompt sat on the outbox (nothing pumped it)");
        let super::WireRequest::Prompt(req) = wire else {
            panic!("expected the prompt wire request");
        };
        d.handle_loop_event(
            LoopEvent::PromptResult {
                tab: own,
                req,
                result: Ok(serde_json::json!({})),
            },
            0,
        );
        d.emit_if_changed();
        let Ok(super::RuntimeEvent::Snapshot(snap)) = ev_rx.try_recv() else {
            panic!("expected a snapshot after the settle");
        };
        assert_eq!(snap.own_running_session_count, 0, "the settled turn is no longer ours");
        assert_eq!(snap.running_session_count, 1, "the other client's turn still runs");
    }

    /// A dead backend revives instead of retrying forever: once a tab has
    /// burned its honest-retry budget AND its instance left discovery, the
    /// re-incubation arms (its own fetch talks to the test's bogus hub and
    /// settles elsewhere — arming is the assertion). A bridge still listed in
    /// discovery never arms. The arming must not require `ever_connected`:
    /// a tab opened right before a hub restart has never connected, yet
    /// after the restart wiped the registry it dialed the corpse id for
    /// minutes until the user closed the tab (observed 2026-10-21).
    #[tokio::test]
    async fn a_dead_instance_re_incubates_instead_of_retrying_forever() {
        let mut d = driver();
        let tab = d.open_tab("i-dead".to_string(), Some("sess_1".to_string()), "/w".to_string());
        // No `ever_connected` seed — the fresh-process shape is the regression.

        for attempt in 0..(super::TAB_REINCUBATE_ATTEMPTS - 1) {
            d.handle_loop_event(
                LoopEvent::ConnectResult {
                    tab,
                    result: Err("dial failed".to_string()),
                },
                attempt as u64 * 10_000,
            );
        }
        assert!(
            d.tabs.iter().all(|t| t.reincubated_at.is_none()),
            "the honest-retry budget must not arm early"
        );

        d.handle_loop_event(
            LoopEvent::ConnectResult {
                tab,
                result: Err("dial failed".to_string()),
            },
            30_000,
        );
        assert!(
            d.tabs.iter().any(|t| t.id == tab && t.reincubated_at.is_some()),
            "the instance left discovery — the revival must arm"
        );

        // The listed-bridge guard: the same burnout against an instance that
        // discovery still holds keeps the plain retry ladder.
        let mut d2 = driver();
        let tab2 = d2.open_tab("i-live".to_string(), Some("sess_2".to_string()), "/w".to_string());
        d2.instances
            .push(instance("i-live", Some("/w"), vec![session("sess_2", None)]));
        for attempt in 0..super::TAB_REINCUBATE_ATTEMPTS {
            d2.handle_loop_event(
                LoopEvent::ConnectResult {
                    tab: tab2,
                    result: Err("dial failed".to_string()),
                },
                attempt as u64 * 10_000,
            );
        }
        assert!(
            d2.tabs.iter().all(|t| t.reincubated_at.is_none()),
            "a bridge still in discovery never re-incubates"
        );
    }

    /// The status bar's quota dock line (ADR-0021) is sticky on failure — a
    /// transient hub hiccup must not blank the resident line — and only a
    /// successful `None` (the hub says "no quota data") clears it. A result
    /// minted under a PREVIOUS lifecycle generation (a hub switch raced the
    /// read) drops whole: the old hub's account must not greet the new hub.
    #[test]
    fn quota_dock_line_is_sticky_on_failure_and_clears_on_successful_none() {
        let (loop_tx, _loop_rx) = unbounded_channel();
        let (_cmd_tx, cmd_rx) = unbounded_channel();
        let (ev_tx, _ev_rx) = unbounded_channel();
        let mut d = super::Driver::new(
            cmd_rx,
            ev_tx,
            std::sync::Arc::new(protocol::ws::TungsteniteWsFactory),
            std::sync::Arc::new(protocol::clock::TokioClock),
            loop_tx,
        );
        assert_eq!(d.quota_dock, None);
        d.on_quota_dock(0, Ok(Some("GLM coding plan · 45%".to_string())));
        assert_eq!(d.quota_dock.as_deref(), Some("GLM coding plan · 45%"));
        d.on_quota_dock(0, Err(HubApiError {
            message: "hub hiccup".to_string(),
            status: None,
            network: true,
            instance_dead: false,
        }));
        assert_eq!(
            d.quota_dock.as_deref(),
            Some("GLM coding plan · 45%"),
            "a failed refresh keeps the resident line"
        );
        // The hub-switch reset: the line clears and the generation bumps, so
        // the OLD hub's in-flight read (still gen 0) drops on arrival.
        d.quota_dock = None;
        d.quota_dock_gen += 1;
        d.on_quota_dock(0, Ok(Some("OLD HUB · 90%".to_string())));
        assert_eq!(d.quota_dock, None, "a stale-generation result drops");
        d.on_quota_dock(1, Ok(None));
        assert_eq!(d.quota_dock, None, "a successful empty clears");
        d.on_quota_dock(1, Ok(Some("NEW HUB · 10%".to_string())));
        assert_eq!(d.quota_dock.as_deref(), Some("NEW HUB · 10%"));
    }

    /// The status bar's quota dock line follows every quota usage read: the
    /// drawer / page and the resident line are the same account quota, so a
    /// manual refresh that repaints one must repaint the other — the hub
    /// caches the two endpoints separately, so only a second request gets
    /// the line fresh. Outside the hub-reachable phases neither read fires.
    #[tokio::test]
    async fn a_quota_usage_read_also_refreshes_the_dock_line() {
        let mut d = driver();
        assert!(matches!(d.phase, super::Phase::Bootstrapping));
        d.handle_cmd(super::UiCommand::RefreshQuota, 0);
        assert!(
            !d.quota_dock_loading,
            "no hub yet — the dock read must wait like the usage read"
        );

        d.phase = super::Phase::Connected;
        d.handle_cmd(super::UiCommand::RefreshQuota, 0);
        assert!(
            d.quota_dock_loading,
            "a quota refresh must also kick the dock line read"
        );
    }

    /// Discovery never opens a tab on its own (2026-10-22 decision, dropping
    /// M1 parity with appStore's cold-start auto-attach): sessions on the
    /// hub — cold start, a remote create, anything — only repaint the
    /// sidebar; a tab needs an explicit gesture (+, sidebar row, history).
    #[test]
    fn discovery_never_opens_a_tab() {
        let mut d = driver();
        d.on_discovery(
            Ok(vec![instance(
                "inst-a",
                Some("/w/a"),
                vec![session("s_latest", Some(9_000))],
            )]),
            0,
        );
        assert!(d.tabs.is_empty(), "no tab may be minted by a poll");
        assert!(
            d.discovery_note.is_none(),
            "a session exists — no landing note"
        );
        assert_eq!(
            d.instances.len(),
            1,
            "the row still lands in the sidebar list"
        );
    }

    /// The landing note (the TS picker's empty state): with no tab open and
    /// no session row anywhere, the status bar carries the hint — instance
    /// rows without sessions never mint a tab either. A session appearing
    /// anywhere replaces the hint with the row, and under the remote-open
    /// watch a mid-run arrival attaches to it as well.
    #[tokio::test]
    async fn sessionless_discovery_keeps_the_landing_note() {
        let mut d = driver();
        d.on_discovery(
            Ok(vec![
                instance("inst-a", Some("/w/a"), vec![]),
                instance("inst-b", Some("/w/b"), vec![]),
            ]),
            0,
        );
        assert!(d.tabs.is_empty());
        assert!(
            d.discovery_note.is_some(),
            "nothing to open — the hint shows"
        );

        d.on_discovery(
            Ok(vec![instance(
                "inst-a",
                Some("/w/a"),
                vec![session("s_a", None)],
            )]),
            1_000,
        );
        assert_eq!(d.tabs.len(), 1, "the mid-run arrival attaches");
        assert_eq!(d.tabs[0].target.as_deref(), Some("s_a"));
        assert!(d.discovery_note.is_none(), "the row replaces the hint");
    }

    /// The third ladder's map fold (TS `new Map(conv.runs.map(...))`): a
    /// REPLACE keyed by run id, never a merge — a run that left the session
    /// must leave the map too, or its stale entry could pin a phantom
    /// session onto a later row.
    #[test]
    fn session_runs_map_replaces_not_merges() {
        use protocol::workflow::ConversationRunSummary;
        let summary = |run_id: &str| ConversationRunSummary {
            run_id: run_id.to_string(),
            tool_call_id: None,
            label: None,
            updated_at: None,
            status: "running".to_string(),
            stop_reason: None,
            resumed_from: None,
            superseded_by: None,
            failure_code: None,
            failure_message: None,
            resumable: false,
        };
        let first = fold_session_runs(&[summary("a"), summary("b")], "s1");
        assert_eq!(first.get("a").map(String::as_str), Some("s1"));
        assert_eq!(first.get("b").map(String::as_str), Some("s1"));
        let second = fold_session_runs(&[summary("b")], "s1");
        assert_eq!(
            second.len(),
            1,
            "a run that left the session must leave the map"
        );
    }
}
