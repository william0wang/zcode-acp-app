//! ZCode configuration API (bridge 0.47.0, server ADR-0025) — write bodies,
//! effect classes, and the settings screens' read payloads, ported from
//! `src/lib/types.ts`.
//!
//! The payloads are deliberately loose where the bridge is free to add fields
//! (models, skills, hooks, agents): unknown keys must survive a round trip,
//! and a strict client type would silently drop them on the way back out.
//! Only the fields the app reads or writes are named.
//!
//! Server references: `src/remote/settings-endpoint.ts` (route table and
//! response envelopes), `src/settings/usage-stats.ts` (UsageSnapshot),
//! `src/quota/platform-usage.ts` (PlatformUsageResult),
//! `src/settings/coding-plan.ts` (ResetCardStatus),
//! `src/settings/app-update.ts` (InstallState).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a configuration write costs to take effect (server `Effect`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Effect {
    Immediate,
    NeedsRestart,
}

/// Every write answers with its effect class. Failures carry `ok: false` plus
/// a human-readable `error` (settings-endpoint.ts `sendError`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteEffect {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<Effect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The answer to `POST /settings/models/test` (server
/// `settings/model-connectivity.ts` `ConnectivityResult`).
///
/// A failed probe is a normal answer: the provider's own message rides
/// `message`, and the `reason` field is set only when no request could be sent
/// at all (an unfinished form is the usual cause), which the UI words
/// differently from "the provider said no".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ConnectivityResult {
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

/// Body of `POST /settings/models` — an upsert: it adds a model to a provider
/// or edits its personal rule. Only the fields present change; the route maps
/// `contextWindow` into `properties` and `reasoningLevels` into `optionSpecs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelUpsert {
    pub provider_id: String,
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_levels: Option<Vec<String>>,
}

/// Body of `PUT /settings/mcp/{name}`. The route MERGES this into the
/// existing entry, so only the fields the form actually edits are sent.
/// `type` decides which transport fields the runtime reads: stdio uses
/// command/args/env, http and sse use url/headers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct McpServerUpsert {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub server_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// Body of `PUT /settings/hooks/{event}/{matcherIndex}` (`hookIndex` rides in
/// the body — the path takes only two segments). The route edits ONE existing
/// entry and merges the patch: `enabled: true` removes the pin rather than
/// storing it, `timeoutMs` (ms) wins over `timeout` (seconds) when both exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HookEntryPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

/// Three-state optional: `T | null` with the key itself optional — `None`
/// omits the key (via the field's `skip_serializing_if`), `Some(None)` sends
/// JSON null (the documented "clear" spelling), `Some(Some(v))` sends the
/// value. The visitor pattern (serde_with::rust::double_option) is required
/// so a PRESENT null decodes as `Some(None)` instead of collapsing onto the
/// absent case.
// The double-option visitor is shared with the workflow crate section
// (amend-run settings carries the same three-state fields).
pub(crate) mod double_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::marker::PhantomData;

    // Generic over T: the workflow amend-run body reuses this helper with
    // numeric fields (`maxConcurrency: number | null`), not just strings.
    pub fn serialize<S, T>(value: &Option<Option<T>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: Serialize,
    {
        match value {
            Some(Some(v)) => v.serialize(serializer),
            // Outer None never reaches here: the field's skip_serializing_if
            // omits the key first. Serialize as null defensively anyway.
            _ => serializer.serialize_none(),
        }
    }

    struct SomeNoneVisitor<T>(PhantomData<T>);

    impl<'de, T> serde::de::Visitor<'de> for SomeNoneVisitor<T>
    where
        T: Deserialize<'de>,
    {
        type Value = Option<Option<T>>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a value, null, or nothing")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(Some(None))
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(Some(None))
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            T::deserialize(deserializer).map(|v| Some(Some(v)))
        }
    }

    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        deserializer.deserialize_option(SomeNoneVisitor(PhantomData))
    }
}

