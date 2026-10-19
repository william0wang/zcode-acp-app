//! Hub discovery / history / filesystem REST payloads and connection
//! profiles, ported from `src/lib/types.ts`.
//!
//! Server references (zcode-acp-server src — the arbiter for divergences):
//! - `src/remote/hub-server.ts` — /api/instances, /api/projects, /api/fs/list,
//!   /api/instances (POST), /api/projects/sessions, /api/upgrade
//! - `src/remote/session-list-endpoint.ts` — the bridge /sessions rows
//! - `src/remote/file-endpoint.ts` — the per-session /fs/list payload
//! - `src/tasks-index.ts` — KnownWorkspace rows behind /api/projects

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Epoch-ms fields arrive as JSON numbers, and the Bun runtime serializes
/// `Date.now()`-derived values with a fractional part (observed live:
/// `updatedAt: 1792045487151.9006` on /api/instances). The TS types say
/// `number` (floats included) and the server wins — deserialize tolerantly,
/// truncating the fraction. Recorded in tests/contract.rs
/// (`epoch_ms_fields_accept_fractional_numbers`).
pub(crate) fn deserialize_epoch_ms_opt<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .or_else(|| v.as_f64().map(|f| f.max(0.0) as u64))
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom("expected an epoch-ms number")),
    }
}

/// Required-field flavor of [`deserialize_epoch_ms_opt`].
pub(crate) fn deserialize_epoch_ms<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    value
        .as_u64()
        .or_else(|| value.as_f64().map(|f| f.max(0.0) as u64))
        .ok_or_else(|| serde::de::Error::custom("expected an epoch-ms number"))
}

/// How a bridge instance was started (bridge 0.17.0, server ADR-0014):
/// "editor" = spawned by an editor over stdio; "serve" = headless bridge
/// created via remote session-create.
///
/// Divergence note: the current hub ALWAYS emits `origin` on /api/instances
/// (hub-server.ts `InstanceEntry.origin` is required and the response maps it
/// unconditionally); the TS type keeps it optional for older bridges and
/// reads absence as "editor" — `Default` mirrors that tolerance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum InstanceOrigin {
    #[default]
    Editor,
    Serve,
}

/// Coarse heartbeat indicator (bridge 0.8.0, ADR-0005): "running" | "idle",
/// up to ~10s stale. Kept permissive (`String`) — the server drops unknown
/// values from the wire entirely (validSessions, hub-server.ts).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubSessionInfo {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_epoch_ms_opt"
    )]
    pub updated_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// One bridge of GET /api/instances. The current hub always sends
/// id/port/pid/startedAt/workspace/origin/sessions (hub-server.ts builds the
/// row from its required `InstanceEntry`); the TS type's optional port/pid/
/// startedAt/workspace are older-bridge tolerance, kept here as-is.
/// `extra` mirrors the TS index signature (additive-only contract).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubInstance {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_epoch_ms_opt"
    )]
    pub started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default)]
    pub origin: InstanceOrigin,
    pub sessions: Vec<HubSessionInfo>,
    /// Unknown keys survive a round trip (REMOTE-CLIENTS.md: additive-only).
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// One known-project workspace of GET /api/projects (server `KnownWorkspace`,
/// tasks-index.ts:595 — shape identical to the TS type).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubProject {
    pub workspace_path: String,
    pub sessions: u64,
    #[serde(deserialize_with = "deserialize_epoch_ms")]
    pub last_active: u64,
}

/// One subdirectory entry of GET /api/fs/list (browse-and-create).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsBrowseEntry {
    pub name: String,
}

/// GET /api/fs/list (bridge 0.65.0): one directory level of the hub machine's
/// filesystem. `path` is the RESOLVED (realpath) spelling — navigate by it,
/// not by what was sent. Only subdirectories are listed; hidden (`.`-prefixed)
/// entries are included, filter client-side. `creatable` = a NEW session may
/// start in the listed directory itself (known project, or under a configured
/// remote.projectRoots root). `parent` is JSON null at the filesystem root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsBrowseResult {
    pub path: String,
    pub parent: Option<String>,
    pub creatable: bool,
    pub entries: Vec<FsBrowseEntry>,
    pub truncated: bool,
}

