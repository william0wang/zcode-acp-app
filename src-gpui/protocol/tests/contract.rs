//! Contract tests: JSON round-trips for every domain/message type ported
//! from `src/lib/types.ts` into the `protocol` crate.
//!
//! ## Fixture provenance (deviation from the task's premise, reported)
//!
//! The task named `/Users/william/Develop/tools/zcode-acp-server/docs/fixtures`
//! as the sample source. That directory DOES NOT EXIST in the server repo
//! (checked on disk and in git history — `docs/` holds only adr/, agents/,
//! proposals/ plus markdown docs; martty's `docs/fixtures/` are TUI theme
//! skins, unrelated). The samples below were therefore DERIVED from the
//! server's TypeScript sources — the same arbiter the task names for
//! divergences — at the sites cited per family:
//!
//! - instances.json            — remote/hub-server.ts:1734-1746 (row map),
//!                               :419-439 (validSessions); `nonce` rides the
//!                               TS index signature (InstanceEntry carries it
//!                               internally; additive-only contract).
//! - projects.json             — tasks-index.ts:595-599 (KnownWorkspace).
//! - fs-browse*.json           — remote/hub-server.ts:1920-1928.
//! - instance-create.json      — hub-server.ts joinIncubation `{id, reused}`.
//! - projects-sessions.json    — hub-server.ts:2030-2038 (hub wrapper) +
//!                               remote/session-list-endpoint.ts:162-167
//!                               (bridge rows: store spread + live/running
//!                               + optional workflowActivity).
//! - upgrade.json              — hub-server.ts:2180-2187, reason vocabulary
//!                               from diskCodeIsNewer (:1098-1103).
//! - fs-listing.json           — remote/file-endpoint.ts:187-192, 245.
//! - quota.json                — handlers/account.ts:123-128 (sections),
//!                               quota/types.ts:38-55 (QuotaItem),
//!                               account.ts:57-121 (Oc/Cp entries).
//! - settings-all.json         — remote/settings-endpoint.ts:631-672,
//!                               usage snapshot from settings/usage-stats.ts:106-125.
//! - settings-usage.json       — settings/usage-stats.ts:87-125, 161-281.
//! - usage-platform.json       — quota/platform-usage.ts:42-105, 657-669.
//! - reset-cards.json          — settings/coding-plan.ts:59-72, 219-260.
//! - app-update.json           — settings/app-update.ts:528-552.
//! - workflow-*.json, run-*,   — settings/workflow.ts (v4 passthroughs +
//!   conversation-runs,          command results), remote/workflow-overview.ts:48-90,
//!   artifact-*, workspace,      371-457; the TS types ARE the contract for the
//!   node-result                 passthrough families.
//! - system-stats.json         — remote/system-stats.ts:30-103, 453-494.
//! - session-updates.json      — handlers/dispatch.ts:165-168, 446-450;
//!                               handlers/io.ts:216-226; acp.ts wire samples.
//! - Client-state fixtures (connection-profile, saved-server, chat-message-*,
//!   prompt-draft, turn-stats, *-upsert, hook-patch) — the TS app's own
//!   persisted/request shapes from src/lib/types.ts.
//!
//! ## Recorded divergences (server src wins; TS type recorded for context)
//!
//! Each entry: TS `src/lib/types.ts` vs server `zcode-acp-server/src`.
//! The Rust type follows the SERVER; tolerance notes explain any Option.
//!
//! 1. `HubUpgradeResult.reason` — TS: "version" | "mtime" | "up-to-date"
//!    (types.ts:118). Server ALSO answers "fingerprint"
//!    (hub-server.ts:1098-1103 — the content-fingerprint check runs FIRST).
//!    Rust: 4-value enum `UpgradeReason` (fixtures/upgrade.json pins
//!    "fingerprint"; upgrade-up-to-date.json pins the hyphenated "up-to-date"
//!    — the COMMON answer a plain lowercase rename would reject).
//! 2. `QuotaItem.leftPercent` — TS lacks the field (types.ts:165-173);
//!    server always sends it (quota/types.ts:46). Rust adds it, optional so
//!    older-hub payloads still decode (fixtures/quota.json carries it).
//! 3. `AccountUsageStats.ollama` — TS optional (types.ts:212, bridge <0.41
//!    tolerance); server ALWAYS sends it (handlers/account.ts:126). Rust
//!    keeps it optional for cross-version payloads; current hubs satisfy it.
//! 4. `AccountUsageStats.cliproxy` — TS has NO such section; server added it
//!    with the CLIProxyAPI quota card (handlers/account.ts:116-127; release
//!    0.66 "feat: add the CLIProxyAPI quota card"). Rust models
//!    `CpUsageStats` (+ windows/accounts/combined/otherAccounts) as an
//!    optional section.
//! 5. `OllamaUsageStats.usage` — TS has no spend detail; server attaches
//!    `OcUsageDetail` when the analytics API reports a spend
//!    (handlers/account.ts:71-88). Rust adds it.
//! 6. `SettingsUsage` — TS names a 3-field summary and a non-nullable
//!    `modelId` with optional counters (types.ts:467-485); the server sends
//!    an 11-field summary, every counter required, and a NULLABLE modelId,
//!    plus `cacheCreationTokens` (settings/usage-stats.ts:87-125). Rust
//!    follows the server shape.
//! 7. `SettingsAll.usage` — TS `SettingsUsage | null`; the server never
//!    sends null (it degrades to `available:false` zeroes,
//!    settings-endpoint.ts:656-662). Rust keeps the nullable spelling.
//! 8. `SettingsAll.workflow` — the PER-INSTANCE mount's block is
//!    `WorkflowGateBlock` (settings-endpoint.ts:681-708); the hub's
//!    machine-level mount instead reports `{available:false}` (:675) — a
//!    different shape the TS app deliberately does not model (it reads the
//!    per-instance route, src/lib/hub.ts:722-741). Rust models BOTH via the
//!    untagged `SettingsWorkflowBlock`: the machine-level snapshot feeds the
//!    plan-quota page's eligibility read, and a strict `WorkflowGateBlock`
//!    failed that whole snapshot's decode on the degrade shape.
//! 9. `FsEntry.mtime` — TS optional; the server always sends a number (0
//!    placeholders for symlinks/raced deletions, file-endpoint.ts:187-192,
//!    226-243). Rust keeps it optional for older bridges; the fixture
//!    carries mtime on every entry including the symlink's 0.
//! 10. `HubInstance.port/pid/startedAt/workspace/origin` — TS optional
//!     (older-bridge tolerance); the current hub always sends them
//!     (hub-server.ts:1734-1744). Rust keeps the TS optionality;
//!     `origin` defaults to "editor" exactly like the TS "treat absent as
//!     editor" reading.
//! 11. `HubHistoryPage.instance.origin` — same as 10 for the history
//!     wrapper (hub-server.ts:2033 always sends it).
//! 12. `SettingsPlatformUsage`, `ResetCardStatus`, `AppUpdateState`,
//!     `SystemStatsResponse`, `WorkflowOverviewResponse`, the workflow
//!     run/artifact/workspace families, `ConfigOption`, `ContextUsage`,
//!     `SlashCommand`, `FsBrowseResult`, `HubProject`, connection/chat
//!     client types — no divergence found between types.ts and the server
//!     sources read for this port (see provenance above for the sites).
//! 13. Epoch-ms number fields (`updatedAt`, `startedAt`, `lastActive`,
//!     history-cursor `before`) — TS says `number`; the server's Bun runtime
//!     serializes `Date.now()`-derived values with a FRACTIONAL part
//!     (observed live on /api/instances: `1792045487151.9006`, 2026-10-15,
//!     desktop M1 smoke). Rust u64 fields deserialize tolerantly (fraction
//!     truncated); pinned by `epoch_ms_fields_accept_fractional_numbers`.
//!
//! ## Round-trip rule
//!
//! A fixture passes when it (a) DESERIALIZES into the typed model,
//! (b) the model SERIALIZES back to JSON, (c) that JSON DESERIALIZES again
//! into an equal typed value, and (d) the re-emitted JSON equals the
//! original sample under numeric-tolerant deep equality (JSON `1` and `1.0`
//! are the same number; object key order is not significant).