/// Body of `PUT /settings/agents/{name}`. A personal agent patches frontmatter
/// fields (`model`/`thoughtLevel` accept null to clear the key — modeled as
/// `Some(None)` here); a built-in agent instead reads a model override, and
/// `providerId`/`modelId` must be null TOGETHER to clear it. A PUT on a
/// missing personal agent creates it — then `description` is required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentUpsert {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub model: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub thought_level: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub provider_id: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub model_id: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_level: Option<String>,
    /// The system-prompt body; `Some(None)` clears it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub system_prompt: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub tools: Option<Option<Vec<String>>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub disallowed_tools: Option<Option<Vec<String>>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub skills: Option<Option<Vec<String>>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub permission_mode: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub max_turns: Option<Option<u32>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub background: Option<Option<bool>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "double_option"
    )]
    pub inject_agents_md: Option<Option<bool>>,
}

/// The two card windows the reset API sells (server `ResetType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResetCardType {
    #[serde(rename = "FIVE_HOUR")]
    FiveHour,
    #[serde(rename = "WEEK")]
    Week,
}

impl ResetCardType {
    /// The quota item key each window clears (server quota/parse.ts
    /// deriveLabel: `token_5h` / `token_week`).
    pub fn window_key(self) -> &'static str {
        match self {
            ResetCardType::FiveHour => "token_5h",
            ResetCardType::Week => "token_week",
        }
    }
}

/// Body of `POST /settings/reset-cards/use`. The `nonce` must be the one the
/// status read minted moments earlier, and `idempotencyKey` must be reused on
/// every retry of the same gesture — the backend keys the spend on it, so a
/// fresh key per attempt defeats the dedupe and can burn a second card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpendResetCard {
    pub provider_id: String,
    pub reset_type: ResetCardType,
    pub nonce: String,
    pub idempotency_key: String,
}

/// Body of `POST /settings/reset-cards/opportunity` (input half).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetOpportunityRequest {
    pub provider_id: String,
    pub idempotency_key: String,
}

/// The backend's answer to whether a reset would be granted right now. A
/// denial is a normal answer carrying the next time to try, not an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetOpportunity {
    pub granted: bool,
    /// Epoch-ms countdown seed; JSON null when there is nothing to count.
    pub next_try_at: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetOpportunityResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opportunity: Option<ResetOpportunity>,
}

/// `GET /api/instances/{id}/settings/pending-restart` — whether that bridge
/// has recorded a needs-restart write since its last restart, and how many.
/// Per-instance on purpose: the hub-local spelling answers 409, because the
/// hub has no backend whose restart could clear its own answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingRestartStatus {
    pub pending_restart: bool,
    pub writes: u64,
}

/// `POST /api/instances/{id}/backend/restart` (server handleBackendRestart,
/// settings-endpoint.ts:1369 — always sends ok/cancelledTurns/closed).
/// `cancelled_turns` is how many conversations were interrupted; `closed` is
/// whether the old subprocess actually went away — false means the writes
/// this restart existed to apply did NOT take effect. `closed` defaults to
/// false when a caller omits it (tolerance; the server never does).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendRestartResult {
    pub cancelled_turns: u64,
    #[serde(default)]
    pub closed: bool,
}

/// One-shot snapshot of every section, for a config page's first paint.
///
/// The two optional sections degrade independently: a machine that never ran
/// an agent has no local database, and one that never linked a coding plan
/// has no decryptable credential store — both are normal, not errors.
///
/// Divergence note: the hub's machine-level mount reports the workflow gate
/// as a bare `{available:false}` (settings-endpoint.ts:675) — a DIFFERENT
/// shape the TS app does not model either; the app reads the per-instance
/// mount (`/api/instances/{id}/settings/all`), whose block is
/// [`WorkflowGateBlock`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsAll {
    pub ok: bool,
    pub models: Value,
    pub skills: Value,
    pub mcp: Value,
    pub hooks: Value,
    pub agents: Value,
    /// The server always sends a full UsageSnapshot here (degrading to
    /// `available:false` zeroes) — the TS type's `| null` never occurs.
    #[serde(default)]
    pub usage: Option<SettingsUsage>,
    pub reset_cards: ResetCardEligibility,
    /// Dynamic-workflow gate (bridge 0.48.0, ADR-0029). Absent on an older
    /// bridge — read as "off", which hides the workflows section.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<SettingsWorkflowBlock>,
}