/// POST /api/instances result: the (new or reused) serve instance id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubCreateInstanceResult {
    pub id: String,
    pub reused: bool,
}

/// Journal workflow activity joined bridge-side onto history rows (bridge
/// 0.57.0; server `WorkflowActivity`, settings/workflow.ts:374).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowActivity {
    /// Runs of this session still pending/running in the journal's newest window.
    pub active: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<WorkflowActivityLast>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowActivityLast {
    pub status: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_epoch_ms_opt"
    )]
    pub updated_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// One row of GET /api/projects/sessions (bridge 0.19.0, server ADR-0015).
/// `updated_at` is the store's ISO timestamp — NOT discovery's epoch-ms
/// number. The bridge always adds `live: false` / `running: false`
/// (session-list-endpoint.ts) — optional here, as in the TS type.
/// `extra` mirrors the TS index signature.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubHistorySession {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_activity: Option<WorkflowActivity>,
    /// Unknown keys survive a round trip.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Composite "load more" cursor. Pass BOTH fields back verbatim — `before`
/// alone would drop rows whose timestamps tie across a page boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubHistoryCursor {
    #[serde(deserialize_with = "deserialize_epoch_ms")]
    pub before: u64,
    pub before_id: String,
}

/// The instance block of a history page — the serve bridge a follow-up
/// resume attaches to (always carries `origin` on the current server,
/// hub-server.ts:2033).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubHistoryInstance {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<InstanceOrigin>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubHistoryPage {
    pub workspace_path: String,
    pub instance: HubHistoryInstance,
    pub sessions: Vec<HubHistorySession>,
    /// null = no older sessions (last page). Serialized as JSON null.
    pub next_cursor: Option<HubHistoryCursor>,
}

/// The saved hub URL + bearer token the client dials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionProfile {
    pub hub_url: String,
    pub token: String,
}

/// One entry of the persisted multi-server book (`zcode-acp:servers`): the
/// connection pair plus a user-editable display name. Exactly one entry is
/// active at a time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedServer {
    pub id: String,
    pub name: String,
    pub hub_url: String,
    pub token: String,
}

/// Why POST /api/upgrade decided the way it did.
///
/// Divergence note (server wins): the TS type lists only
/// "version" | "mtime" | "up-to-date", but the server also answers
/// "fingerprint" (hub-server.ts `diskCodeIsNewer`) when a content-fingerprint
/// mismatch is detected. All four spellings are modeled here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpgradeReason {
    Fingerprint,
    Version,
    Mtime,
    UpToDate,
}

/// POST /api/upgrade (bridge 0.11.7): the hub's own staleness-check verdict.
/// `restarting` means IT decided the on-disk code is newer — the client only
/// triggered the check; the decision was never ours to make.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubUpgradeResult {
    pub ok: bool,
    pub restarting: bool,
    pub reason: UpgradeReason,
    pub running_version: String,
    /// JSON null when the on-disk package.json was unreadable.
    pub disk_version: Option<String>,
}

/// Entry kind of the per-session /fs/list (Session Files, bridge 0.7.0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsEntryKind {
    File,
    Dir,
    Symlink,
}

/// One entry of the per-session /fs/list.
///
/// Divergence note (server wins): the server always sends `mtime` as a
/// number — 0 placeholder for symlinks and raced deletions
/// (file-endpoint.ts `ListEntry`); the TS type keeps it optional for older
/// bridges. Kept optional here for the same tolerance.
///
/// Both numbers ride the tolerant epoch-ms decoders: `mtime` is Node's raw
/// `stat().mtimeMs`, which carries sub-millisecond FRACTIONS on APFS (the
/// bridge floors it for its own comparisons but ships the list raw), and a
/// strict `u64` rejected the WHOLE listing on the first fractional stamp —
/// the files panel read "invalid type: floating point, expected u64" for
/// every real directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsEntry {
    pub name: String,
    pub kind: FsEntryKind,
    #[serde(deserialize_with = "deserialize_epoch_ms")]
    pub size: u64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_epoch_ms_opt"
    )]
    pub mtime: Option<u64>,
}

/// /fs/list response — one directory level per call; entries sort dirs-first
/// in byte order server-side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsListing {
    pub root: String,
    pub entries: Vec<FsEntry>,
    pub truncated: bool,
}