use serde::Deserialize;
use serde_json::Value;
use std::fmt::Debug;

use protocol::acp::{ConfigOption, ContextUsage, SessionUpdate, SlashCommand};
use protocol::chat::{ChatMessage, PromptDraft, TurnStats};
use protocol::hub::{
    ConnectionProfile, FsBrowseResult, FsListing, HubCreateInstanceResult, HubHistoryCursor,
    HubHistoryPage, HubInstance, HubProject, HubUpgradeResult, SavedServer,
};
use protocol::quota::AccountUsageStats;
use protocol::settings::{
    AgentUpsert, AppUpdateState, HookEntryPatch, McpServerUpsert, ModelUpsert, ResetCardStatus,
    SettingsAll, SettingsPlatformUsage, SettingsUsage, WorkflowGateSetResponse, WriteEffect,
};
use protocol::system_stats::SystemStatsResponse;
use protocol::workflow::{
    AmendRunSettingsResponse, ConversationRunsResponse, WorkflowArgDeclaration, WorkflowArtifact,
    WorkflowArtifactItemsResponse, WorkflowArtifactReadResponse, WorkflowCreatePromptResponse,
    WorkflowDetailResponse, WorkflowListResponse, WorkflowNodeResultResponse,
    WorkflowOverviewResponse, WorkflowRunEventsResponse, WorkflowRunsHistoryResponse,
    WorkflowStartResponse, WorkflowWorkspaceResponse,
};

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{}", env!("CARGO_MANIFEST_DIR"), name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// Deep equality with numeric tolerance: serde_json distinguishes `1` from
/// `1.0` lexically, but a u64-then-f64 round trip of the same JSON number is
/// not drift.
fn value_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(p), Some(q)) => p == q,
            _ => x == y,
        },
        (Value::Object(m1), Value::Object(m2)) => {
            m1.len() == m2.len()
                && m1
                    .iter()
                    .all(|(k, v)| m2.get(k).is_some_and(|w| value_eq(v, w)))
        }
        (Value::Array(a1), Value::Array(b1)) => {
            a1.len() == b1.len() && a1.iter().zip(b1).all(|(x, y)| value_eq(x, y))
        }
        _ => a == b,
    }
}