/// `/settings/all`'s workflow block: two mounts, two shapes. The per-instance
/// mount (a live bridge) reports the pinned verdict ([`WorkflowGateBlock`]);
/// the hub's machine-level mount has no backend and degrades to a bare
/// `{available:false}`. Both must decode — the machine-level snapshot feeds
/// the plan-quota page, and a strict `WorkflowGateBlock` failed that whole
/// snapshot on the degrade shape (the "quota page unusable" bug, 2026-10-22).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SettingsWorkflowBlock {
    Verdict(WorkflowGateBlock),
    /// The machine-level degrade: no gate on this mount, no verdict to read.
    Unavailable {
        available: bool,
    },
}

impl SettingsWorkflowBlock {
    /// The pinned verdict, or `None` for the machine-level degrade block.
    pub fn verdict(self) -> Option<WorkflowGateBlock> {
        match self {
            SettingsWorkflowBlock::Verdict(v) => Some(v),
            SettingsWorkflowBlock::Unavailable { .. } => None,
        }
    }
}

/// Which providers could spend a reset card, and whether the machine has a
/// decryptable credential store (settings-endpoint.ts `resetCardEligibility`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetCardEligibility {
    pub providers: Vec<String>,
    pub credentials: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A local workflow-gate override position (bridge ≥0.53).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GateOverride {
    Disabled,
    OnDemand,
    AlwaysOn,
}

/// The gate verdict block (per-instance GET /settings/all and PUT
/// /settings/workflow-gate). `source:"override"` + `override` mark a local
/// override; `mode:"unknown"` means the verdict could not be resolved
/// (fail-closed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowGateBlock {
    pub enabled: bool,
    pub mode: String,
    pub source: String,
    /// Present only under a local override: which position produced this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#override: Option<GateOverride>,
}

/// The switch's write vocabulary (bridge ≥0.53): "auto" follows the remote
/// verdict (server `WorkflowGateModeSetting`, settings/workflow.ts:132).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkflowGateModeSetting {
    Auto,
    Disabled,
    OnDemand,
    AlwaysOn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowGateSetResponse {
    pub ok: bool,
    /// The effective verdict the next gate read sees.
    pub gate: WorkflowGateBlock,
}

/// Usage range of the local-machine usage snapshot (server `UsageRange`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsageRange {
    #[serde(rename = "7d")]
    SevenDays,
    #[serde(rename = "30d")]
    ThirtyDays,
    #[serde(rename = "all")]
    All,
}

impl UsageRange {
    /// The wire spelling (also the UI range key the store tracks).
    pub fn as_str(self) -> &'static str {
        match self {
            UsageRange::SevenDays => "7d",
            UsageRange::ThirtyDays => "30d",
            UsageRange::All => "all",
        }
    }
}

/// Per-model row of the usage snapshot (server `ModelUsageRow`).
///
/// Divergence note (server wins): the TS type names only modelId/
/// totalTokens plus optionals; the server always sends every counter and a
/// `share` fraction, and `modelId` is nullable (`| null`), not required —
/// the Rust type models the server shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsageRow {
    pub model_id: Option<String>,
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_read_tokens: u64,
    /// Divergence note: absent from the TS type; the server always sends it.
    pub cache_creation_tokens: u64,
    pub request_count: u64,
    /// Fraction of the window's total tokens, 0..1.
    pub share: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyModelRow {
    pub model_id: Option<String>,
    pub total_tokens: u64,
}

/// One local calendar day bucket (`YYYY-MM-DD`, process timezone).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DailyUsageRow {
    pub date: String,
    pub models: Vec<DailyModelRow>,
}

/// The window summary (server `UsageSnapshot.summary`).
///
/// Divergence note (server wins): the TS type names only totalTokens/
/// requestCount/models; the server sends all eleven counters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSummary {
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_read_tokens: u64,
    pub request_count: u64,
    pub session_count: u64,
    pub turn_count: u64,
    pub tool_call_count: u64,
    pub models: u64,
    pub active_days: u64,
}

/// Local machine usage (GET /settings/usage and the `/settings/all`
/// snapshot's `usage` section; server `UsageSnapshot`, usage-stats.ts:106).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsUsage {
    /// false when the database is absent — an empty state, not an error.
    pub available: bool,
    pub range: UsageRange,
    pub summary: UsageSummary,
    pub models: Vec<ModelUsageRow>,
    pub daily: Vec<DailyUsageRow>,
}

