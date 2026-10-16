//! The ZCode-configuration screens' pure decode/encode helpers — the Rust
//! port of the functions `tests/config-response-shapes.test.ts` and
//! `tests/config-reset-rules.test.ts` import from the TS screens
//! (`src/screens/config/*`, `src/components/config/ConfigFormSheet.tsx`).
//!
//! These are the value-level half of the settings pages: turning wire
//! payloads into rows/options/bodies. The TS file's rule — "every decode
//! function is IMPORTED from the screen that uses it, never copied" —
//! becomes, here, that the screens do not exist yet: this module IS the
//! decode, and the future gpui pages must call it, not fork it.
//!
//! Permissive-wire doctrine applies throughout: a missing section decodes
//! to an empty list / a default, never an error — the bridge is free to
//! add fields, and an older payload must still paint.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::settings::{McpServerUpsert, ResetCardType};
use crate::settings_client::encode_uri_component;

// ─── models (ModelsPage.ts) ─────────────────────────────────────────────────

/// One selectable model the bridge reports (`available[]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
}

/// One per-model rule (`modelRules[]`) — the context window and reasoning
/// levels `available` only names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_levels: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_name: Option<String>,
}

/// The GET /settings/models payload: an OBJECT with `available` +
/// `modelRules`, not an array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelsPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<ModelsBlock>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelsBlock {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available: Option<Vec<ModelRef>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub providers: Option<Vec<ProviderRef>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_rules: Option<Vec<ModelRule>>,
}

/// One picker row of the model list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOption {
    pub provider_id: String,
    pub model_id: String,
    pub label: String,
    pub reasoning_levels: Vec<String>,
}

/// One add-model picker row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderOption {
    pub id: String,
    pub name: String,
}

/// A stored model selection (the runtime's `providerId/modelId` pair).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSelection {
    pub provider_id: String,
    pub model_id: String,
}

/// A coding-plan provider the desktop manages; its models are not ours to
/// add (the write route refuses `account:*` outright).
pub fn is_account_provider(provider_id: &str) -> bool {
    provider_id.starts_with("account:")
}

/// The provider id as a MATCH key: coding-plan ids normalize to the legacy
/// `builtin:` spelling. The models payload normalizes server-side
/// (configProviderIdFor) while agent files written by the desktop keep the
/// registry's `account:` spelling — both name the same provider, and
/// comparing raw strings never matches them. Undefined collapses to "" so
/// two missing ids compare equal rather than to everything.
pub fn match_provider_id(provider_id: Option<&str>) -> String {
    let Some(pid) = provider_id else {
        return String::new();
    };
    if !pid.starts_with("account:") {
        return pid.to_string();
    }
    // ^account:([a-z0-9]+)-(?:(?:individual|team|start)-)?coding-plan$
    normalize_plan_pid(pid).unwrap_or_else(|| pid.to_string())
}

/// `account:{family}[-{individual|team|start}]-coding-plan` →
/// `builtin:{family}-coding-plan`.
fn normalize_plan_pid(pid: &str) -> Option<String> {
    let body = pid.strip_prefix("account:")?;
    let rest = body.strip_suffix("coding-plan")?.strip_suffix('-')?;
    // Strip the optional plan-kind group AND its leading dash together; the
    // no-group case leaves the family as-is (no second dash to remove).
    let family = ["individual", "team", "start"]
        .iter()
        .find_map(|group| rest.strip_suffix(group))
        .and_then(|r| r.strip_suffix('-'))
        .unwrap_or(rest);
    if family.is_empty()
        || !family
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return None;
    }
    Some(format!("builtin:{family}-coding-plan"))
}

/// The rule row for one selectable model — the prefill source for editing.
/// Public because the edit form lives in the app crate, and this module is
/// the decode the screens must call, not fork.
pub fn rule_for<'a>(model: &ModelRef, rules: &'a [ModelRule]) -> Option<&'a ModelRule> {
    let pid = match_provider_id(model.provider_id.as_deref());
    rules.iter().find(|r| {
        match_provider_id(r.provider_id.as_deref()) == pid && r.model_id == model.model_id
    })
}

/// The context window for one selectable model, joined from `modelRules` on
/// `{providerId}/{modelId}` (through `match_provider_id`, so the two halves
/// can spell one coding-plan provider differently). A model absent from the
/// rules simply has no window rather than being an error.
pub fn context_window_for(model: &ModelRef, rules: Option<&[ModelRule]>) -> Option<u64> {
    rule_for(model, rules.unwrap_or(&[]))?.context_window
}

/// Providers a new model may belong to: the rules block first, then any
/// provider the selectable list names that the block omits. `account:*`
/// providers are left out — the write route refuses them (their models
/// come from the coding plan, not this file).
pub fn provider_options(payload: Option<&ModelsPayload>) -> Vec<ProviderOption> {
    let mut out: Vec<ProviderOption> = Vec::new();
    let mut push = |id: &str, name: Option<&str>| {
        if is_account_provider(id) || out.iter().any(|p| p.id == id) {
            return;
        }
        out.push(ProviderOption {
            id: id.to_string(),
            name: name.unwrap_or(id).to_string(),
        });
    };
    if let Some(block) = payload.and_then(|p| p.models.as_ref()) {
        if let Some(providers) = block.providers.as_ref() {
            for p in providers {
                if let Some(id) = p.provider_id.as_deref() {
                    push(id, p.provider_name.as_deref());
                }
            }
        }
        if let Some(available) = block.available.as_ref() {
            for m in available {
                if let Some(id) = m.provider_id.as_deref() {
                    push(id, m.provider_name.as_deref());
                }
            }
        }
    }
    out
}