fn assert_round_trip<T>(file: &str)
where
    T: for<'de> Deserialize<'de> + serde::Serialize + PartialEq + Debug,
{
    let raw = fixture(file);
    let original: Value = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("fixture {file} is not valid JSON: {e}"));
    // (a) deserialize the sample into the typed model.
    let typed: T = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("decode {} from {file}: {e}", type_name::<T>()));
    // (b) serialize the typed model back to JSON.
    let emitted =
        serde_json::to_string(&typed).unwrap_or_else(|e| panic!("serialize {}: {e}", file));
    // (c) deserialize AGAIN — the emitted JSON must decode to the same value.
    let retyped: T = serde_json::from_str(&emitted)
        .unwrap_or_else(|e| panic!("re-decode {} from {file}: {e}", type_name::<T>()));
    assert_eq!(typed, retyped, "typed round-trip drifted for {file}");
    // (d) no JSON drift against the original sample.
    let reemitted: Value = serde_json::from_str(&emitted).expect("re-emitted JSON is valid");
    assert!(
        value_eq(&original, &reemitted),
        "JSON drift for {file}:\noriginal:  {original}\nre-emitted: {reemitted}"
    );
}

fn type_name<T>() -> &'static str {
    std::any::type_name::<T>()
}

// ─── hub discovery / history / fs ────────────────────────────────────────────

#[test]
fn instances_round_trip() {
    assert_round_trip::<Vec<HubInstance>>("instances.json");
}

/// Divergence record #13: the live hub's Bun runtime serializes
/// `Date.now()`-derived epoch-ms fields with a fractional part — observed on
/// /api/instances as `updatedAt: 1792045487151.9006` (2026-10-15, live
/// smoke of the desktop M1 slice). The TS type says `number`, so the Rust
/// u64 fields deserialize tolerantly (fraction truncated). The fixtures keep
/// integer spellings; this test pins the fractional acceptance.
#[test]
fn epoch_ms_fields_accept_fractional_numbers() {
    let instances: Vec<HubInstance> = serde_json::from_str(
        r#"[{
            "id": "inst-1",
            "startedAt": 1792045487151.9006,
            "sessions": [
                { "sessionId": "s1", "title": "t", "updatedAt": 1792045487151.9006 }
            ]
        }]"#,
    )
    .expect("fractional epoch-ms must deserialize");
    assert_eq!(instances[0].started_at, Some(1792045487151));
    assert_eq!(instances[0].sessions[0].updated_at, Some(1792045487151));

    let projects: Vec<HubProject> =
        serde_json::from_str(r#"[{ "workspacePath": "/w", "sessions": 1, "lastActive": 1.5 }]"#)
            .expect("fractional lastActive must deserialize");
    assert_eq!(projects[0].last_active, 1);

    let cursor: HubHistoryCursor =
        serde_json::from_str(r#"{ "before": 1792045487151.9, "beforeId": "s1" }"#)
            .expect("fractional cursor before must deserialize");
    assert_eq!(cursor.before, 1792045487151);

    // Junk stays rejected — a string is not a number.
    assert!(serde_json::from_str::<Vec<HubInstance>>(
        r#"[{ "id": "i", "sessions": [{ "sessionId": "s", "updatedAt": "soon" }] }]"#
    )
    .is_err());
}

/// The per-session /fs/list ships Node's raw `stat().mtimeMs` — fractional
/// on APFS — and a strict u64 rejected the WHOLE listing on the first such
/// stamp (the files panel showed "expected u64" for every real directory,
/// 2026-10-19). Fractions floor; the TS client never noticed because a JS
/// number is a JS number.
#[test]
fn fs_list_entries_accept_fractional_mtime_and_size() {
    let listing: protocol::hub::FsListing = serde_json::from_str(
        r#"{
            "root": "/w",
            "entries": [
                { "name": "src", "kind": "dir", "size": 96, "mtime": 1760000000000.456 },
                { "name": "a.rs", "kind": "file", "size": 1234.0, "mtime": 1760000000001 },
                { "name": "lnk", "kind": "symlink", "size": 0, "mtime": 0 },
                { "name": "old", "kind": "file", "size": 1 }
            ],
            "truncated": false
        }"#,
    )
    .expect("fractional mtime must not reject the listing");
    assert_eq!(listing.entries[0].mtime, Some(1760000000000));
    assert_eq!(listing.entries[1].size, 1234);
    assert_eq!(listing.entries[1].mtime, Some(1760000000001));
    assert_eq!(listing.entries[2].mtime, Some(0));
    assert_eq!(listing.entries[3].mtime, None, "older bridges omit mtime");
}