// ---- platform usage (GET /settings/usage-platform) ----
// The account-level monitor data behind the desktop app's Coding Plan usage
// tab: a fixed 365-day activity window plus range-scoped credit/model/tool
// detail. Server `PlatformUsageResult` (quota/platform-usage.ts:96); only
// `kind === "success"` carries the optional blocks.

/// One heatmap square. `level` is 0-4 (0 = no usage), precomputed by the bridge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformUsageHeatmapCell {
    pub date: String,
    pub level: u64,
    pub tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformUsageSummary {
    pub total_tokens: u64,
    pub peak_daily_tokens: u64,
    pub peak_daily_tokens_date: Option<String>,
    pub total_usage_duration_ms: u64,
    pub current_streak_days: u64,
    pub longest_streak_days: u64,
}

/// Sunday-start weeks; days missing from the series are JSON null cells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformUsageWeek {
    pub days: Vec<Option<PlatformUsageHeatmapCell>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformUsageHeatmap {
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub max_tokens: u64,
    pub weeks: Vec<PlatformUsageWeek>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformUsageActivity {
    pub summary: PlatformUsageSummary,
    pub heatmap: PlatformUsageHeatmap,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformUsageDetailSummary {
    /// Percent, e.g. 72.4; null when the upstream did not report it.
    pub cache_hit_rate: Option<f64>,
    /// Period-over-period change of the metric; sign carries direction.
    pub cache_hit_rate_trend: Option<f64>,
    pub total_credits: f64,
    pub total_credits_trend: Option<f64>,
    pub average_daily_credits: f64,
    pub average_daily_credits_trend: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformUsageModelRow {
    pub name: String,
    pub total_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_credits: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uncached_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    pub sort_order: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformUsageToolRow {
    pub name: String,
    pub total_usage_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_credits: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlatformSeriesGranularity {
    Hour,
    Day,
}

/// Aggregate tokens per day (or per hour today) over the selected range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformUsageSeries {
    pub granularity: PlatformSeriesGranularity,
    pub x_time: Vec<String>,
    pub totals: Vec<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformUsageRange {
    Today,
    #[serde(rename = "7d")]
    SevenDays,
    #[serde(rename = "30d")]
    ThirtyDays,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsPlatformUsage {
    /// "success" | "auth_error" | "rate_limited" | "unavailable" — kept
    /// permissive (String) like the TS type.
    pub kind: String,
    pub range: PlatformUsageRange,
    pub generated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<PlatformUsageActivity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<PlatformUsageDetail>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<PlatformUsageModelRow>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<PlatformUsageToolRow>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series: Option<PlatformUsageSeries>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlatformUsageDetail {
    pub model: Option<PlatformUsageDetailSummary>,
    pub tool: Option<PlatformUsageDetailSummary>,
}

/// The reset-card inventory (GET /settings/reset-cards?providerId=…; server
/// `ResetCardStatus`, coding-plan.ts:59 — shape identical to the TS type).
/// A card carries an expiry and nothing else — the API exposes no card
/// number or denomination, so a list can only be counted and dated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetCardStatus {
    pub ok: bool,
    pub reset_cards: ResetCardInventory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetCardInventory {
    pub available_five_hour: Vec<ResetCardExpiry>,
    pub available_week: Vec<ResetCardExpiry>,
    pub latest_five_hour: Option<ResetCardUse>,
    pub latest_week: Option<ResetCardUse>,
    pub has_unread_history: bool,
    /// Send back verbatim with a spend; ties it to this status read.
    pub nonce: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetCardExpiry {
    pub expire_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetCardUse {
    pub used_at: u64,
}

/// The install outcome of an app update, as the bridge reports it (server
/// `InstallStage`, app-update.ts:528).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AppUpdateStage {
    Idle,
    Downloading,
    Installing,
    Done,
    NeedsUserInstall,
    Failed,
}

/// The bridge-side install state (server `InstallState`, app-update.ts:531 —
/// shape identical to the TS `AppUpdateState`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppUpdateState {
    pub stage: AppUpdateStage,
    /// JSON null when no version is in flight.
    pub version: Option<String>,
    pub received_bytes: u64,
    pub total_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_required: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
