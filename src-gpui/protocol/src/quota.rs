//! account/usage_stats (Proposal 0002) — the combined multi-provider quota
//! behind `/api/quota` and the ACP `account/usage_stats` result, ported from
//! `src/lib/types.ts`.
//!
//! Server reference: `src/handlers/account.ts` (`UsageStatsResult`) plus
//! `src/quota/types.ts` (`QuotaItem`), `src/quota/opencode-go`,
//! `src/quota/ollama-cloud`, `src/quota/cliproxy` for the section shapes.
//! GLM items pass through verbatim (counts, reset timestamps, per-model
//! details); Go windows carry the countdown resolved to absolute epoch ms.

use serde::{Deserialize, Serialize};

/// One rendered line of the GLM section (server `QuotaItem`, quota/types.ts).
///
/// Divergence note (server wins): the server ALWAYS carries `leftPercent`
/// (= 100 - usedPercent) on every item; the TS type omits it. Kept optional
/// here so older-hub payloads still decode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaItem {
    /// Stable key derived from type+number, e.g. "token_5h" / "mcp".
    pub key: String,
    /// Human-readable label for the line, e.g. "5h" / "MCP" / "Week".
    pub label: String,
    /// Used percentage, clamped to [0, 100].
    pub used_percent: f64,
    /// Remaining percentage = 100 - usedPercent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_reset_time: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Vec<QuotaModelDetail>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaModelDetail {
    pub model_code: String,
    pub usage: u64,
}

/// GLM section — `items` present only on success
/// (kind: success | auth_error | rate_limited | unavailable).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GlmUsageStats {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<QuotaItem>>,
}

/// Opencode Go window keys (server `GoWindowKey`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GoWindowKey {
    Rolling,
    Weekly,
    Monthly,
}

/// One Opencode Go window with the reset countdown resolved to epoch ms.
/// Labels follow the CLI card: rolling → "5h", weekly → "Week",
/// monthly → "Month".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoWindowEntry {
    pub key: GoWindowKey,
    pub label: String,
    pub usage_percent: f64,
    pub resets_at: u64,
}

/// Opencode Go section — `windows` present only on success
/// (kind: success | not_configured | auth_error | unavailable).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoUsageStats {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows: Option<Vec<GoWindowEntry>>,
}

/// Ollama Cloud window keys (server `OcWindowEntry["key"]`: the closed set
/// "session" | "weekly" | "monthly").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OcWindowKey {
    Session,
    Weekly,
    Monthly,
}

/// One Ollama Cloud window (bridge 0.41.0): whichever windows the account's
/// plan exposes (legacy: 5h + Week; credit plans: Month). ollama.com returns
/// fractions only — reset moments are derived hub-side (bridge 0.42.0) and
/// ride along when available.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OcWindowEntry {
    pub key: OcWindowKey,
    pub label: String,
    pub usage_percent: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
}

/// Ollama Cloud spend detail (analytics API, 2026-10+): usage in USD over the
/// rolling 30d window.
///
/// Divergence note (server wins): the whole block is absent from the TS
/// type; the server attaches it whenever the analytics API reports a spend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OcUsageDetail {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    pub usage_usd: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credit_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// The rolling window the spend covers — always "30d".
    pub window: String,
}

/// Ollama Cloud section — `windows` present only on success
/// (kind: success | not_configured | auth_error | unavailable).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OllamaUsageStats {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows: Option<Vec<OcWindowEntry>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<OcUsageDetail>,
}

/// One CLIProxy window: a model family's usage fraction as percent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CpWindowEntry {
    /// "5h" | "week" — kept permissive (String) like the other vocab fields.
    pub window: String,
    pub used_percent: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
}

/// One CLIProxy Gemini model family and its per-window usage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CpModelEntry {
    pub name: String,
    pub windows: Vec<CpWindowEntry>,
}

/// One CLIProxy Gemini account and its per-family usage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CpAccountEntry {
    pub label: String,
    pub models: Vec<CpModelEntry>,
}

/// CLIProxy section — `accounts` (per-subscription detail) and `combined`
/// (pool-wide per-window rows) present only on success.
///
/// Divergence note (server wins): the ENTIRE section is absent from the TS
/// type — the server added it with the CLIProxyAPI quota card (bridge 0.66,
/// handlers/account.ts `CpUsageStats`). Optional here so older-hub payloads
/// (the hub-first lookup tolerates them) still decode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CpUsageStats {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accounts: Option<Vec<CpAccountEntry>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub combined: Option<Vec<CpWindowEntry>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_accounts: Option<u64>,
}

/// `/api/quota` and `account/usage_stats` result (server `UsageStatsResult`,
/// handlers/account.ts:123).
///
/// Divergence note (server wins): the server now ALWAYS sends `ollama` and
/// `cliproxy`; the TS type carries `ollama?` (bridge <0.41 tolerance) and no
/// `cliproxy` at all. Both stay optional for cross-version payload tolerance,
/// exactly like the TS `ollama?`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountUsageStats {
    pub glm: GlmUsageStats,
    pub opencode: GoUsageStats,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ollama: Option<OllamaUsageStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cliproxy: Option<CpUsageStats>,
}