#[test]
fn projects_round_trip() {
    assert_round_trip::<Vec<HubProject>>("projects.json");
}

#[test]
fn fs_browse_round_trip() {
    assert_round_trip::<FsBrowseResult>("fs-browse.json");
}

#[test]
fn fs_browse_root_round_trip() {
    // Exercises `parent: null` at the filesystem root (hub-server.ts:1924).
    assert_round_trip::<FsBrowseResult>("fs-browse-root.json");
}

#[test]
fn instance_create_round_trip() {
    assert_round_trip::<HubCreateInstanceResult>("instance-create.json");
}

#[test]
fn projects_sessions_round_trip() {
    assert_round_trip::<HubHistoryPage>("projects-sessions.json");
}

#[test]
fn upgrade_round_trip() {
    // Divergence #1: reason "fingerprint" exists only on the server side.
    assert_round_trip::<HubUpgradeResult>("upgrade.json");
}

/// The COMMON verdict (nothing newer on disk): the server hyphenates
/// "up-to-date", so this fixture pins the spelling a plain lowercase enum
/// rename would reject — the desktop's upgrade check errored on every
/// up-to-date hub before the explicit `rename`.
#[test]
fn upgrade_up_to_date_round_trip() {
    assert_round_trip::<HubUpgradeResult>("upgrade-up-to-date.json");
}

#[test]
fn fs_listing_round_trip() {
    assert_round_trip::<FsListing>("fs-listing.json");
}

// ─── connection / client state ───────────────────────────────────────────────

#[test]
fn connection_profile_round_trip() {
    assert_round_trip::<ConnectionProfile>("connection-profile.json");
}

#[test]
fn saved_server_round_trip() {
    assert_round_trip::<SavedServer>("saved-server.json");
}

// ─── ACP wire ────────────────────────────────────────────────────────────────

#[test]
fn session_updates_round_trip() {
    // Unknown kinds (future_update_kind) must survive via the flatten map —
    // the permissive-wire doctrine (types.ts:223-225).
    assert_round_trip::<Vec<SessionUpdate>>("session-updates.json");
}

#[test]
fn context_usage_round_trip() {
    assert_round_trip::<ContextUsage>("context-usage.json");
}

#[test]
fn config_option_round_trip() {
    assert_round_trip::<ConfigOption>("config-option.json");
}

#[test]
fn slash_command_round_trip() {
    assert_round_trip::<SlashCommand>("slash-command.json");
}

// ─── chat model ──────────────────────────────────────────────────────────────

#[test]
fn chat_message_user_round_trip() {
    assert_round_trip::<ChatMessage>("chat-message-user.json");
}

#[test]
fn chat_message_assistant_round_trip() {
    // Exercises the tool-call tag plus embedded diff blocks.
    assert_round_trip::<ChatMessage>("chat-message-assistant.json");
}

#[test]
fn chat_message_attachment_round_trip() {
    // Exercises the image part and the replay `collapsed` flag.
    assert_round_trip::<ChatMessage>("chat-message-attachment.json");
}

#[test]
fn prompt_draft_round_trip() {
    assert_round_trip::<PromptDraft>("prompt-draft.json");
}

#[test]
fn turn_stats_round_trip() {
    assert_round_trip::<TurnStats>("turn-stats.json");
}

