//! Dynamic-workflow management payloads (bridge 0.48.0+, server ADR-0029),
//! ported from `src/lib/types.ts`.
//!
//! Shapes of the per-instance settings routes under
//! `/api/instances/{id}/settings/workflows*` and `/settings/workflow-runs*`,
//! plus the hub's machine-level `/api/workflow-overview`. Read-heavy and
//! append-only: the backend is the authority, the app renders what arrives
//! and never infers fields the route did not send.
//!
//! Server references: `src/settings/workflow.ts` (the v4 passthroughs and
//! command results — the app's TS shapes ARE the contract for these, the
//! bridge relays the backend payloads verbatim after joining `acpSessionId`
//! and stripping dismissed runs), `src/remote/workflow-overview.ts`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Workflow scope vocabulary (upstream workflows/* `scope`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowScope {
    Project,
    Global,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// One invalid-file row of a workflow list (`invalid[]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowInvalidEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowListResponse {
    pub ok: bool,
    pub workflows: Vec<WorkflowEntry>,
    pub invalid: Vec<WorkflowInvalidEntry>,
    /// The scanned directory, returned even when it does not exist yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowDetailResponse {
    pub ok: bool,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub scope: WorkflowScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<BTreeMap<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
}

/// One artifact reference on a journal run row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRunArtifactRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// One history row from the journal (`workflows/runs` — cross-restart, not
/// session-bound). `parentSessionId` is the BACKEND session id;
/// `acpSessionId` is the attachable alias the bridge joined onto it
/// (bridge 0.57.0) — present means an editor-launched run is actionable
/// (open session / resume) without this app's own launch memory.
/// `ownerInstanceId` is the hub's annotation (bridge 0.60.0): the instance
/// whose live session list currently holds this run's session — the address
/// for stop/resume from a client attached elsewhere. Absent when no instance
/// lists the session (fall back to launch memory).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRunRow {
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spent_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<BTreeMap<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<Vec<WorkflowRunArtifactRef>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_instance_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRunsHistoryResponse {
    pub ok: bool,
    pub runs: Vec<WorkflowRunRow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
}

/// One group of the hub's machine-level workflow overview (bridge 0.60.0):
/// the GLOBAL workflows (one machine-wide group — `instanceId` is whichever
/// instance answered) or one workspace's PROJECT workflows. `lastRuns` is
/// the per-name newest journal row (the list badge source). `error` marks an
/// instance that refused (gate off, offline, old bridge) — the group still
/// renders. Note the server uses `workspace: ""` for the global group.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowHubGroup {
    pub instance_id: String,
    pub scope: WorkflowScope,
    /// Display label: the instance's workspace basename (global group uses
    /// the scope label).
    pub workspace: String,
    pub workflows: Vec<WorkflowEntry>,
    pub invalid: Vec<WorkflowInvalidEntry>,
    pub last_runs: BTreeMap<String, WorkflowRunRow>,
    pub error: Option<String>,
}

/// The hub's machine-level workflow page payload (`/api/workflow-overview`):
/// one group per WORKSPACE (a project with an editor + serve bridge renders
/// once — the hub dedupes), each pre-joined with its per-name newest journal
/// row, plus the machine-wide active-run list and its finished complement.
/// The app renders this as-is — no client-side merging or dedupe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowOverviewResponse {
    pub ok: bool,
    pub groups: Vec<WorkflowHubGroup>,
    pub active_runs: Vec<WorkflowRunRow>,
    /// The same journal read's finished complement (terminal statuses, newest
    /// first, hub-capped): the top-level "recently finished" list's feed.
    pub recent_runs: Vec<WorkflowRunRow>,
}

/// One declared arg on a saved workflow's meta (`args` map, upstream
/// `zcodeSavedWorkflowArgsDeclarationSchema`): drives the typed launch form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowArgDeclaration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
}

/// `POST /settings/workflow-runs/{runId}/settings` (bridge 0.49.0, PR #265).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AmendRunSettingsResponse {
    pub ok: bool,
    /// The run that continues the work — same id in place, NEW id after
    /// supersede.
    pub run_id: String,
    pub tool_call_id: String,
    /// Present only when the amendment stopped and superseded a flying run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_run_id: Option<String>,
}

/// A session-scoped run summary (`v4 workflowRuns` via the bridge).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationRunSummary {
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<u64>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_message: Option<String>,
    /// cancelled ∪ failed-Interrupted — the resume button's source of truth.
    pub resumable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationRunsResponse {
    pub ok: bool,
    pub runs: Vec<ConversationRunSummary>,
}

/// One journal event; `sequence` is the append-only cursor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRunEvent {
    pub sequence: u64,
    pub r#type: String,
    #[serde(default)]
    pub payload: WorkflowRunEventPayload,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
}

/// The event payload — the v4 journal contract deliberately does not
/// restate the engine's shapes ("readers interpret per event kind"), so the
/// payload rides as a raw object (insertion-ordered Map: previews must
/// serialize keys in wire order like the TS app's JSON.stringify).
pub type WorkflowRunEventPayload = Map<String, Value>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRunEventsResponse {
    pub ok: bool,
    pub events: Vec<WorkflowRunEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
}

/// One artifact. Kind-specific latest fields sit at the top level (kept open
/// via `extra`, mirroring the TS index signature); `versions` carries
/// history (≤16).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowArtifact {
    pub id: String,
    /// "file" | "markdown" | "chart" | "table" | "metrics" | "board" | <any
    /// string> — permissive on purpose.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub versions: Option<Vec<BTreeMap<String, Value>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<bool>,
    /// Unknown keys survive a round trip.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// One item row of a board/table artifact's paginated inventory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowArtifactItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordinal: Option<u64>,
    pub item: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowArtifactItemsResponse {
    pub ok: bool,
    pub items: Vec<WorkflowArtifactItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowArtifactReadResponse {
    pub ok: bool,
    pub data_base64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<u64>,
}

/// A workspace node's summary is a strict OBJECT upstream (v4
/// workflow-workspace schema), never a string — which fields are set depends
/// on the op (arrays carry resultCount, world.run carries exitCode and the
/// two output streams, string bodies only resultBytes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowNodeSummary {
    pub result_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowWorkspaceNode {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordinal: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<WorkflowNodeSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowWorkspaceResponse {
    pub ok: bool,
    pub nodes: Vec<WorkflowWorkspaceNode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowNodeResultResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
}

/// Body of `POST /settings/workflow-runs/{runId}/settings?sessionId=` —
/// three-state per field: the key's PRESENCE is the discriminator
/// (absent = keep, `null` = revert to default, value = set).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AmendRunSettingsBody {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::settings::double_option"
    )]
    pub subagent_model: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::settings::double_option"
    )]
    pub max_concurrency: Option<Option<u64>>,
}

/// `POST …/dismiss` — hiding one settled journal run (409 while flying).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DismissRunResponse {
    pub ok: bool,
}

/// `POST …/dismiss-batch` — still-running ids are skipped, the settled ones
/// hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DismissRunsResponse {
    pub ok: bool,
    pub dismissed: u64,
    pub skipped_active: u64,
}

/// `GET …/artifacts` — the run's artifact references (kind-specific detail
/// rides each artifact's own route).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunArtifactsResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<Vec<Value>>,
}

/// `POST …/start` — `acpSessionId` is what openSession() consumes
/// (server `StartSavedWorkflowResult`, settings/workflow.ts:725).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowStartResponse {
    pub ok: bool,
    pub acp_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowCreatePromptResponse {
    pub ok: bool,
    pub prompt: String,
}