/// Every selectable model as a picker option: the value the runtime spells
/// (`providerId/modelId`), the label, and the reasoning levels its rule
/// carries. The label follows the bridge's own session-switcher rule: a
/// builtin (or coding-plan) model reads as its bare model id, a third-party
/// one is qualified with its provider, and a model id two providers share
/// is always qualified.
pub fn model_options(payload: Option<&ModelsPayload>) -> Vec<ModelOption> {
    let block = payload.and_then(|p| p.models.as_ref());
    let empty: Vec<ModelRef> = Vec::new();
    let available = block
        .and_then(|b| b.available.as_ref())
        .map(|v| v.as_slice())
        .unwrap_or(empty.as_slice());
    let rules: Vec<ModelRule> = block
        .and_then(|b| b.model_rules.clone())
        .unwrap_or_default();

    let mut by_id: HashMap<&str, usize> = HashMap::new();
    for m in available {
        if let (Some(_), Some(model_id)) = (m.provider_id.as_deref(), m.model_id.as_deref()) {
            *by_id.entry(model_id).or_insert(0) += 1;
        }
    }
    let mut seen: HashSet<(&str, &str)> = HashSet::new();
    let mut out = Vec::new();
    for m in available {
        let (Some(provider_id), Some(model_id)) = (m.provider_id.as_deref(), m.model_id.as_deref())
        else {
            continue;
        };
        if !seen.insert((provider_id, model_id)) {
            continue;
        };
        let rule = rule_for(m, &rules);
        let builtin_like =
            provider_id.starts_with("builtin:") || is_account_provider(provider_id);
        let qualify = !builtin_like || by_id.get(model_id).copied().unwrap_or(0) > 1;
        out.push(ModelOption {
            provider_id: provider_id.to_string(),
            model_id: model_id.to_string(),
            label: if qualify {
                format!(
                    "{} › {}",
                    m.provider_name.as_deref().unwrap_or(provider_id),
                    model_id
                )
            } else {
                model_id.to_string()
            },
            reasoning_levels: rule
                .and_then(|r| r.reasoning_levels.clone())
                .unwrap_or_default(),
        });
    }
    out
}

/// The picker option a stored model selection names — matching through
/// `match_provider_id`, so an `account:`-spelled agent file maps onto the
/// `builtin:`-spelled option for the same plan.
pub fn find_model_option<'a>(
    options: &'a [ModelOption],
    provider_id: &str,
    model_id: &str,
) -> Option<&'a ModelOption> {
    let key = match_provider_id(Some(provider_id));
    options
        .iter()
        .find(|o| match_provider_id(Some(&o.provider_id)) == key && o.model_id == model_id)
}

/// The frontmatter `model` value for one selection — the desktop's own
/// encoding (subagent-markdown-selection.ts): a plain `providerId/modelId`
/// stays readable, and anything the parser would misread (a provider id
/// with `/` or the `custom:` prefix) travels as
/// `custom:{encoded provider}:{encoded model}`.
pub fn encode_subagent_model(provider_id: &str, model_id: &str) -> String {
    if provider_id.starts_with("custom:")
        || provider_id.contains('/')
        || model_id.contains('$')
    {
        format!(
            "custom:{}:{}",
            encode_uri_component(provider_id),
            encode_uri_component(model_id)
        )
    } else {
        format!("{provider_id}/{model_id}")
    }
}

/// The inverse of [`encode_subagent_model`], for prefilling the picker from
/// a file the desktop app may have written. Returns None for a value this
/// build cannot map onto a picker option (a hand-written id, or the legacy
/// `inherit`-style spellings); the form then shows it read-only rather
/// than silently substituting a different model.
///
/// The legacy `custom:builtin:<family>:<model>` spelling carries an
/// UNENCODED colon inside the provider id, so the separator cannot simply
/// be the first one.
pub fn decode_subagent_model(value: Option<&str>) -> Option<ModelSelection> {
    let raw = value?.trim();
    if raw.is_empty() {
        return None;
    }
    if let Some(body) = raw.strip_prefix("custom:") {
        // TS: body.startsWith("builtin:") ? indexOf(":", 8) : indexOf(":")
        let sep = if body.starts_with("builtin:") {
            body["builtin:".len()..].find(':').map(|i| i + "builtin:".len())
        } else {
            body.find(':')
        };
        let sep = sep?;
        if sep == 0 {
            return None;
        }
        return Some(ModelSelection {
            provider_id: decode_segment(&body[..sep]),
            model_id: decode_segment(&body[sep + 1..]),
        });
    }
    let sep = raw.find('/')?;
    if sep == 0 {
        return None;
    }
    Some(ModelSelection {
        provider_id: raw[..sep].to_string(),
        // The `$level` suffix is a picker spelling the frontmatter does not
        // use; a value carrying it still names the same model.
        model_id: raw[sep + 1..].split('$').next().unwrap_or("").to_string(),
    })
}

/// decodeURIComponent that leaves a malformed escape as-is rather than
/// failing the whole decode.
fn decode_segment(value: &str) -> String {
    percent_decode(value).unwrap_or_else(|| value.to_string())
}

/// Percent-decode a JS-encoded segment; None when the escapes are malformed
/// or the bytes are not valid UTF-8.
fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if !bytes.contains(&b'%') {
        return Some(value.to_string());
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let hi = (hex[0] as char).to_digit(16)?;
            let lo = (hex[1] as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

// ─── mcp (McpPage.tsx + ConfigFormSheet.ts) ─────────────────────────────────

/// One configured server as the bridge reports it (`readMcpView`). All
/// fields optional — the route's payload is permissive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct McpServerRow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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

/// The server list out of the route's payload. The route wraps it
/// (`{ok, mcp:{servers}}`): reading `servers` off the top level yields
/// nothing and the page renders its empty state on a machine that has
/// servers configured. A missing section is an empty list, never an error.
pub fn mcp_servers(payload: Option<&Value>) -> Vec<McpServerRow> {
    payload
        .and_then(|v| v.get("mcp"))
        .and_then(|m| m.get("servers"))
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| serde_json::from_value(row.clone()).unwrap_or_default())
                .collect()
        })
        .unwrap_or_default()
}

/// The MCP form's raw fields (`McpFormFields` in the TS page).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpFormFields<'a> {
    pub server_type: &'a str,
    pub enabled: bool,
    /// http/sse (the remote transports) pick url+headers; stdio picks
    /// command+args+env. The boolean, not the type string, decides — same
    /// as the TS form.
    pub remote: bool,
    pub url: &'a str,
    pub headers_text: &'a str,
    pub command: &'a str,
    pub args_text: &'a str,
    pub env_text: &'a str,
}

/// The PUT body for one server, built from the form's raw fields.
///
/// A body carries ONLY its transport's fields: the route MERGES, so a
/// `url` landing on a stdio server (or a `command` on an http one) is
/// written next to the real transport instead of replacing it. An EMPTY
/// env/headers map is omitted — blank means "keep what the entry had",
/// while sending `{}` would clear it. Blank args ARE sent as `[]` (the
/// desktop form's own spelling for "no arguments"), unlike env.
pub fn mcp_upsert_body(f: &McpFormFields) -> McpServerUpsert {
    let kv = parse_kv_lines(if f.remote { f.headers_text } else { f.env_text });
    let kv_present = !kv.is_empty();
    let mut body = McpServerUpsert {
        server_type: Some(f.server_type.to_string()),
        enabled: Some(f.enabled),
        ..Default::default()
    };
    if f.remote {
        body.url = Some(f.url.trim().to_string());
        if kv_present {
            body.headers = Some(kv);
        }
    } else {
        body.command = Some(f.command.trim().to_string());
        body.args = Some(split_lines(f.args_text));
        if kv_present {
            body.env = Some(kv);
        }
    }
    body
}