// ─── settings ────────────────────────────────────────────────────────────────

#[test]
fn settings_all_round_trip() {
    // Divergences #6-#8 (usage snapshot shape, workflow gate mount).
    assert_round_trip::<SettingsAll>("settings-all.json");
}

#[test]
fn settings_usage_round_trip() {
    // Divergence #6: the server's 11-field summary + nullable modelId +
    // cacheCreationTokens.
    assert_round_trip::<SettingsUsage>("settings-usage.json");
}

#[test]
fn usage_platform_round_trip() {
    assert_round_trip::<SettingsPlatformUsage>("usage-platform.json");
}

#[test]
fn reset_cards_round_trip() {
    assert_round_trip::<ResetCardStatus>("reset-cards.json");
}

#[test]
fn app_update_round_trip() {
    assert_round_trip::<AppUpdateState>("app-update.json");
}

#[test]
fn write_effect_round_trip() {
    assert_round_trip::<WriteEffect>("write-effect.json");
}

#[test]
fn model_upsert_round_trip() {
    assert_round_trip::<ModelUpsert>("model-upsert.json");
}

#[test]
fn mcp_upsert_round_trip() {
    assert_round_trip::<McpServerUpsert>("mcp-upsert.json");
}

#[test]
fn hook_patch_round_trip() {
    assert_round_trip::<HookEntryPatch>("hook-patch.json");
}

#[test]
fn agent_upsert_round_trip() {
    // `"model": null` is the documented CLEAR spelling (three-state field):
    // the double_option port must keep it a JSON null, not drop the key.
    assert_round_trip::<AgentUpsert>("agent-upsert.json");
}

#[test]
fn workflow_gate_set_round_trip() {
    assert_round_trip::<WorkflowGateSetResponse>("workflow-gate-set.json");
}

// ─── quota ───────────────────────────────────────────────────────────────────

#[test]
fn quota_round_trip() {
    // Divergences #2-#5 (leftPercent, cliproxy section, ollama usage detail).
    assert_round_trip::<AccountUsageStats>("quota.json");
}

// ─── workflow management ─────────────────────────────────────────────────────

#[test]
fn workflow_list_round_trip() {
    assert_round_trip::<WorkflowListResponse>("workflow-list.json");
}

#[test]
fn workflow_detail_round_trip() {
    assert_round_trip::<WorkflowDetailResponse>("workflow-detail.json");
}

#[test]
fn workflow_runs_round_trip() {
    assert_round_trip::<WorkflowRunsHistoryResponse>("workflow-runs.json");
}

#[test]
fn workflow_overview_round_trip() {
    assert_round_trip::<WorkflowOverviewResponse>("workflow-overview.json");
}

#[test]
fn amend_run_settings_round_trip() {
    assert_round_trip::<AmendRunSettingsResponse>("amend-run-settings.json");
}

#[test]
fn conversation_runs_round_trip() {
    assert_round_trip::<ConversationRunsResponse>("conversation-runs.json");
}

#[test]
fn run_events_round_trip() {
    assert_round_trip::<WorkflowRunEventsResponse>("run-events.json");
}

#[test]
fn artifact_markdown_round_trip() {
    assert_round_trip::<WorkflowArtifact>("artifact-markdown.json");
}

#[test]
fn artifact_board_round_trip() {
    // `columns` rides the TS index signature — unknown keys survive.
    assert_round_trip::<WorkflowArtifact>("artifact-board.json");
}

#[test]
fn artifact_items_round_trip() {
    assert_round_trip::<WorkflowArtifactItemsResponse>("artifact-items.json");
}

#[test]
fn artifact_read_round_trip() {
    assert_round_trip::<WorkflowArtifactReadResponse>("artifact-read.json");
}

#[test]
fn workspace_round_trip() {
    assert_round_trip::<WorkflowWorkspaceResponse>("workspace.json");
}

#[test]
fn node_result_round_trip() {
    assert_round_trip::<WorkflowNodeResultResponse>("node-result.json");
}

#[test]
fn workflow_start_round_trip() {
    assert_round_trip::<WorkflowStartResponse>("workflow-start.json");
}

#[test]
fn workflow_create_prompt_round_trip() {
    assert_round_trip::<WorkflowCreatePromptResponse>("workflow-create-prompt.json");
}

#[test]
fn workflow_arg_declaration_round_trip() {
    assert_round_trip::<WorkflowArgDeclaration>("workflow-arg-declaration.json");
}

// ─── system stats ────────────────────────────────────────────────────────────

#[test]
fn system_stats_round_trip() {
    assert_round_trip::<SystemStatsResponse>("system-stats.json");
}