/// Split a KEY=VALUE textarea into a record. Lines without a separator are
/// skipped rather than erroring — the user is mid-typing.
fn parse_kv_lines(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.split('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some(eq) = trimmed.find('=') else {
            continue;
        };
        if eq == 0 {
            continue;
        }
        out.insert(trimmed[..eq].trim().to_string(), trimmed[eq + 1..].to_string());
    }
    out
}

/// Non-blank lines of a one-per-line textarea.
fn split_lines(text: &str) -> Vec<String> {
    text.split('\n')
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

// ─── hooks (HooksPage.tsx) ──────────────────────────────────────────────────

/// One flattened row: an event, its matcher group, and one hook inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRow {
    /// `{event}/{matcherIndex}/{hookIndex}` — display key for the row.
    pub key: String,
    pub event: String,
    /// The three coordinates `PUT /settings/hooks/{event}/{matcherIndex}`
    /// (+ body `hookIndex`) needs.
    pub matcher_index: usize,
    pub hook_index: usize,
    pub matcher: String,
    /// "process" or "command" — decides the timeout unit of a NEW timeout.
    pub hook_type: Option<String>,
    pub command: String,
    /// Absent means on: the config spells `enabled: false` only when a hook
    /// is switched off.
    pub enabled: bool,
    /// Raw `timeoutMs` (milliseconds) when the entry spells it; wins over
    /// `timeout`.
    pub raw_timeout_ms: Option<u64>,
    /// Raw `timeout` (seconds) when the entry spells it.
    pub raw_timeout_sec: Option<u64>,
}

/// Flatten the two-level hooks tree (`events[event]` → matcher groups →
/// hook entries) into editable rows. The indexes ride along because the
/// edit route addresses an entry by all three, and the two timeout
/// spellings stay RAW — the edit form writes back in the unit the entry
/// already uses, so editing never adds a second timeout field.
pub fn flatten_hooks(events: &Map<String, Value>) -> Vec<HookRow> {
    let mut rows = Vec::new();
    for (event, groups) in events {
        let groups = groups.as_array().cloned().unwrap_or_default();
        for (matcher_index, group) in groups.iter().enumerate() {
            let matcher = group
                .get("matcher")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            for (hook_index, entry) in group
                .get("hooks")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .iter()
                .enumerate()
            {
                // Entries decode permissively field-by-field: an unknown
                // shape is an all-default row, never a failure.
                let get_str = |k: &str| entry.get(k).and_then(Value::as_str).map(str::to_string);
                let get_num = |k: &str| {
                    entry.get(k).and_then(|v| {
                        v.as_u64().or_else(|| v.as_f64().map(|f| f as u64))
                    })
                };
                rows.push(HookRow {
                    key: format!("{event}/{matcher_index}/{hook_index}"),
                    event: event.clone(),
                    matcher_index,
                    hook_index,
                    matcher: matcher.clone(),
                    hook_type: get_str("type"),
                    command: get_str("command").unwrap_or_default(),
                    enabled: entry.get("enabled").and_then(Value::as_bool) != Some(false),
                    raw_timeout_ms: get_num("timeoutMs"),
                    raw_timeout_sec: get_num("timeout"),
                });
            }
        }
    }
    rows
}

/// The entry's timeout in milliseconds, whichever spelling it carries.
pub fn timeout_ms_of(row: &HookRow) -> Option<u64> {
    row.raw_timeout_ms.or(row.raw_timeout_sec.map(|s| s * 1000))
}

/// Whether the hooks tree is live. The gate is surfaced twice — the
/// route's own top-level field and the copy nested in the config — and the
/// route's field is the authority, the nested one the fallback for an
/// older build. Absent means OFF, not on: `setHooksEnabled(false)` DELETES
/// the key, and the runtime treats a missing `enabled` as disabled.
pub fn hooks_enabled(payload: Option<&Value>) -> bool {
    match payload.and_then(|v| v.get("enabled")).and_then(Value::as_bool) {
        Some(top) => top,
        None => payload
            .and_then(|v| v.get("hooks"))
            .and_then(|h| h.get("enabled"))
            .map_or(false, |v| v == &Value::Bool(true)),
    }
}

// ─── skills / agents (SkillsPage.tsx / AgentsPage.tsx) ─────────────────────

/// Whether a skill already lives in one of the user's own trees
/// (`user` | `agents`); plugin and project scopes do not.
pub fn is_user_tree_scope(scope: Option<&str>) -> bool {
    matches!(scope, Some("user") | Some("agents"))
}

/// Whether an agent's controls must be hidden, on the server's verdict
/// (`readOnly === true` — a built-in has no file to edit or delete, and
/// the API refuses both with a 400 the user cannot act on).
pub fn is_agent_read_only(read_only: Option<bool>) -> bool {
    read_only == Some(true)
}

// ─── plan quota (QuotaPage.tsx) ─────────────────────────────────────────────

/// Below this used-percentage a reset-card spend needs an extra warning
/// step; at or above it the single confirmation is the spend itself.
pub const SPEND_THRESHOLD_PCT: f64 = 90.0;

/// The provider ids that can own a coding plan (server coding-plan.ts).
pub const CODING_PLAN_PROVIDERS: [&str; 4] = [
    "account:bigmodel-individual-coding-plan",
    "account:bigmodel-team-coding-plan",
    "account:zai-individual-coding-plan",
    "account:zai-team-coding-plan",
];

/// A team plan needs organization/project headers a headless process
/// cannot read, so its cards can never be spent from here.
pub fn is_team_plan(provider_id: &str) -> bool {
    provider_id.contains("-team-")
}

/// The used percentage of the ONE window a card type clears. A card's
/// confirmation count reads its OWN window — an exhausted week must not
/// let a 5-hour card skip its warning, because that card cannot clear the
/// week. Only the GLM branch counts (the payload also carries Opencode Go
/// and Ollama windows, which a coding-plan card does nothing for), and a
/// window the payload does not list reads as 0 — unknown usage keeps the
/// warning rather than skipping it.
pub fn window_used_percent(usage: Option<&Value>, card: ResetCardType) -> f64 {
    let Some(glm) = usage.and_then(|u| u.get("glm")) else {
        return 0.0;
    };
    let Some(items) = glm.get("items").and_then(Value::as_array) else {
        return 0.0;
    };
    items
        .iter()
        .find(|it| it.get("key").and_then(Value::as_str) == Some(card.window_key()))
        .and_then(|it| it.get("usedPercent"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    //! Ports of `tests/config-response-shapes.test.ts` (DECODE coverage) and
    //! `tests/config-reset-rules.test.ts` (spend threshold + provider
    //! eligibility). The TS rule — every decode is imported from the screen,
    //! never copied — becomes: this module IS the decode the future screens
    //! must call; these tests pin it against the shapes the bridge emits.

    use serde::Deserialize;
    use serde_json::{json, Value};

    use super::*;

    fn models(payload: Value) -> ModelsPayload {
        serde_json::from_value(payload).unwrap()
    }

    // --- mcp: wrapped in `{ok, mcp:{servers}}`, like models and usage -------

    fn mcp_wire() -> Value {
        json!({
            "ok": true,
            "mcp": {
                "servers": [
                    {
                        "name": "ctx7",
                        "type": "stdio",
                        "command": "npx",
                        "args": ["-y", "@upstash/context7-mcp"],
                        "enabled": true
                    },
                    {
                        "name": "off",
                        "type": "http",
                        "url": "https://example.com/mcp",
                        "enabled": false
                    }
                ]
            }
        })
    }

    #[test]
    fn mcp_reads_servers_from_inside_mcp_not_the_top_level() {
        let wire = mcp_wire();
        // The bug this guards: `payload.servers` is undefined, `.length
        // === 0`, and the page renders "no MCP servers configured" on a
        // machine that has them.
        assert!(wire.get("servers").is_none());
        assert_eq!(mcp_servers(Some(&wire)).len(), 2);
    }

    #[test]
    fn mcp_keeps_the_command_plus_args_for_the_rows_subtitle() {
        let first = &mcp_servers(Some(&mcp_wire()))[0];
        let parts: Vec<String> = std::iter::once(first.command.clone().unwrap())
            .chain(first.args.clone().unwrap_or_default())
            .collect();
        assert_eq!(parts.join(" "), "npx -y @upstash/context7-mcp");
    }

    #[test]
    fn mcp_treats_an_absent_enabled_as_on_matching_the_config_file_default() {
        let wire = json!({
            "ok": true,
            "mcp": { "servers": [ { "name": "a" }, { "name": "b", "enabled": false } ] }
        });
        let rows = mcp_servers(Some(&wire));
        // TS `rows[0].enabled !== false` — absent is on.
        assert_ne!(rows[0].enabled, Some(false));
        assert_eq!(rows[1].enabled, Some(false));
    }

    #[test]
    fn mcp_returns_an_empty_list_rather_than_failing_on_a_missing_section() {
        assert!(mcp_servers(Some(&json!({ "ok": true }))).is_empty());
        assert!(mcp_servers(None).is_empty());
    }

    // --- the mcp upsert body: only the transport's fields, blank kv omitted -

    fn stdio_fields<'a>() -> McpFormFields<'a> {
        McpFormFields {
            server_type: "stdio",
            enabled: true,
            remote: false,
            url: "",
            headers_text: "",
            command: "npx",
            args_text: "-y\nctx7",
            env_text: "A=1\nB=x=y",
        }
    }

    #[test]
    fn mcp_upsert_sends_only_the_transports_own_fields() {
        // The route MERGES the body into the entry, so a field from the
        // wrong transport branch lands next to the real one instead of
        // replacing it.
        let body = mcp_upsert_body(&stdio_fields());
        assert_eq!(
            serde_json::to_value(&body).unwrap(),
            json!({
                "type": "stdio",
                "enabled": true,
                "command": "npx",
                "args": ["-y", "ctx7"],
                "env": { "A": "1", "B": "x=y" }
            })
        );
        let remote = McpFormFields {
            server_type: "sse",
            enabled: false,
            remote: true,
            url: "https://x/mcp",
            headers_text: "Authorization=Bearer t",
            command: "leftover",
            args_text: "leftover",
            env_text: "",
        };
        assert_eq!(
            serde_json::to_value(&mcp_upsert_body(&remote)).unwrap(),
            json!({
                "type": "sse",
                "enabled": false,
                "url": "https://x/mcp",
                "headers": { "Authorization": "Bearer t" }
            })
        );
    }

    #[test]
    fn mcp_upsert_omits_a_blank_env_headers_instead_of_clearing_the_entry() {
        // Desktop parity: blank means "keep". A `{}` would wipe the entry's
        // env — or shadow a legacy `http_headers` block the runtime merges
        // with `??`.
        let stdio_blank_env = McpFormFields {
            args_text: "",
            env_text: "  \n",
            ..stdio_fields()
        };
        let body = serde_json::to_value(&mcp_upsert_body(&stdio_blank_env)).unwrap();
        let Value::Object(map) = &body else { panic!("object body") };
        assert!(!map.contains_key("env"));

        let http_blank = McpFormFields {
            server_type: "http",
            remote: true,
            url: "https://x",
            headers_text: "",
            command: "",
            args_text: "",
            env_text: "",
            ..stdio_fields()
        };
        let body = serde_json::to_value(&mcp_upsert_body(&http_blank)).unwrap();
        let Value::Object(map) = &body else { panic!("object body") };
        assert!(!map.contains_key("headers"));
    }

    #[test]
    fn mcp_upsert_still_sends_args_as_an_empty_array_the_desktops_spelling() {
        let blank_args = McpFormFields {
            args_text: "  \n",
            env_text: "",
            ..stdio_fields()
        };
        assert_eq!(
            serde_json::to_value(&mcp_upsert_body(&blank_args)).unwrap()["args"],
            json!([])
        );
    }

    // --- models: an OBJECT with `available` + `modelRules`, not an array ----

    fn models_payload() -> Value {
        json!({
            "ok": true,
            "models": {
                "available": [
                    { "providerId": "account:bigmodel-individual-coding-plan", "modelId": "glm-4.6" },
                    { "providerId": "builtin:bigmodel-coding-plan", "modelId": "glm-4.5-air" }
                ],
                "providers": [
                    { "providerId": "builtin:bigmodel-coding-plan", "providerName": "Bigmodel" }
                ],
                "modelRules": [
                    {
                        "providerId": "account:bigmodel-individual-coding-plan",
                        "modelId": "glm-4.6",
                        "contextWindow": 200000
                    }
                ]
            }
        })
    }

    #[test]
    fn models_reads_the_selectable_list_out_of_the_object_not_the_object_itself() {
        let payload = models_payload();
        // The bug this guards: treating `models` as an array gives `length`
        // undefined, which passes a `=== 0` check and then throws on map.
        assert!(payload["models"]["available"].is_array());
        assert_eq!(payload["models"]["available"].as_array().unwrap().len(), 2);
        assert!(!payload["models"].is_array());
    }

    #[test]
    fn models_joins_the_context_window_from_model_rules_by_provider_and_model() {
        let payload = models_payload();
        let block = models(payload.clone());
        let available = block.models.as_ref().unwrap().available.as_ref().unwrap();
        assert_eq!(context_window_for(&available[0], block.models.as_ref().unwrap().model_rules.as_deref()), Some(200000));
    }

    #[test]
    fn models_leaves_the_window_absent_when_no_rule_carries_properties() {
        let payload = models_payload();
        let block = models(payload);
        let available = block.models.as_ref().unwrap().available.as_ref().unwrap();
        assert_eq!(
            context_window_for(&available[1], block.models.as_ref().unwrap().model_rules.as_deref()),
            None
        );
    }

    #[test]
    fn models_joins_a_rule_across_the_two_coding_plan_spellings() {
        // `available` normalizes coding-plan ids to `builtin:*` server-side
        // while `modelRules` ride through verbatim (the desktop writes
        // `account:*` there) — an exact-string join misses the rule, its
        // context window and its reasoning levels.
        let rule = json!({
            "providerId": "account:bigmodel-individual-coding-plan",
            "modelId": "glm-4.5-air",
            "contextWindow": 128000,
            "reasoningLevels": ["low", "high"]
        });
        let builtin_model = ModelRef {
            provider_id: Some("builtin:bigmodel-coding-plan".into()),
            provider_name: None,
            model_id: Some("glm-4.5-air".into()),
        };
        let rules: Vec<ModelRule> = vec![serde_json::from_value(rule.clone()).unwrap()];
        assert_eq!(context_window_for(&builtin_model, Some(&rules)), Some(128000));

        let mut payload = models_payload();
        payload["models"]["modelRules"]
            .as_array_mut()
            .unwrap()
            .push(rule);
        let options = model_options(Some(&models(payload)));
        let air = options
            .iter()
            .find(|o| o.model_id == "glm-4.5-air")
            .unwrap();
        assert_eq!(air.reasoning_levels, vec!["low".to_string(), "high".to_string()]);
    }

    #[test]
    fn models_builds_picker_options_with_each_models_reasoning_levels() {
        // The agent forms pick a model from this list; a rule that declares
        // levels must reach the picker, or the level dropdown is empty on a
        // model that supports reasoning.
        let mut payload = models_payload();
        payload["models"]["modelRules"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "providerId": "builtin:bigmodel-coding-plan",
                "modelId": "glm-4.5-air",
                "reasoningLevels": ["high", "low"]
            }));
        let options = model_options(Some(&models(payload)));
        assert_eq!(options.len(), 2);
        assert!(options[0].reasoning_levels.is_empty());
        assert_eq!(options[1].reasoning_levels, vec!["high".to_string(), "low".to_string()]);
        // Duplicate entries (the union across providers) collapse on the key.
        let keys: HashSet<String> = options
            .iter()
            .map(|o| format!("{}/{}", o.provider_id, o.model_id))
            .collect();
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn models_keeps_a_picker_option_for_a_provider_the_rules_block_omits() {
        // The agent picker names models from `available`; an account plan
        // provider that `providers` omits must still reach the model list.
        let options = model_options(Some(&models(models_payload())));
        let ids: Vec<&str> = options.iter().map(|o| o.provider_id.as_str()).collect();
        assert!(ids.contains(&"account:bigmodel-individual-coding-plan"));
        assert!(ids.contains(&"builtin:bigmodel-coding-plan"));
    }

    #[test]
    fn models_leaves_account_providers_out_of_the_add_model_picker() {
        // The write route refuses `account:*` providers outright, so
        // offering one only produces an unactionable 400.
        let options = provider_options(Some(&models(models_payload())));
        let ids: Vec<&str> = options.iter().map(|p| p.id.as_str()).collect();
        assert!(ids.contains(&"builtin:bigmodel-coding-plan"));
        assert!(!ids.contains(&"account:bigmodel-individual-coding-plan"));
    }

    #[test]
    fn models_labels_models_the_way_the_session_switcher_does() {
        // One rule everywhere: a builtin/coding-plan model reads as its bare
        // id, a third-party one is qualified, and an id two providers share
        // always qualifies.
        let mut payload = models_payload();
        payload["models"]["available"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "providerId": "uuid-1", "providerName": "Go", "modelId": "deepseek-v4" }));
        payload["models"]["available"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "providerId": "uuid-2", "providerName": "Other", "modelId": "glm-4.6" }));
        payload["models"]["modelRules"] = json!([]);
        let options = model_options(Some(&models(payload)));
        let label = |provider_id: &str, model_id: &str| -> String {
            options
                .iter()
                .find(|o| o.provider_id == provider_id && o.model_id == model_id)
                .unwrap()
                .label
                .clone()
        };
        // Collision on glm-4.6: both carriers qualify, a nameless one with
        // its id.
        assert_eq!(label("uuid-2", "glm-4.6"), "Other › glm-4.6");
        assert_eq!(
            label("account:bigmodel-individual-coding-plan", "glm-4.6"),
            "account:bigmodel-individual-coding-plan › glm-4.6"
        );
        assert_eq!(label("builtin:bigmodel-coding-plan", "glm-4.5-air"), "glm-4.5-air"); // builtin: bare
        assert_eq!(label("uuid-1", "deepseek-v4"), "Go › deepseek-v4"); // third-party
    }

    // --- provider spelling: `account:` vs `builtin:` ------------------------

    #[test]
    fn provider_maps_coding_plan_spellings_onto_one_match_key() {
        assert_eq!(
            match_provider_id(Some("account:bigmodel-individual-coding-plan")),
            "builtin:bigmodel-coding-plan"
        );
        assert_eq!(
            match_provider_id(Some("account:acme-team-coding-plan")),
            "builtin:acme-coding-plan"
        );
        assert_eq!(
            match_provider_id(Some("account:acme-start-coding-plan")),
            "builtin:acme-coding-plan"
        );
        // Anything else passes through untouched, whatever the prefix.
        assert_eq!(
            match_provider_id(Some("builtin:bigmodel-coding-plan")),
            "builtin:bigmodel-coding-plan"
        );
        assert_eq!(match_provider_id(Some("account:odd-shape")), "account:odd-shape");
        assert_eq!(match_provider_id(None), "");
    }

    #[test]
    fn provider_maps_an_agent_files_model_onto_the_payloads_option() {
        // The real-world pair: the file spells the plan `account:…`, the
        // payload spells it `builtin:…`. The cross-spelling match is what
        // lets the picker prefill.
        let plan_payload = json!({
            "ok": true,
            "models": {
                "available": [
                    { "providerId": "builtin:bigmodel-coding-plan", "providerName": "BigModel", "modelId": "GLM-5.3" }
                ],
                "providers": [],
                "modelRules": []
            }
        });
        let options = model_options(Some(&models(plan_payload)));
        assert_eq!(options[0].label, "GLM-5.3");
        let stored = decode_subagent_model(Some("custom:account%3Abigmodel-individual-coding-plan:GLM-5.3"))
            .expect("decodes");
        assert_eq!(stored.provider_id, "account:bigmodel-individual-coding-plan");
        assert_eq!(stored.model_id, "GLM-5.3");
        let found = find_model_option(&options, &stored.provider_id, &stored.model_id);
        assert_eq!(found.map(|o| o.model_id.as_str()), Some("GLM-5.3"));
    }

    // --- the frontmatter model spelling --------------------------------------

    #[test]
    fn subagent_encoding_keeps_a_plain_id_readable_and_encodes_a_colliding_one() {
        assert_eq!(
            encode_subagent_model("builtin:bigmodel", "glm-4.6"),
            "builtin:bigmodel/glm-4.6"
        );
        assert_eq!(
            encode_subagent_model("custom:account:bigmodel-individual-coding-plan", "GLM-5.3"),
            "custom:custom%3Aaccount%3Abigmodel-individual-coding-plan:GLM-5.3"
        );
    }

    #[test]
    fn subagent_decoding_decodes_the_desktops_custom_spelling_back_to_ids() {
        assert_eq!(
            decode_subagent_model(Some("custom:account%3Abigmodel-individual-coding-plan:GLM-5.3")),
            Some(ModelSelection {
                provider_id: "account:bigmodel-individual-coding-plan".into(),
                model_id: "GLM-5.3".into()
            })
        );
        assert_eq!(
            decode_subagent_model(Some("builtin:bigmodel/glm-4.6")),
            Some(ModelSelection {
                provider_id: "builtin:bigmodel".into(),
                model_id: "glm-4.6".into()
            })
        );
    }

    #[test]
    fn subagent_decoding_reads_the_legacy_custom_builtin_spelling() {
        // The provider id itself contains an UNENCODED colon, so the
        // separator is not the first one — treating it as such yields
        // providerId "builtin" and a modelId matching no picker option.
        assert_eq!(
            decode_subagent_model(Some("custom:builtin:bigmodel-coding-plan:GLM-5.3")),
            Some(ModelSelection {
                provider_id: "builtin:bigmodel-coding-plan".into(),
                model_id: "GLM-5.3".into()
            })
        );
    }

    #[test]
    fn subagent_encoding_round_trips_through_both_spellings() {
        let encoded = encode_subagent_model("custom:a:b", "GLM-5.3");
        assert_eq!(
            decode_subagent_model(Some(&encoded)),
            Some(ModelSelection {
                provider_id: "custom:a:b".into(),
                model_id: "GLM-5.3".into()
            })
        );
    }

    #[test]
    fn subagent_decoding_returns_none_for_a_value_it_cannot_map() {
        // The form then shows it read-only instead of substituting a model.
        assert_eq!(decode_subagent_model(Some("")), None);
        assert_eq!(decode_subagent_model(None), None);
        assert_eq!(decode_subagent_model(Some("inherit")), None);
        assert_eq!(decode_subagent_model(Some("noseparator")), None);
        assert_eq!(decode_subagent_model(Some("custom:onlyprovider")), None);
    }

    // --- usage: wrapped in `{ok, usage}` --------------------------------------

    #[derive(Deserialize)]
    struct UsageEnvelope {
        usage: Option<crate::settings::SettingsUsage>,
    }

    #[test]
    fn usage_unwraps_the_section_the_raw_response_has_no_available_at_the_top() {
        // The bug this guards: storing the whole response made
        // `usage.available` undefined, so the "no data" empty state
        // rendered forever.
        let wire = json!({
            "ok": true,
            "usage": {
                "available": true,
                "range": "7d",
                "summary": {
                    "totalTokens": 12345, "inputTokens": 0, "outputTokens": 0,
                    "reasoningTokens": 0, "cacheReadTokens": 0, "requestCount": 7,
                    "sessionCount": 0, "turnCount": 0, "toolCallCount": 0,
                    "models": 2, "activeDays": 1
                },
                "models": [ {
                    "modelId": "glm-4.6", "totalTokens": 9000, "inputTokens": 0,
                    "outputTokens": 0, "reasoningTokens": 0, "cacheReadTokens": 0,
                    "cacheCreationTokens": 0, "requestCount": 7, "share": 0.72
                } ],
                "daily": [ { "date": "2026-09-22", "models": [ { "modelId": "glm-4.6", "totalTokens": 9000 } ] } ]
            }
        });
        assert!(wire.get("available").is_none());
        let env: UsageEnvelope = serde_json::from_value(wire).unwrap();
        let usage = env.usage.expect("usage section");
        assert!(usage.available);
        assert_eq!(usage.summary.total_tokens, 12345);
        assert_eq!(usage.models.len(), 1);
        assert_eq!(usage.daily.len(), 1);
    }

    #[test]
    fn usage_treats_a_machine_with_no_agent_database_as_unavailable_not_broken() {
        // The server degrades to available:false zeroes rather than null.
        let wire = json!({
            "ok": true,
            "usage": {
                "available": false,
                "range": "7d",
                "summary": {
                    "totalTokens": 0, "inputTokens": 0, "outputTokens": 0,
                    "reasoningTokens": 0, "cacheReadTokens": 0, "requestCount": 0,
                    "sessionCount": 0, "turnCount": 0, "toolCallCount": 0,
                    "models": 0, "activeDays": 0
                },
                "models": [],
                "daily": []
            }
        });
        let env: UsageEnvelope = serde_json::from_value(wire).unwrap();
        assert!(!env.usage.expect("usage section").available);
    }

    // --- hooks: `{ok, hooks:{enabled, events}}` -------------------------------

    fn hooks_wire() -> Value {
        json!({
            "ok": true,
            "hooks": {
                "enabled": true,
                "events": {
                    "PostToolUse": [
                        { "matcher": "Bash", "hooks": [ { "command": "echo one" }, { "command": "echo two" } ] }
                    ],
                    "SessionStart": [
                        { "hooks": [ { "command": "boot", "enabled": false, "timeout": 5 } ] }
                    ]
                }
            },
            "enabled": true
        })
    }

    #[test]
    fn hooks_reads_events_from_inside_hooks_not_the_top_level() {
        // The bug this guards: `payload.events` is undefined, so the screen
        // showed its empty state on a machine that had hooks configured.
        let wire = hooks_wire();
        assert!(wire.get("events").is_none());
        assert!(wire["hooks"]["events"].is_object());
    }

    #[test]
    fn hooks_flattens_the_two_level_matcher_hook_nesting_into_rows() {
        let rows = flatten_hooks(hooks_wire()["hooks"]["events"].as_object().unwrap());
        assert_eq!(rows.len(), 3);
        let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, vec!["PostToolUse/0/0", "PostToolUse/0/1", "SessionStart/0/0"]);
    }

    #[test]
    fn hooks_keeps_the_matcher_as_a_label_and_defaults_an_absent_one_to_any() {
        let rows = flatten_hooks(hooks_wire()["hooks"]["events"].as_object().unwrap());
        assert_eq!(rows[0].matcher, "Bash");
        assert_eq!(rows[2].matcher, "");
    }

    #[test]
    fn hooks_reads_an_absent_enabled_as_on_and_keeps_the_seconds_timeout_raw() {
        let rows = flatten_hooks(hooks_wire()["hooks"]["events"].as_object().unwrap());
        assert!(rows[0].enabled);
        assert!(!rows[2].enabled);
        assert_eq!(rows[2].raw_timeout_sec, Some(5));
    }

    #[test]
    fn hooks_keeps_each_timeout_spelling_raw_and_either_reads_back_in_ms() {
        // The edit form writes back in the unit the entry already spells,
        // so the decode must not collapse the two into one field.
        let events = json!({
            "Mixed": [
                { "hooks": [
                    { "command": "a", "timeoutMs": 1500 },
                    { "command": "b", "timeout": 90 },
                    { "command": "c" }
                ] }
            ]
        });
        let rows = flatten_hooks(events.as_object().unwrap());
        assert_eq!(rows[0].raw_timeout_ms, Some(1500));
        assert_eq!(rows[0].raw_timeout_sec, None);
        assert_eq!(timeout_ms_of(&rows[0]), Some(1500));
        assert_eq!(rows[1].raw_timeout_sec, Some(90));
        assert_eq!(timeout_ms_of(&rows[1]), Some(90_000));
        assert_eq!(timeout_ms_of(&rows[2]), None);
        // The edit route addresses an entry by all three coordinates.
        assert_eq!(rows[1].matcher_index, 0);
        assert_eq!(rows[1].hook_index, 1);
    }

    #[test]
    fn hooks_reads_an_absent_nested_enabled_as_off_because_false_deletes_the_key() {
        // setHooksEnabled(false) removes `enabled` rather than storing
        // false, and the runtime treats the missing key as disabled. A
        // `!== false` read shows the tree as live while every hook is
        // inert.
        assert!(!hooks_enabled(Some(&json!({ "hooks": { "events": {} } }))));
    }

    #[test]
    fn hooks_reads_the_gate_on_only_when_the_route_says_so_explicitly() {
        assert!(hooks_enabled(Some(&json!({ "enabled": true, "hooks": { "enabled": true, "events": {} } }))));
        assert!(!hooks_enabled(Some(&json!({ "enabled": false, "hooks": { "enabled": true, "events": {} } }))));
    }

    #[test]
    fn hooks_falls_back_to_the_nested_copy_when_the_route_omits_its_field() {
        assert!(hooks_enabled(Some(&json!({ "hooks": { "enabled": true, "events": {} } }))));
        assert!(!hooks_enabled(Some(&json!({ "hooks": { "events": {} } }))));
        assert!(!hooks_enabled(None));
    }

    #[test]
    fn hooks_prefers_the_routes_top_level_gate_over_the_nested_copy() {
        // Both present and either being false means hooks are inert; the
        // route's own field is the authoritative one.
        assert!(!hooks_enabled(Some(&json!({ "enabled": false, "hooks": { "enabled": true } }))));
        assert!(hooks_enabled(Some(&json!({ "enabled": true, "hooks": { "enabled": false } }))));
    }

    // --- skills: `scope` decides what may be done ------------------------------

    #[test]
    fn skills_counts_only_the_users_own_trees_as_already_user_scope() {
        let wire = json!([
            { "name": "mine", "path": "/h/.zcode/skills/mine/SKILL.md", "scope": "user", "enabled": true },
            { "name": "plug", "path": "/h/.zcode/cli/plugins/cache/p/SKILL.md", "scope": "plugin", "enabled": true },
            { "name": "ws", "path": "/w/.zcode/skills/ws/SKILL.md", "scope": "project", "enabled": false }
        ]);
        let scope = |i: usize| wire[i]["scope"].as_str();
        assert_eq!(
            (0..3).map(scope).map(is_user_tree_scope).collect::<Vec<_>>(),
            vec![true, false, false]
        );
        // Copy-to-user is offered for exactly the scopes outside the user's
        // trees.
        assert_eq!(
            (0..3)
                .map(scope)
                .map(|s| !is_user_tree_scope(s))
                .collect::<Vec<_>>(),
            vec![false, true, true]
        );
        assert!(!is_user_tree_scope(None));
    }

    // --- agents: `readOnly` is the server's verdict ------------------------------

    #[test]
    fn agents_hides_the_controls_for_a_read_only_agent() {
        // The bug this guards: reading a `builtIn` field the bridge never
        // sends left the enable and delete controls on built-ins, where
        // every call is refused with a 400 the user cannot act on.
        let wire = json!([
            {
                "name": "general-purpose",
                "frontmatter": { "name": "general-purpose", "description": "General agent" },
                "systemPrompt": "…",
                "enabled": true,
                "readOnly": true
            },
            {
                "name": "mine",
                "frontmatter": { "name": "mine", "description": "My agent" },
                "systemPrompt": "…",
                "enabled": true,
                "readOnly": false,
                "modelSelection": { "modelId": "glm-4.6", "thoughtLevel": "high" }
            }
        ]);
        let read_only = |i: usize| wire[i]["readOnly"].as_bool();
        assert!(is_agent_read_only(read_only(0)));
        assert!(wire[0].get("builtIn").is_none());
        assert!(!is_agent_read_only(read_only(1)));
        assert!(!is_agent_read_only(None));
    }

    #[test]
    fn agents_reads_the_description_from_frontmatter_and_the_override_from_model_selection() {
        let wire = json!([
            { "name": "general-purpose", "frontmatter": { "description": "General agent" } },
            { "name": "mine", "modelSelection": { "modelId": "glm-4.6", "thoughtLevel": "high" } }
        ]);
        assert_eq!(wire[0]["frontmatter"]["description"], "General agent");
        assert_eq!(wire[1]["modelSelection"]["modelId"], "glm-4.6");
        assert_eq!(wire[1]["modelSelection"]["thoughtLevel"], "high");
    }

    // --- reset cards: the provider list is all four ids, not the account's -------

    #[test]
    fn reset_card_eligibility_never_assumes_a_single_provider() {
        // The bug this guards: a `providers.length === 1` check could never
        // be true, so the whole reset-card feature was unreachable.
        let wire = json!({
            "ok": true,
            "resetCards": {
                "providers": [
                    "account:bigmodel-individual-coding-plan",
                    "account:bigmodel-team-coding-plan",
                    "account:zai-individual-coding-plan",
                    "account:zai-team-coding-plan"
                ],
                "credentials": true
            }
        });
        let providers = wire["resetCards"]["providers"].as_array().unwrap();
        assert_eq!(providers.len(), 4);
        assert_ne!(providers.len(), 1);
    }

    #[test]
    fn reset_card_eligibility_separates_no_credentials_from_no_plan() {
        // credentials:false means the store could not be decrypted — a
        // normal state, and a different message from having nothing to
        // spend.
        let wire = json!({ "resetCards": { "providers": [], "credentials": true } });
        assert_eq!(wire["resetCards"]["credentials"], json!(true));
        let no_creds = json!({ "resetCards": { "providers": [], "credentials": false } });
        assert_eq!(no_creds["resetCards"]["credentials"], json!(false));
    }

    #[test]
    fn reset_cards_still_refuse_to_spend_on_a_team_plan() {
        assert!(is_team_plan("account:bigmodel-team-coding-plan"));
        assert!(is_team_plan("account:zai-team-coding-plan"));
        assert!(!is_team_plan("account:bigmodel-individual-coding-plan"));
        assert!(!is_team_plan("account:zai-individual-coding-plan"));
        // Not a plan at all.
        assert!(!is_team_plan("builtin:bigmodel-coding-plan"));
    }

    #[test]
    fn reset_cards_know_the_four_coding_plan_ids() {
        assert_eq!(
            CODING_PLAN_PROVIDERS.to_vec(),
            vec![
                "account:bigmodel-individual-coding-plan",
                "account:bigmodel-team-coding-plan",
                "account:zai-individual-coding-plan",
                "account:zai-team-coding-plan"
            ]
        );
    }

    // --- the confirm threshold (config-reset-rules.test.ts) ---------------------

    #[test]
    fn threshold_reads_each_cards_own_window_never_the_max_across_them() {
        // A card cannot clear a window it does not reset: an exhausted week
        // must not let a 5-hour card skip its warning.
        let usage = json!({
            "glm": {
                "items": [
                    { "key": "token_5h", "usedPercent": 20 },
                    { "key": "token_week", "usedPercent": 100 }
                ]
            }
        });
        assert_eq!(window_used_percent(Some(&usage), ResetCardType::FiveHour), 20.0);
        assert!(window_used_percent(Some(&usage), ResetCardType::FiveHour) < SPEND_THRESHOLD_PCT);
        assert_eq!(window_used_percent(Some(&usage), ResetCardType::Week), 100.0);
        assert!(window_used_percent(Some(&usage), ResetCardType::Week) >= SPEND_THRESHOLD_PCT);
    }

    #[test]
    fn threshold_skips_the_extra_confirmation_once_the_cards_window_reaches_it() {
        let at = json!({ "glm": { "items": [ { "key": "token_5h", "usedPercent": SPEND_THRESHOLD_PCT } ] } });
        assert!(window_used_percent(Some(&at), ResetCardType::FiveHour) >= SPEND_THRESHOLD_PCT);
        // The boundary-1 case keeps the warning.
        let just_below = json!({ "glm": { "items": [ { "key": "token_week", "usedPercent": SPEND_THRESHOLD_PCT - 1.0 } ] } });
        assert!(window_used_percent(Some(&just_below), ResetCardType::Week) < SPEND_THRESHOLD_PCT);
    }

    #[test]
    fn threshold_reads_a_window_the_payload_does_not_list_as_zero_not_unlimited() {
        // Unknown usage keeps the warning rather than skipping it.
        assert_eq!(window_used_percent(None, ResetCardType::Week), 0.0);
        assert_eq!(
            window_used_percent(Some(&json!({ "glm": {} })), ResetCardType::FiveHour),
            0.0
        );
        // Only token windows count — the MCP limit is not a card window.
        assert_eq!(
            window_used_percent(
                Some(&json!({ "glm": { "items": [ { "key": "mcp", "usedPercent": 100 } ] } })),
                ResetCardType::Week
            ),
            0.0
        );
    }

    #[test]
    fn threshold_ignores_the_opencode_go_and_ollama_windows() {
        // Those are not coding-plan windows; a card does nothing for them,
        // so they must not be able to skip the warning.
        let usage = json!({
            "glm": { "items": [ { "key": "token_5h", "usedPercent": 20 } ] },
            "opencode": { "kind": "success", "windows": [ { "usagePercent": 100 } ] }
        });
        assert_eq!(window_used_percent(Some(&usage), ResetCardType::FiveHour), 20.0);
    }
}
