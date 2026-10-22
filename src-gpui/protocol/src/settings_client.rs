//! Settings API client (bridge 0.47.0+, server ADR-0025) — the Rust port of
//! `src/lib/hub.ts`'s settings section: every ZCode-configuration read/write
//! route, the dynamic-workflow management routes that ride the per-instance
//! settings mount, the machine-level system-stats read, and the backend
//! restart that applies a `needs-restart` write.
//!
//! Purity contract (`desktop-client-requirements.md` §5): this module is
//! transport-INJECTABLE, not transport-free of any. All HTTP concerns are
//! behind [`HttpTransport`]; everything the tests can see — method, URL,
//! headers, serialized body, resolved timeout, and the status/error
//! classification — is shaped here, in plain data. The app crate supplies a
//! real transport; these unit tests (the port of
//! `tests/hub-settings-client.test.ts`) script one.
//!
//! Error doctrine (ported from `HubApiError`): a network failure is an
//! expected "offline" state, 401 is "check token", and a refused write's
//! BODY is the message — the settings API answers a refusal with a reason
//! ("nonce is stale — refresh the reset card status"), and dropping it
//! leaves the user with "HTTP 409" and no way forward. The workflow routes
//! carry both a short reason token (`error`) and the human detail
//! (`message`, e.g. compile diagnostics) — the detail wins.

use std::future::Future;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::hub::{ConnectionProfile, HubUpgradeResult};
use crate::settings::{
    AgentUpsert, BackendRestartResult, ConnectivityResult, HookEntryPatch, McpServerUpsert,
    ModelUpsert, PendingRestartStatus, ResetCardStatus, ResetOpportunityRequest,
    ResetOpportunityResponse, SettingsAll, SpendResetCard, WorkflowGateModeSetting,
    WorkflowGateSetResponse, WriteEffect,
};
use crate::system_stats::SystemStatsResponse;
use crate::workflow::{
    AmendRunSettingsBody, AmendRunSettingsResponse, ConversationRunsResponse, DismissRunResponse,
    DismissRunsResponse, RunArtifactsResponse, WorkflowArtifactItemsResponse,
    WorkflowArtifactReadResponse, WorkflowCreatePromptResponse, WorkflowDetailResponse,
    WorkflowListResponse, WorkflowNodeResultResponse, WorkflowOverviewResponse,
    WorkflowRunEventsResponse, WorkflowRunsHistoryResponse, WorkflowScope, WorkflowStartResponse,
    WorkflowWorkspaceResponse,
};

/// Default response deadline (hub.ts: `timeoutMs = 10_000`). A phone waking
/// from deep sleep can park a fetch on a socket that never delivers a byte;
/// without a deadline the reconnect loop's discovery await hangs forever.
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;

/// HTTP verb the settings API uses (the configuration API writes with PUT
/// and DELETE as well as POST).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
        }
    }
}

impl std::fmt::Display for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One outbound request exactly as the client shaped it. The transport
/// enforces `timeout_ms` (None = no deadline); everything else is final.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    /// Insertion-ordered: Authorization first, Content-Type when a body rides.
    pub headers: Vec<(String, String)>,
    /// Serialized JSON body; None sends no body at all.
    pub body: Option<String>,
    /// Resolved deadline in milliseconds; None = wait indefinitely.
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// The injectable HTTP half. One call per request; the implementation owns
/// connections, redirects, and the deadline.
pub trait HttpTransport: Send + Sync {
    type Error: std::fmt::Display + Send + Sync + 'static;

    fn fetch(
        &self,
        request: HttpRequest,
    ) -> impl Future<Output = Result<HttpResponse, Self::Error>> + Send;
}

/// Port of `HubApiError`: the classification callers branch on.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HubApiError {
    /// The fetch itself failed (hub unreachable / deadline passed). The hub
    /// lives and dies with the editor, so this is an expected "offline"
    /// state, not an error.
    #[error("network error: {0}")]
    Network(String),
    #[error("unauthorized: check token")]
    Unauthorized,
    /// A refused request: `message` is the body's `message`/`error` detail
    /// (preferred) or the bare `HTTP {status}` fallback.
    #[error("{message}")]
    Http { status: u16, message: String },
    /// A 2xx body that did not decode into the expected shape.
    #[error("{0}")]
    Unexpected(String),
}

impl HubApiError {
    /// The HTTP status when the hub answered with one (TS `HubApiError.status`).
    pub fn status(&self) -> Option<u16> {
        match self {
            HubApiError::Http { status, .. } => Some(*status),
            _ => None,
        }
    }
}

/// JS `encodeURIComponent`: percent-encodes every byte except the unreserved
/// set `A-Za-z0-9 - _ . ! ~ * ' ( )`. Path segments and query values built
/// through template literals in hub.ts use exactly this spelling.
pub fn encode_uri_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => out.push(b as char),
            b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `URLSearchParams` value serialization (`application/x-www-form-urlencoded`
/// percent-encode set): same unreserved set minus the JS extras, and a space
/// becomes `+` rather than `%20`. hub.ts builds its `sessionId=…&…` query
/// strings through URLSearchParams, so `sess 1` rides as `sess+1`.
pub fn encode_query_value(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Build a query string in insertion order (`k1=v1&k2=v2`), the order
/// URLSearchParams serializes in; empty input renders as no `?` at all.
fn query_string(pairs: &[(&str, String)]) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    let joined = pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    format!("?{joined}")
}

/// Per-call response deadline, port of hub.ts's `timeoutMs` option:
/// [`Deadline::Default`] (the 10s), an override, or infinite for the
/// slow-by-design routes (instance incubation, the app-update download).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Deadline {
    #[default]
    Default,
    Ms(u64),
    Infinite,
}

impl Deadline {
    fn resolve(self) -> Option<u64> {
        match self {
            Deadline::Default => Some(DEFAULT_TIMEOUT_MS),
            Deadline::Ms(ms) => Some(ms),
            Deadline::Infinite => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FetchOptions {
    pub deadline: Deadline,
}

/// The Settings API client over an injected transport. Stateless: one
/// instance may be shared for the connection's lifetime.
#[derive(Debug, Clone)]
pub struct HubHttpClient<T> {
    hub_url: String,
    token: String,
    transport: T,
}

impl<T: HttpTransport> HubHttpClient<T> {
    pub fn new(hub_url: impl Into<String>, token: impl Into<String>, transport: T) -> Self {
        Self {
            hub_url: hub_url.into(),
            token: token.into(),
            transport,
        }
    }

    pub fn from_profile(profile: &ConnectionProfile, transport: T) -> Self {
        Self::new(profile.hub_url.clone(), profile.token.clone(), transport)
    }

    /// hub.ts `url()`: hub URL with trailing slashes stripped + path (the
    /// path carries its own leading slash — no separator is added).
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.hub_url.trim_end_matches('/'), path)
    }

    /// The shared fetch pipeline: headers, body serialization, deadline
    /// resolution, and the error classification every route shares.
    async fn fetch_raw(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        opts: FetchOptions,
    ) -> Result<HttpResponse, HubApiError> {
        let mut headers = vec![("Authorization".to_string(), format!("Bearer {}", self.token))];
        let body = match body {
            Some(value) => {
                headers.push((
                    "Content-Type".to_string(),
                    "application/json".to_string(),
                ));
                Some(
                    serde_json::to_string(&value)
                        .map_err(|e| HubApiError::Unexpected(format!("serialize body: {e}")))?,
                )
            }
            None => None,
        };
        // Undefined deadline = the 10s default; an override or Infinite is
        // the caller's explicit choice (hub.ts `timeoutMs: null` = infinite).
        let request = HttpRequest {
            method,
            url: self.url(path),
            headers,
            body,
            timeout_ms: opts.deadline.resolve(),
        };
        let response = self
            .transport
            .fetch(request)
            .await
            .map_err(|e| HubApiError::Network(e.to_string()))?;
        if response.status == 401 {
            return Err(HubApiError::Unauthorized);
        }
        if !(200..=299).contains(&response.status) {
            return Err(HubApiError::Http {
                status: response.status,
                message: refusal_detail(&response),
            });
        }
        Ok(response)
    }

    /// `fetch_raw` + JSON decode of the 2xx body.
    async fn fetch_json<R: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        opts: FetchOptions,
    ) -> Result<R, HubApiError> {
        let response = self.fetch_raw(method, path, body, opts).await?;
        serde_json::from_str(&response.body)
            .map_err(|e| HubApiError::Unexpected(format!("invalid payload from {path}: {e}")))
    }

    // ---- machine-level reads (the hub's settings mount) ----

    /// One-shot snapshot of every section (first screen of a config page).
    pub async fn settings_all(&self) -> Result<SettingsAll, HubApiError> {
        self.fetch_json(Method::Get, "/api/settings/all", None, FetchOptions::default())
            .await
    }

    pub async fn settings_models(&self) -> Result<Value, HubApiError> {
        self.fetch_json(Method::Get, "/api/settings/models", None, FetchOptions::default())
            .await
    }

    pub async fn settings_skills(&self) -> Result<Value, HubApiError> {
        self.fetch_json(Method::Get, "/api/settings/skills", None, FetchOptions::default())
            .await
    }

    pub async fn settings_mcp(&self) -> Result<Value, HubApiError> {
        self.fetch_json(Method::Get, "/api/settings/mcp", None, FetchOptions::default())
            .await
    }

    pub async fn settings_hooks(&self) -> Result<Value, HubApiError> {
        self.fetch_json(Method::Get, "/api/settings/hooks", None, FetchOptions::default())
            .await
    }

    pub async fn settings_agents(&self) -> Result<Value, HubApiError> {
        self.fetch_json(Method::Get, "/api/settings/agents", None, FetchOptions::default())
            .await
    }

    /// `range` is `7d | 30d | all`.
    pub async fn settings_usage(&self, range: &str) -> Result<Value, HubApiError> {
        let path = format!(
            "/api/settings/usage?range={}",
            encode_uri_component(range)
        );
        self.fetch_json(Method::Get, &path, None, FetchOptions::default())
            .await
    }

    /// Platform usage (account-level monitor data). `range` is
    /// `today | 7d | 30d`. A 404 means the bridge predates the route.
    pub async fn settings_usage_platform(&self, range: &str) -> Result<Value, HubApiError> {
        let path = format!(
            "/api/settings/usage-platform?range={}",
            encode_uri_component(range)
        );
        self.fetch_json(Method::Get, &path, None, FetchOptions::default())
            .await
    }

    /// Account quota (bridge 0.8.0, ADR-0005): the `account/usage_stats`
    /// payload without an instance connection. Reads the hub's `/api/quota`
    /// — the route the TS app's `client.quota()` uses — whose body is the
    /// RAW combined payload. The settings mount's `/settings/quota` wraps
    /// the same data in a `{ok, quota}` envelope; reading that route and
    /// parsing it bare silently yields "no data" on every successful fetch,
    /// so this client sticks to the hub route.
    pub async fn quota(&self) -> Result<Value, HubApiError> {
        self.fetch_json(Method::Get, "/api/quota", None, FetchOptions::default())
            .await
    }

    /// Reset-card inventory for one coding-plan provider. The `nonce` in the
    /// answer is what [`Self::spend_reset_card`] must send back — a spend is
    /// tied to a status read made moments earlier, so a stale screen cannot
    /// burn a card.
    pub async fn reset_card_status(&self, provider_id: &str) -> Result<ResetCardStatus, HubApiError> {
        let path = format!(
            "/api/settings/reset-cards?providerId={}",
            encode_uri_component(provider_id)
        );
        self.fetch_json(Method::Get, &path, None, FetchOptions::default())
            .await
    }

    pub async fn spend_reset_card(
        &self,
        input: SpendResetCard,
    ) -> Result<Value, HubApiError> {
        let body = serde_json::to_value(&input)
            .map_err(|e| HubApiError::Unexpected(format!("serialize spend body: {e}")))?;
        self.fetch_json(
            Method::Post,
            "/api/settings/reset-cards/use",
            Some(body),
            FetchOptions::default(),
        )
        .await
    }

    pub async fn request_reset_opportunity(
        &self,
        input: ResetOpportunityRequest,
    ) -> Result<ResetOpportunityResponse, HubApiError> {
        let body = serde_json::to_value(&input)
            .map_err(|e| HubApiError::Unexpected(format!("serialize opportunity body: {e}")))?;
        self.fetch_json(
            Method::Post,
            "/api/settings/reset-cards/opportunity",
            Some(body),
            FetchOptions::default(),
        )
        .await
    }

    pub async fn mark_reset_history_read(&self, provider_id: &str) -> Result<(), HubApiError> {
        self.fetch_json(
            Method::Post,
            "/api/settings/reset-cards/history-read",
            Some(serde_json::json!({ "providerId": provider_id })),
            FetchOptions::default(),
        )
        .await
    }

    pub async fn settings_backups(&self) -> Result<Value, HubApiError> {
        self.fetch_json(Method::Get, "/api/settings/backups", None, FetchOptions::default())
            .await
    }

    /// Whether one bridge has recorded a needs-restart write since its last
    /// restart. Per-instance on purpose: the hub-local spelling answers 409,
    /// because the hub has no backend whose restart could clear its own
    /// answer.
    pub async fn settings_pending_restart(
        &self,
        instance_id: &str,
    ) -> Result<PendingRestartStatus, HubApiError> {
        let path = format!(
            "/api/instances/{}/settings/pending-restart",
            encode_uri_component(instance_id)
        );
        self.fetch_json(Method::Get, &path, None, FetchOptions::default())
            .await
    }

    pub async fn settings_app_update(&self, channel: Option<&str>) -> Result<Value, HubApiError> {
        let path = match channel {
            Some(channel) => format!(
                "/api/settings/app-update?channel={}",
                encode_uri_component(channel)
            ),
            None => "/api/settings/app-update".to_string(),
        };
        self.fetch_json(Method::Get, &path, None, FetchOptions::default())
            .await
    }

    /// The hub's own staleness check (bridge 0.11.7, `POST /api/upgrade`):
    /// the hub alone decides whether the on-disk code is newer and restarts
    /// onto it — `restarting` means it exits ~500ms after answering and
    /// re-spawns; callers ride that out with their normal health/discovery
    /// polling (TS `upgradeHub`).
    pub async fn upgrade_hub(&self) -> Result<HubUpgradeResult, HubApiError> {
        self.fetch_json(Method::Post, "/api/upgrade", None, FetchOptions::default())
            .await
    }

    /// The machine-level workflow page payload (bridge 0.60.0): one group per
    /// workspace, deduped hub-side. NOT instance-addressed — the whole point
    /// of the route is that the hub aggregates every registered bridge.
    pub async fn workflow_overview(&self) -> Result<WorkflowOverviewResponse, HubApiError> {
        self.fetch_json(Method::Get, "/api/workflow-overview", None, FetchOptions::default())
            .await
    }

    /// Machine-level system status (`/api/system-stats`, bridge 0.62.0):
    /// host info, CPU/memory/storage, battery, power assertions, network,
    /// and the hub/bridge process resources. Rate fields need two reads ~1s
    /// apart (the hub deltas across reads — first read is null).
    pub async fn system_stats(&self) -> Result<SystemStatsResponse, HubApiError> {
        self.fetch_json(Method::Get, "/api/system-stats", None, FetchOptions::default())
            .await
    }

    // ---- configuration writes (each answers its Effect Class) ----

    pub async fn update_provider(
        &self,
        provider_id: &str,
        body: Value,
    ) -> Result<WriteEffect, HubApiError> {
        let path = format!(
            "/api/settings/providers/{}",
            encode_uri_component(provider_id)
        );
        self.fetch_json(Method::Put, &path, Some(body), FetchOptions::default())
            .await
    }

    /// POST /settings/models is an upsert: one route adds a model to a
    /// provider and edits its personal rule.
    pub async fn add_model(&self, body: ModelUpsert) -> Result<WriteEffect, HubApiError> {
        let value = serde_json::to_value(&body)
            .map_err(|e| HubApiError::Unexpected(format!("serialize model body: {e}")))?;
        self.fetch_json(Method::Post, "/api/settings/models", Some(value), FetchOptions::default())
            .await
    }

    /// POST /settings/models/test — one minimal request against the provider.
    ///
    /// The answer is always `Ok` for a provider-side failure: a refused key or
    /// an unreachable host is the RESULT of the probe, not an error of it, and
    /// the UI paints it next to the row.
    pub async fn test_model_connectivity(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Result<ConnectivityResult, HubApiError> {
        let body = serde_json::json!({
            "providerId": provider_id,
            "modelId": model_id,
        });
        let value: Value = self
            .fetch_json(Method::Post, "/api/settings/models/test", Some(body), FetchOptions::default())
            .await?;
        serde_json::from_value(value.get("result").cloned().unwrap_or(Value::Null))
            .map_err(|e| HubApiError::Unexpected(format!("decode connectivity result: {e}")))
    }

    pub async fn remove_model(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Result<WriteEffect, HubApiError> {        let path = format!(
            "/api/settings/models/{}/{}",
            encode_uri_component(provider_id),
            encode_uri_component(model_id)
        );
        self.fetch_json(Method::Delete, &path, None, FetchOptions::default())
            .await
    }

    pub async fn set_skill_enabled(&self, path: &str, enable: bool) -> Result<WriteEffect, HubApiError> {
        self.fetch_json(
            Method::Post,
            "/api/settings/skills/enable",
            Some(serde_json::json!({ "path": path, "enable": enable })),
            FetchOptions::default(),
        )
        .await
    }

    pub async fn copy_skill_to_user(&self, path: &str) -> Result<WriteEffect, HubApiError> {
        self.fetch_json(
            Method::Post,
            "/api/settings/skills/copy-to-user",
            Some(serde_json::json!({ "path": path })),
            FetchOptions::default(),
        )
        .await
    }

    pub async fn delete_skill(&self, skill_path: &str) -> Result<WriteEffect, HubApiError> {
        let path = format!(
            "/api/settings/skills/{}",
            encode_uri_component(skill_path)
        );
        self.fetch_json(Method::Delete, &path, None, FetchOptions::default())
            .await
    }

    /// PUT /settings/mcp/{name} MERGES into the existing entry, so only the
    /// fields the form actually edits travel.
    pub async fn upsert_mcp_server(
        &self,
        name: &str,
        body: McpServerUpsert,
    ) -> Result<WriteEffect, HubApiError> {
        let path = format!("/api/settings/mcp/{}", encode_uri_component(name));
        let value = serde_json::to_value(&body)
            .map_err(|e| HubApiError::Unexpected(format!("serialize mcp body: {e}")))?;
        self.fetch_json(Method::Put, &path, Some(value), FetchOptions::default())
            .await
    }

    pub async fn remove_mcp_server(&self, name: &str) -> Result<WriteEffect, HubApiError> {
        let path = format!("/api/settings/mcp/{}", encode_uri_component(name));
        self.fetch_json(Method::Delete, &path, None, FetchOptions::default())
            .await
    }

    /// The route reads `enabled` (not `enable`, unlike skills and agents).
    pub async fn set_mcp_enabled(&self, name: &str, enable: bool) -> Result<WriteEffect, HubApiError> {
        self.fetch_json(
            Method::Post,
            "/api/settings/mcp/enable",
            Some(serde_json::json!({ "name": name, "enabled": enable })),
            FetchOptions::default(),
        )
        .await
    }

    /// Edit one existing hook entry. The entry is addressed by THREE
    /// coordinates: event + matcher index in the path, hook index in the
    /// body (the route only takes two path segments). There is no way to
    /// add or remove an entry through here.
    pub async fn update_hook(
        &self,
        event_name: &str,
        matcher_index: usize,
        hook_index: usize,
        body: HookEntryPatch,
    ) -> Result<WriteEffect, HubApiError> {
        let path = format!(
            "/api/settings/hooks/{}/{}",
            encode_uri_component(event_name),
            matcher_index
        );
        let mut value = serde_json::to_value(&body)
            .map_err(|e| HubApiError::Unexpected(format!("serialize hook patch: {e}")))?;
        value["hookIndex"] = serde_json::json!(hook_index);
        self.fetch_json(Method::Put, &path, Some(value), FetchOptions::default())
            .await
    }

    pub async fn set_hooks_enabled(&self, enabled: bool) -> Result<WriteEffect, HubApiError> {
        self.fetch_json(
            Method::Post,
            "/api/settings/hooks/enabled",
            Some(serde_json::json!({ "enabled": enabled })),
            FetchOptions::default(),
        )
        .await
    }

    /// A PUT on a missing personal agent creates it; personal-agent patches
    /// carry JSON nulls to clear frontmatter keys, and a built-in override
    /// clear sends providerId+modelId null TOGETHER.
    pub async fn upsert_agent(&self, name: &str, body: AgentUpsert) -> Result<WriteEffect, HubApiError> {
        let path = format!("/api/settings/agents/{}", encode_uri_component(name));
        let value = serde_json::to_value(&body)
            .map_err(|e| HubApiError::Unexpected(format!("serialize agent body: {e}")))?;
        self.fetch_json(Method::Put, &path, Some(value), FetchOptions::default())
            .await
    }

    pub async fn delete_agent(&self, name: &str) -> Result<WriteEffect, HubApiError> {
        let path = format!("/api/settings/agents/{}", encode_uri_component(name));
        self.fetch_json(Method::Delete, &path, None, FetchOptions::default())
            .await
    }

    pub async fn set_agent_enabled(&self, name: &str, enable: bool) -> Result<WriteEffect, HubApiError> {
        let path = format!(
            "/api/settings/agents/{}/enable",
            encode_uri_component(name)
        );
        self.fetch_json(
            Method::Post,
            &path,
            Some(serde_json::json!({ "enable": enable })),
            FetchOptions::default(),
        )
        .await
    }

    /// Restore one config backup. Size-dependent file copy — minutes would
    /// be wrong, but the 10s default is too.
    pub async fn restore_backup(&self, file: &str, path: &str) -> Result<WriteEffect, HubApiError> {
        self.fetch_json(
            Method::Post,
            "/api/settings/backups/restore",
            Some(serde_json::json!({ "file": file, "path": path })),
            FetchOptions {
                deadline: Deadline::Ms(60_000),
            },
        )
        .await
    }

    /// The hub machine downloads the artifact before answering — however
    /// long its own network takes. The channel must travel with the
    /// install: the route re-reads the manifest for the channel it is TOLD
    /// and refuses any version that is not the latest THERE.
    pub async fn install_app_update(
        &self,
        version: &str,
        url: &str,
        channel: Option<&str>,
    ) -> Result<Value, HubApiError> {
        let mut body = serde_json::json!({ "version": version, "url": url });
        if let Some(channel) = channel {
            body["channel"] = serde_json::json!(channel);
        }
        self.fetch_json(
            Method::Post,
            "/api/settings/app-update/install",
            Some(body),
            FetchOptions {
                deadline: Deadline::Infinite,
            },
        )
        .await
    }

    /// Restart one bridge's backend so its `needs-restart` writes take
    /// effect. Per-instance on purpose: the hub's own settings mount has no
    /// backend, so the restart must name the bridge it means to disturb.
    /// `cancelled_turns` in the answer is how many conversations were
    /// interrupted — warn with it BEFORE the user commits. `closed` is
    /// whether the old subprocess actually went away: false means the
    /// writes this restart existed to apply did not take effect, so the
    /// caller must keep the affordance alive.
    pub async fn restart_backend(
        &self,
        instance_id: &str,
    ) -> Result<BackendRestartResult, HubApiError> {
        // Stop + respawn of the sandboxed backend: seconds-scale.
        let path = format!(
            "/api/instances/{}/backend/restart",
            encode_uri_component(instance_id)
        );
        self.fetch_json(
            Method::Post,
            &path,
            None,
            FetchOptions {
                deadline: Deadline::Ms(30_000),
            },
        )
        .await
    }

    // ---- dynamic-workflow management (per-instance settings proxy) ----

    /// Base path for one bridge's per-instance settings routes. Every
    /// workflow route is PER-INSTANCE: the hub's machine-level settings
    /// mount has no backend (its `/settings/all` reports the workflow gate
    /// as a bare `{available:false}`), so each call names the bridge.
    fn inst_settings(&self, instance_id: &str) -> String {
        format!(
            "/api/instances/{}/settings",
            encode_uri_component(instance_id)
        )
    }

    /// The per-instance `/settings/all` — the only mount whose `workflow`
    /// block carries the real gate verdict.
    pub async fn instance_settings_all(&self, instance_id: &str) -> Result<SettingsAll, HubApiError> {
        let path = format!("{}/all", self.inst_settings(instance_id));
        self.fetch_json(Method::Get, &path, None, FetchOptions::default())
            .await
    }

    /// The machine-level gate override switch (bridge ≥0.53): persists
    /// `workflow.mode` in the bridge's user config and answers the
    /// effective verdict the next gate read sees.
    pub async fn workflow_gate_set(
        &self,
        instance_id: &str,
        mode: WorkflowGateModeSetting,
    ) -> Result<WorkflowGateSetResponse, HubApiError> {
        let path = format!("{}/workflow-gate", self.inst_settings(instance_id));
        let body = serde_json::json!({ "mode": mode });
        self.fetch_json(Method::Put, &path, Some(body), FetchOptions::default())
            .await
    }

    pub async fn workflows_list(
        &self,
        instance_id: &str,
        scope: Option<WorkflowScope>,
    ) -> Result<WorkflowListResponse, HubApiError> {
        let path = match scope {
            // Scope values are the closed "project"|"global" pair — hub.ts
            // interpolates them raw.
            Some(scope) => format!(
                "{}/workflows?scope={}",
                self.inst_settings(instance_id),
                scope_name(scope)
            ),
            None => format!("{}/workflows", self.inst_settings(instance_id)),
        };
        self.fetch_json(Method::Get, &path, None, FetchOptions::default())
            .await
    }

    pub async fn workflow_get(
        &self,
        instance_id: &str,
        scope: WorkflowScope,
        name: &str,
    ) -> Result<WorkflowDetailResponse, HubApiError> {
        let path = format!(
            "{}/workflows/{}/{}",
            self.inst_settings(instance_id),
            encode_uri_component(scope_name(scope)),
            encode_uri_component(name)
        );
        self.fetch_json(Method::Get, &path, None, FetchOptions::default())
            .await
    }

    pub async fn workflow_update_meta(
        &self,
        instance_id: &str,
        scope: WorkflowScope,
        name: &str,
        meta: Value,
    ) -> Result<Value, HubApiError> {
        let path = format!(
            "{}/workflows/{}/{}/meta",
            self.inst_settings(instance_id),
            encode_uri_component(scope_name(scope)),
            encode_uri_component(name)
        );
        self.fetch_json(Method::Put, &path, Some(meta), FetchOptions::default())
            .await
    }

    pub async fn workflow_delete(
        &self,
        instance_id: &str,
        scope: WorkflowScope,
        name: &str,
    ) -> Result<Value, HubApiError> {
        let path = format!(
            "{}/workflows/{}/{}",
            self.inst_settings(instance_id),
            encode_uri_component(scope_name(scope)),
            encode_uri_component(name)
        );
        self.fetch_json(Method::Delete, &path, None, FetchOptions::default())
            .await
    }

    pub async fn workflow_move(
        &self,
        instance_id: &str,
        scope: WorkflowScope,
        name: &str,
    ) -> Result<Value, HubApiError> {
        let path = format!(
            "{}/workflows/{}/{}/move",
            self.inst_settings(instance_id),
            encode_uri_component(scope_name(scope)),
            encode_uri_component(name)
        );
        self.fetch_json(Method::Post, &path, None, FetchOptions::default())
            .await
    }

    pub async fn workflow_start(
        &self,
        instance_id: &str,
        scope: WorkflowScope,
        name: &str,
        args: Option<Value>,
        session_id: Option<&str>,
    ) -> Result<WorkflowStartResponse, HubApiError> {
        let mut body = serde_json::Map::new();
        if let Some(args) = args {
            body.insert("args".to_string(), args);
        }
        if let Some(session_id) = session_id {
            body.insert("sessionId".to_string(), serde_json::json!(session_id));
        }
        let path = format!(
            "{}/workflows/{}/{}/start",
            self.inst_settings(instance_id),
            encode_uri_component(scope_name(scope)),
            encode_uri_component(name)
        );
        let body = if body.is_empty() { None } else { Some(Value::Object(body)) };
        self.fetch_json(Method::Post, &path, body, FetchOptions::default())
            .await
    }

    pub async fn workflow_resume(
        &self,
        instance_id: &str,
        run_id: &str,
        session_id: &str,
        name: Option<&str>,
    ) -> Result<Value, HubApiError> {
        let mut body = serde_json::json!({ "sessionId": session_id });
        if let Some(name) = name {
            body["name"] = serde_json::json!(name);
        }
        let path = format!(
            "{}/workflow-runs/{}/resume",
            self.inst_settings(instance_id),
            encode_uri_component(run_id)
        );
        self.fetch_json(Method::Post, &path, Some(body), FetchOptions::default())
            .await
    }

    /// Stop a flying run (bridge 0.56.0): `session/cancelBackgroundTask`
    /// via the hub, so a run on ANY instance is stoppable. 409 = not
    /// running (already settled).
    pub async fn workflow_stop(
        &self,
        instance_id: &str,
        run_id: &str,
        session_id: &str,
    ) -> Result<Value, HubApiError> {
        let path = format!(
            "{}/workflow-runs/{}/stop",
            self.inst_settings(instance_id),
            encode_uri_component(run_id)
        );
        self.fetch_json(
            Method::Post,
            &path,
            Some(serde_json::json!({ "sessionId": session_id })),
            FetchOptions::default(),
        )
        .await
    }

    /// Amend a run's settings (bridge 0.49.0). Three-state per field: the
    /// key's PRESENCE is the discriminator — absent = keep, `null` =
    /// revert to default, value = set. `?sessionId=` is required.
    pub async fn amend_run_settings(
        &self,
        instance_id: &str,
        run_id: &str,
        session_id: &str,
        body: AmendRunSettingsBody,
    ) -> Result<AmendRunSettingsResponse, HubApiError> {
        let query = query_string(&[("sessionId", encode_query_value(session_id))]);
        let path = format!(
            "{}/workflow-runs/{}/settings{query}",
            self.inst_settings(instance_id),
            encode_uri_component(run_id)
        );
        let body = serde_json::to_value(&body)
            .map_err(|e| HubApiError::Unexpected(format!("serialize amend body: {e}")))?;
        self.fetch_json(Method::Post, &path, Some(body), FetchOptions::default())
            .await
    }

    /// Journal run history — cross-restart, not session-bound.
    pub async fn workflow_runs_history(
        &self,
        instance_id: &str,
        scope: Option<WorkflowScope>,
        name: Option<&str>,
        limit: Option<u64>,
    ) -> Result<WorkflowRunsHistoryResponse, HubApiError> {
        let mut pairs: Vec<(&str, String)> = Vec::new();
        if let Some(scope) = scope {
            pairs.push(("scope", scope_name(scope).to_string()));
        }
        if let Some(name) = name {
            pairs.push(("name", encode_query_value(name)));
        }
        if let Some(limit) = limit {
            pairs.push(("limit", limit.to_string()));
        }
        let path = format!(
            "{}/workflows/runs{}",
            self.inst_settings(instance_id),
            query_string(&pairs)
        );
        self.fetch_json(Method::Get, &path, None, FetchOptions::default())
            .await
    }

    /// Hide one settled journal run from the bridge's run lists (bridge
    /// 0.58.0). 409 while the run is still flying.
    pub async fn dismiss_workflow_run(
        &self,
        instance_id: &str,
        run_id: &str,
    ) -> Result<DismissRunResponse, HubApiError> {
        let path = format!(
            "{}/workflow-runs/{}/dismiss",
            self.inst_settings(instance_id),
            encode_uri_component(run_id)
        );
        self.fetch_json(Method::Post, &path, None, FetchOptions::default())
            .await
    }

    /// Batch variant: still-running ids are skipped, the settled ones hidden.
    pub async fn dismiss_workflow_runs(
        &self,
        instance_id: &str,
        run_ids: &[String],
    ) -> Result<DismissRunsResponse, HubApiError> {
        let path = format!(
            "{}/workflow-runs/dismiss-batch",
            self.inst_settings(instance_id)
        );
        self.fetch_json(
            Method::Post,
            &path,
            Some(serde_json::json!({ "runIds": run_ids })),
            FetchOptions::default(),
        )
        .await
    }

    /// The desktop's prefilled "create via conversation" prompt.
    pub async fn workflow_create_prompt(
        &self,
        instance_id: &str,
        scope: Option<WorkflowScope>,
    ) -> Result<WorkflowCreatePromptResponse, HubApiError> {
        let path = match scope {
            Some(scope) => format!(
                "{}/workflow-create-prompt?scope={}",
                self.inst_settings(instance_id),
                scope_name(scope)
            ),
            None => format!(
                "{}/workflow-create-prompt",
                self.inst_settings(instance_id)
            ),
        };
        self.fetch_json(Method::Get, &path, None, FetchOptions::default())
            .await
    }

    /// A session's runs, incl. the `resumable` verdict per run.
    pub async fn conversation_runs(
        &self,
        instance_id: &str,
        session_id: &str,
        limit: Option<u64>,
    ) -> Result<ConversationRunsResponse, HubApiError> {
        let mut pairs = vec![("sessionId", encode_query_value(session_id))];
        if let Some(limit) = limit {
            pairs.push(("limit", limit.to_string()));
        }
        let path = format!(
            "{}/workflow-runs{}",
            self.inst_settings(instance_id),
            query_string(&pairs)
        );
        self.fetch_json(Method::Get, &path, None, FetchOptions::default())
            .await
    }

    pub async fn run_events(
        &self,
        instance_id: &str,
        session_id: &str,
        run_id: &str,
        after_sequence: Option<u64>,
    ) -> Result<WorkflowRunEventsResponse, HubApiError> {
        let mut pairs = vec![("sessionId", encode_query_value(session_id))];
        if let Some(after) = after_sequence {
            pairs.push(("afterSequence", after.to_string()));
        }
        let path = format!(
            "{}/workflow-runs/{}/events{}",
            self.inst_settings(instance_id),
            encode_uri_component(run_id),
            query_string(&pairs)
        );
        self.fetch_json(Method::Get, &path, None, FetchOptions::default())
            .await
    }

    pub async fn run_artifacts(
        &self,
        instance_id: &str,
        session_id: &str,
        run_id: &str,
    ) -> Result<RunArtifactsResponse, HubApiError> {
        let query = query_string(&[("sessionId", encode_query_value(session_id))]);
        let path = format!(
            "{}/workflow-runs/{}/artifacts{query}",
            self.inst_settings(instance_id),
            encode_uri_component(run_id)
        );
        self.fetch_json(Method::Get, &path, None, FetchOptions::default())
            .await
    }

    pub async fn run_artifact_data(
        &self,
        instance_id: &str,
        session_id: &str,
        run_id: &str,
        artifact_id: &str,
        after_sequence: Option<u64>,
        limit: Option<u64>,
    ) -> Result<WorkflowArtifactItemsResponse, HubApiError> {
        let mut pairs = vec![("sessionId", encode_query_value(session_id))];
        if let Some(after) = after_sequence {
            pairs.push(("afterSequence", after.to_string()));
        }
        if let Some(limit) = limit {
            pairs.push(("limit", limit.to_string()));
        }
        let path = format!(
            "{}/workflow-runs/{}/artifacts/{}/data{}",
            self.inst_settings(instance_id),
            encode_uri_component(run_id),
            encode_uri_component(artifact_id),
            query_string(&pairs)
        );
        self.fetch_json(Method::Get, &path, None, FetchOptions::default())
            .await
    }

    pub async fn run_artifact_read(
        &self,
        instance_id: &str,
        session_id: &str,
        run_id: &str,
        artifact_id: &str,
        version: u64,
        offset: u64,
        limit: Option<u64>,
    ) -> Result<WorkflowArtifactReadResponse, HubApiError> {
        let mut pairs = vec![
            ("sessionId", encode_query_value(session_id)),
            ("version", version.to_string()),
            ("offset", offset.to_string()),
        ];
        if let Some(limit) = limit {
            pairs.push(("limit", limit.to_string()));
        }
        let path = format!(
            "{}/workflow-runs/{}/artifacts/{}/read{}",
            self.inst_settings(instance_id),
            encode_uri_component(run_id),
            encode_uri_component(artifact_id),
            query_string(&pairs)
        );
        self.fetch_json(Method::Get, &path, None, FetchOptions::default())
            .await
    }

    pub async fn run_workspace(
        &self,
        instance_id: &str,
        session_id: &str,
        run_id: &str,
    ) -> Result<WorkflowWorkspaceResponse, HubApiError> {
        let query = query_string(&[("sessionId", encode_query_value(session_id))]);
        let path = format!(
            "{}/workflow-runs/{}/workspace{query}",
            self.inst_settings(instance_id),
            encode_uri_component(run_id)
        );
        self.fetch_json(Method::Get, &path, None, FetchOptions::default())
            .await
    }

    pub async fn run_node_result(
        &self,
        instance_id: &str,
        session_id: &str,
        run_id: &str,
        site_id: &str,
        ordinal: u64,
    ) -> Result<WorkflowNodeResultResponse, HubApiError> {
        let query = query_string(&[("sessionId", encode_query_value(session_id))]);
        let path = format!(
            "{}/workflow-runs/{}/nodes/{}/{ordinal}{query}",
            self.inst_settings(instance_id),
            encode_uri_component(run_id),
            encode_uri_component(site_id),
        );
        self.fetch_json(Method::Get, &path, None, FetchOptions::default())
            .await
    }
}

/// The wire spelling of a workflow scope ("project" | "global").
fn scope_name(scope: WorkflowScope) -> &'static str {
    match scope {
        WorkflowScope::Project => "project",
        WorkflowScope::Global => "global",
    }
}

/// The message a refused request surfaces: the body's `message` (the
/// workflow routes put compile diagnostics there), else its `error`, else
/// the bare status. Anything unparseable is just the status.
fn refusal_detail(response: &HttpResponse) -> String {
    let fallback = format!("HTTP {}", response.status);
    let Ok(Value::Object(body)) = serde_json::from_str::<Value>(&response.body) else {
        return fallback;
    };
    for key in ["message", "error"] {
        if let Some(Value::String(detail)) = body.get(key) {
            if !detail.is_empty() {
                return detail.clone();
            }
        }
    }
    fallback
}

/// A shared transport is a shared client: the app holds one connection-level
/// transport in an `Arc`, so the blanket impl forwards.
impl<T: HttpTransport + ?Sized> HttpTransport for std::sync::Arc<T> {
    type Error = T::Error;

    fn fetch(
        &self,
        request: HttpRequest,
    ) -> impl Future<Output = Result<HttpResponse, Self::Error>> + Send {
        (**self).fetch(request)
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! The scripted transport both ported suites drive: a FIFO queue (the
    //! hub-settings-client tests, mirroring the TS file's `responses` array)
    //! plus URL-fragment routing with longest-match-wins and replace
    //! semantics (the config-store tests, mirroring `route()`).

    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;

    use super::{HttpTransport, HttpRequest, HttpResponse};

    #[derive(Debug, Clone)]
    pub(crate) struct ScriptedResponse {
        pub status: u16,
        pub body: String,
        pub delay_ms: u64,
    }

    /// `Response.json(body)` — the TS tests' default 200.
    pub(crate) fn ok_json(body: serde_json::Value) -> ScriptedResponse {
        ScriptedResponse {
            status: 200,
            body: serde_json::to_string(&body).unwrap(),
            delay_ms: 0,
        }
    }

    /// A refused request: status plus `{ok:false, error}` body.
    pub(crate) fn refused(status: u16, error: &str) -> ScriptedResponse {
        ScriptedResponse {
            status,
            body: serde_json::json!({ "ok": false, "error": error }).to_string(),
            delay_ms: 0,
        }
    }

    /// A 2xx body carrying only `ok:false` and a raw JSON body string.
    pub(crate) fn raw(status: u16, body: &str) -> ScriptedResponse {
        ScriptedResponse {
            status,
            body: body.to_string(),
            delay_ms: 0,
        }
    }

    pub(crate) fn delayed(mut res: ScriptedResponse, ms: u64) -> ScriptedResponse {
        res.delay_ms = ms;
        res
    }

    pub(crate) struct ScriptedTransport {
        queue: Mutex<VecDeque<ScriptedResponse>>,
        /// (url fragment, response) — re-routing a fragment replaces it.
        routes: Mutex<Vec<(String, ScriptedResponse)>>,
        requested: Mutex<Vec<HttpRequest>>,
    }

    impl ScriptedTransport {
        pub(crate) fn new() -> Self {
            Self {
                queue: Mutex::new(VecDeque::new()),
                routes: Mutex::new(Vec::new()),
                requested: Mutex::new(Vec::new()),
            }
        }

        /// Queue the next FIFO answer (TS `responses.push`).
        pub(crate) fn push(&self, res: ScriptedResponse) {
            self.queue.lock().unwrap().push_back(res);
        }

        /// Script the answer for any request whose URL contains `match`
        /// (TS `route`): re-routing the same fragment REPLACES the previous
        /// answer. Longest fragment wins, so `/settings/reset-cards/use`
        /// beats the generic `/settings/reset-cards`.
        pub(crate) fn route(&self, fragment: &str, res: ScriptedResponse) {
            let mut routes = self.routes.lock().unwrap();
            routes.retain(|(m, _)| m != fragment);
            routes.push((fragment.to_string(), res));
        }

        pub(crate) fn requested(&self) -> Vec<HttpRequest> {
            self.requested.lock().unwrap().clone()
        }

        pub(crate) fn urls(&self) -> Vec<String> {
            self.requested()
                .into_iter()
                .map(|r| r.url)
                .collect()
        }

        /// The parsed JSON body of the n-th recorded request.
        pub(crate) fn body(&self, index: usize) -> serde_json::Value {
            let body = self.requested()[index].body.clone().expect("request had a body");
            serde_json::from_str(&body).unwrap()
        }

        /// Requests whose URL contains `fragment` (TS
        /// `requested.filter((r) => r.url.includes(...))`).
        pub(crate) fn requests_containing(&self, fragment: &str) -> Vec<HttpRequest> {
            self.requested()
                .into_iter()
                .filter(|r| r.url.contains(fragment))
                .collect()
        }
    }

    impl HttpTransport for ScriptedTransport {
        type Error = String;

        async fn fetch(&self, request: HttpRequest) -> Result<HttpResponse, String> {
            self.requested.lock().unwrap().push(request.clone());
            let routed = {
                let routes = self.routes.lock().unwrap();
                routes
                    .iter()
                    .filter(|(m, _)| request.url.contains(m.as_str()))
                    .max_by_key(|(m, _)| m.len())
                    .map(|(_, r)| r.clone())
            };
            let next = match routed {
                Some(res) => Some(res),
                None => self.queue.lock().unwrap().pop_front(),
            };
            match next {
                Some(res) => {
                    if res.delay_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(res.delay_ms)).await;
                    }
                    Ok(HttpResponse {
                        status: res.status,
                        body: res.body,
                    })
                }
                None => Err(format!(
                    "test bug: no scripted response for {}",
                    request.url
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Port of `tests/hub-settings-client.test.ts` (REST-layer coverage for
    //! the ZCode configuration client). Port notes:
    //!
    //! - TS `Response.json({…})` + an `as` cast never validates; Rust decodes
    //!   strictly, so scripted bodies carry the fields the SERVER always
    //!   sends (e.g. `SettingsAll` needs every section; a workflow list needs
    //!   `workflows` + `invalid`). The TS test's minimal mocks are mock
    //!   artifacts, not wire claims.
    //! - `toMatchObject` on `{url, method, body}` becomes exact equality:
    //!   the client's requests are deterministic, and the full body IS what
    //!   the route reads.
    //! - Assertions the TS suite did not need but the port makes visible
    //!   (Bearer header, Content-Type with-body-only, per-route deadlines)
    //!   are added where hub.ts pins them.

    use std::sync::Arc;

    use serde_json::json;

    use super::test_support::{ok_json, raw, refused, ScriptedTransport};
    use super::*;
    use crate::settings::{
        AgentUpsert, HookEntryPatch, McpServerUpsert, ModelUpsert, ResetCardType, SpendResetCard,
    };
    use crate::workflow::AmendRunSettingsBody;

    fn transport() -> Arc<ScriptedTransport> {
        Arc::new(ScriptedTransport::new())
    }

    fn client(t: &Arc<ScriptedTransport>) -> HubHttpClient<Arc<ScriptedTransport>> {
        HubHttpClient::new("http://hub/", "tok", t.clone())
    }

    /// A complete `/settings/all` body (the server always sends every
    /// section — settings-endpoint.ts:631-672; usage degrades to zeroes).
    fn settings_all_body() -> Value {
        json!({
            "ok": true,
            "models": {},
            "skills": [],
            "mcp": {},
            "hooks": {},
            "agents": [],
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
            },
            "resetCards": { "providers": [], "credentials": true }
        })
    }

    #[tokio::test]
    async fn settings_all_reads_the_machine_level_mount_not_an_instance() {
        let t = transport();
        t.push(ok_json(settings_all_body()));
        client(&t).settings_all().await.unwrap();
        let req = &t.requested()[0];
        assert_eq!(req.url, "http://hub/api/settings/all");
        assert_eq!(req.method, Method::Get);
        // The fetch contract: bearer auth always, no content-type without a
        // body, and the 10s default deadline.
        assert_eq!(
            req.headers,
            vec![("Authorization".to_string(), "Bearer tok".to_string())]
        );
        assert_eq!(req.timeout_ms, Some(DEFAULT_TIMEOUT_MS));
    }

    #[tokio::test]
    async fn workflow_overview_is_a_hub_level_read() {
        let t = transport();
        t.push(ok_json(json!({ "ok": true, "groups": [], "activeRuns": [], "recentRuns": [] })));
        let res = client(&t).workflow_overview().await.unwrap();
        assert!(res.groups.is_empty());
        // NOT instance-addressed — the whole point of the route is that the
        // hub dedupes bridges and merges the machine-wide journal.
        assert_eq!(t.urls()[0], "http://hub/api/workflow-overview");
    }

    #[tokio::test]
    async fn section_reads_hit_their_own_routes_with_the_usage_range_encoded() {
        let t = transport();
        t.push(ok_json(json!({ "ok": true })));
        t.push(ok_json(json!({ "ok": true })));
        t.push(ok_json(json!({ "ok": true })));
        let c = client(&t);
        c.settings_models().await.unwrap();
        c.settings_skills().await.unwrap();
        c.settings_usage("30d").await.unwrap();
        assert_eq!(
            t.urls(),
            vec![
                "http://hub/api/settings/models",
                "http://hub/api/settings/skills",
                "http://hub/api/settings/usage?range=30d",
            ]
        );
    }

    #[tokio::test]
    async fn writes_use_the_verb_the_route_expects() {
        let t = transport();
        t.push(ok_json(json!({ "ok": true, "effect": "needs-restart" })));
        t.push(ok_json(json!({ "ok": true, "effect": "needs-restart" })));
        t.push(ok_json(json!({ "ok": true, "effect": "immediate" })));

        let c = client(&t);
        c.upsert_mcp_server(
            "ctx7",
            McpServerUpsert {
                command: Some("npx".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        c.remove_mcp_server("ctx7").await.unwrap();
        c.set_skill_enabled("/s/SKILL.md", false).await.unwrap();

        let reqs = t.requested();
        assert_eq!(reqs[0].url, "http://hub/api/settings/mcp/ctx7");
        assert_eq!(reqs[0].method, Method::Put);
        assert_eq!(reqs[1].url, "http://hub/api/settings/mcp/ctx7");
        assert_eq!(reqs[1].method, Method::Delete);
        assert_eq!(reqs[2].url, "http://hub/api/settings/skills/enable");
        assert_eq!(reqs[2].method, Method::Post);
        // A body-bearing write carries the JSON content-type.
        assert!(reqs[0].headers.contains(&(
            "Content-Type".to_string(),
            "application/json".to_string()
        )));
    }

    #[tokio::test]
    async fn a_skill_path_is_encoded_so_slashes_survive_the_route() {
        let t = transport();
        t.push(ok_json(json!({ "ok": true })));
        client(&t)
            .delete_skill("/home/u/.zcode/skills/x/SKILL.md")
            .await
            .unwrap();
        assert_eq!(
            t.urls()[0],
            "http://hub/api/settings/skills/%2Fhome%2Fu%2F.zcode%2Fskills%2Fx%2FSKILL.md"
        );
    }

    #[tokio::test]
    async fn a_model_upsert_posts_provider_and_model_with_only_the_set_rule_fields() {
        let t = transport();
        t.push(ok_json(json!({ "ok": true, "effect": "immediate" })));
        client(&t)
            .add_model(ModelUpsert {
                provider_id: "account:bigmodel-individual-coding-plan".into(),
                model_id: "glm-4.6".into(),
                enabled: Some(true),
                context_window: Some(200_000),
                reasoning_levels: Some(vec!["high".into(), "low".into()]),
            })
            .await
            .unwrap();
        // One route serves add AND edit (an upsert): the body names the model
        // and the rule fields that changed, nothing else.
        let req = &t.requested()[0];
        assert_eq!(req.url, "http://hub/api/settings/models");
        assert_eq!(req.method, Method::Post);
        assert_eq!(
            t.body(0),
            json!({
                "providerId": "account:bigmodel-individual-coding-plan",
                "modelId": "glm-4.6",
                "enabled": true,
                "contextWindow": 200000,
                "reasoningLevels": ["high", "low"],
            })
        );
    }

    #[tokio::test]
    async fn an_agent_upsert_puts_nulls_verbatim_clearing_rides_on_them() {
        let t = transport();
        t.push(ok_json(json!({ "ok": true, "effect": "needs-restart" })));
        t.push(ok_json(json!({ "ok": true, "effect": "needs-restart" })));
        let c = client(&t);
        // A personal-agent patch: `model: null` means "remove the key", so
        // the body must carry the null, not drop it.
        c.upsert_agent(
            "mine",
            AgentUpsert {
                description: Some("d".into()),
                model: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // A built-in override clear: BOTH keys null together, or the server
        // calls the request malformed.
        c.upsert_agent(
            "general-purpose",
            AgentUpsert {
                provider_id: Some(None),
                model_id: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let reqs = t.requested();
        assert_eq!(reqs[0].url, "http://hub/api/settings/agents/mine");
        assert_eq!(reqs[0].method, Method::Put);
        assert_eq!(t.body(0), json!({ "description": "d", "model": null }));
        assert_eq!(reqs[1].url, "http://hub/api/settings/agents/general-purpose");
        assert_eq!(t.body(1), json!({ "providerId": null, "modelId": null }));
    }

    #[tokio::test]
    async fn a_hook_edit_carries_all_three_coordinates_two_in_the_path_one_in_the_body() {
        let t = transport();
        t.push(ok_json(json!({ "ok": true, "effect": "needs-restart" })));
        client(&t)
            .update_hook(
                "PostToolUse",
                2,
                1,
                HookEntryPatch {
                    command: Some("echo hi".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        // The route only takes two path segments, so the hook's index inside
        // its matcher group travels in the body. Sending `{event}/{index}`
        // alone is a 400 — and it is exactly the shape a reader would guess.
        let req = &t.requested()[0];
        assert_eq!(req.url, "http://hub/api/settings/hooks/PostToolUse/2");
        assert_eq!(req.method, Method::Put);
        assert_eq!(t.body(0), json!({ "command": "echo hi", "hookIndex": 1 }));
    }

    #[tokio::test]
    async fn the_mcp_enable_route_wants_enabled_not_enable() {
        let t = transport();
        t.push(ok_json(json!({ "ok": true, "effect": "needs-restart" })));
        client(&t).set_mcp_enabled("ctx7", false).await.unwrap();
        let req = &t.requested()[0];
        assert_eq!(req.url, "http://hub/api/settings/mcp/enable");
        assert_eq!(req.method, Method::Post);
        assert_eq!(t.body(0), json!({ "name": "ctx7", "enabled": false }));
    }

    #[tokio::test]
    async fn a_reset_card_spend_carries_the_nonce_from_the_status_read() {
        let t = transport();
        t.push(ok_json(json!({
            "ok": true,
            "resetCards": {
                "availableFiveHour": [{ "expireAt": 1700000000000u64 }],
                "availableWeek": [],
                "latestFiveHour": null,
                "latestWeek": null,
                "hasUnreadHistory": false,
                "nonce": "n-1"
            }
        })));
        let status = client(&t)
            .reset_card_status("account:bigmodel-individual-coding-plan")
            .await
            .unwrap();
        assert_eq!(
            t.urls()[0],
            "http://hub/api/settings/reset-cards?providerId=account%3Abigmodel-individual-coding-plan"
        );

        t.push(ok_json(json!({ "ok": true, "used": true })));
        client(&t)
            .spend_reset_card(SpendResetCard {
                provider_id: "account:bigmodel-individual-coding-plan".into(),
                reset_type: ResetCardType::FiveHour,
                nonce: status.reset_cards.nonce.clone(),
                idempotency_key: "k-1".into(),
            })
            .await
            .unwrap();
        let req = &t.requested()[1];
        assert_eq!(req.url, "http://hub/api/settings/reset-cards/use");
        assert_eq!(req.method, Method::Post);
        assert_eq!(
            t.body(1),
            json!({
                "providerId": "account:bigmodel-individual-coding-plan",
                "resetType": "FIVE_HOUR",
                "nonce": "n-1",
                "idempotencyKey": "k-1",
            })
        );
    }

    #[tokio::test]
    async fn a_denied_opportunity_is_data_not_a_throw() {
        let t = transport();
        t.push(ok_json(json!({
            "ok": true,
            "opportunity": { "granted": false, "nextTryAt": 1700000000000u64 }
        })));
        let res = client(&t)
            .request_reset_opportunity(ResetOpportunityRequest {
                provider_id: "account:bigmodel-individual-coding-plan".into(),
                idempotency_key: "k-1".into(),
            })
            .await
            .unwrap();
        assert_eq!(res.opportunity.map(|o| o.granted), Some(false));
    }

    #[tokio::test]
    async fn the_backend_restart_names_an_instance_the_hub_mount_has_no_backend() {
        let t = transport();
        // hub.ts scripts `{cancelledTurns: 2}`; the server always adds
        // ok/closed — `closed` defaults to false when omitted.
        t.push(ok_json(json!({ "cancelledTurns": 2 })));
        let res = client(&t).restart_backend("inst-1").await.unwrap();
        assert_eq!(res.cancelled_turns, 2);
        let req = &t.requested()[0];
        assert_eq!(req.url, "http://hub/api/instances/inst-1/backend/restart");
        assert_eq!(req.method, Method::Post);
        // Stop + respawn of the sandboxed backend: hub.ts's 30s deadline.
        assert_eq!(req.timeout_ms, Some(30_000));
    }

    #[tokio::test]
    async fn a_backup_restore_sends_the_file_and_the_backup_path() {
        let t = transport();
        t.push(ok_json(json!({ "ok": true, "effect": "needs-restart" })));
        client(&t)
            .restore_backup("cliConfig", "/h/.zcode/cli/config.json.bak-20260922")
            .await
            .unwrap();
        let req = &t.requested()[0];
        assert_eq!(req.url, "http://hub/api/settings/backups/restore");
        assert_eq!(req.method, Method::Post);
        assert_eq!(
            t.body(0),
            json!({
                "file": "cliConfig",
                "path": "/h/.zcode/cli/config.json.bak-20260922",
            })
        );
        // Size-dependent file copy — the one write with a 60s deadline.
        assert_eq!(req.timeout_ms, Some(60_000));
    }

    #[tokio::test]
    async fn an_app_update_install_posts_the_version_and_artifact_url() {
        let t = transport();
        t.push(ok_json(json!({ "ok": true, "install": { "stage": "downloading" } })));
        client(&t)
            .install_app_update("3.14.2", "https://cdn/zcode.zip", Some("stable"))
            .await
            .unwrap();
        let req = &t.requested()[0];
        assert_eq!(req.url, "http://hub/api/settings/app-update/install");
        assert_eq!(req.method, Method::Post);
        assert_eq!(
            t.body(0),
            json!({
                "version": "3.14.2",
                "url": "https://cdn/zcode.zip",
                "channel": "stable",
            })
        );
        // The hub machine downloads the artifact before answering — no
        // client-side deadline (hub.ts `timeoutMs: null`).
        assert_eq!(req.timeout_ms, None);
    }

    #[tokio::test]
    async fn the_install_carries_the_channel_the_check_was_made_with() {
        let t = transport();
        t.push(ok_json(json!({ "ok": true })));
        client(&t)
            .install_app_update("3.14.3-preview.1", "https://cdn/zcode.zip", Some("preview"))
            .await
            .unwrap();
        assert_eq!(
            t.body(0),
            json!({ "version": "3.14.3-preview.1", "url": "https://cdn/zcode.zip", "channel": "preview" })
        );
    }

    #[tokio::test]
    async fn the_check_reads_the_requested_channel_not_just_stable() {
        let t = transport();
        t.push(ok_json(json!({ "ok": true })));
        client(&t).settings_app_update(Some("preview")).await.unwrap();
        assert_eq!(
            t.urls()[0],
            "http://hub/api/settings/app-update?channel=preview"
        );
    }

    // --- dynamic-workflow routes (bridge 0.48.0, server ADR-0029) ---------

    #[tokio::test]
    async fn workflow_management_routes_name_the_instance_and_use_their_verbs() {
        let t = transport();
        t.push(ok_json(json!({ "ok": true, "workflows": [], "invalid": [] })));
        t.push(ok_json(json!({ "ok": true, "name": "night ly", "scope": "global" })));
        t.push(ok_json(json!({ "ok": true })));
        t.push(ok_json(json!({ "ok": true })));
        t.push(ok_json(json!({ "ok": true })));
        t.push(ok_json(json!({ "ok": true, "acpSessionId": "s-1" })));

        let c = client(&t);
        c.workflows_list("i1", Some(WorkflowScope::Project))
            .await
            .unwrap();
        c.workflow_get("i1", WorkflowScope::Global, "night ly")
            .await
            .unwrap();
        c.workflow_update_meta("i1", WorkflowScope::Global, "night ly", json!({ "description": "x" }))
            .await
            .unwrap();
        c.workflow_delete("i1", WorkflowScope::Global, "night ly")
            .await
            .unwrap();
        c.workflow_move("i1", WorkflowScope::Global, "night ly")
            .await
            .unwrap();
        c.workflow_start("i1", WorkflowScope::Project, "deploy", Some(json!({ "target": "src/" })), None)
            .await
            .unwrap();

        let reqs = t.requested();
        let expected = [
            ("http://hub/api/instances/i1/settings/workflows?scope=project", Method::Get),
            ("http://hub/api/instances/i1/settings/workflows/global/night%20ly", Method::Get),
            ("http://hub/api/instances/i1/settings/workflows/global/night%20ly/meta", Method::Put),
            ("http://hub/api/instances/i1/settings/workflows/global/night%20ly", Method::Delete),
            ("http://hub/api/instances/i1/settings/workflows/global/night%20ly/move", Method::Post),
            ("http://hub/api/instances/i1/settings/workflows/project/deploy/start", Method::Post),
        ];
        for (i, (url, method)) in expected.iter().enumerate() {
            assert_eq!(reqs[i].url, *url, "request {i}");
            assert_eq!(reqs[i].method, *method, "request {i}");
        }
        assert_eq!(t.body(2), json!({ "description": "x" }));
        assert_eq!(t.body(5), json!({ "args": { "target": "src/" } }));
    }

    #[tokio::test]
    async fn run_queries_carry_the_session_id_and_the_journal_cursor() {
        let t = transport();
        t.push(ok_json(json!({ "ok": true, "runs": [] })));
        t.push(ok_json(json!({ "ok": true, "events": [] })));
        t.push(ok_json(json!({ "ok": true, "items": [] })));
        t.push(ok_json(json!({ "ok": true, "dataBase64": "" })));
        t.push(ok_json(json!({ "ok": true })));

        let c = client(&t);
        c.conversation_runs("i1", "sess 1", None).await.unwrap();
        c.run_events("i1", "sess 1", "run 2", Some(41)).await.unwrap();
        c.run_artifact_data("i1", "sess 1", "run 2", "a/b", Some(7), Some(50))
            .await
            .unwrap();
        c.run_artifact_read("i1", "sess 1", "run 2", "a/b", 3, 0, Some(1024))
            .await
            .unwrap();
        c.run_node_result("i1", "sess 1", "run 2", "site 1", 2)
            .await
            .unwrap();

        assert_eq!(
            t.urls(),
            vec![
                "http://hub/api/instances/i1/settings/workflow-runs?sessionId=sess+1",
                "http://hub/api/instances/i1/settings/workflow-runs/run%202/events?sessionId=sess+1&afterSequence=41",
                "http://hub/api/instances/i1/settings/workflow-runs/run%202/artifacts/a%2Fb/data?sessionId=sess+1&afterSequence=7&limit=50",
                "http://hub/api/instances/i1/settings/workflow-runs/run%202/artifacts/a%2Fb/read?sessionId=sess+1&version=3&offset=0&limit=1024",
                "http://hub/api/instances/i1/settings/workflow-runs/run%202/nodes/site%201/2?sessionId=sess+1",
            ]
        );
    }

    #[tokio::test]
    async fn resume_and_the_create_prompt_keep_their_body_shape_contracts() {
        let t = transport();
        t.push(ok_json(json!({ "ok": true })));
        t.push(ok_json(json!({ "ok": true, "prompt": "Help me…" })));

        let c = client(&t);
        c.workflow_resume("i1", "run 2", "sess 1", Some("deploy"))
            .await
            .unwrap();
        c.workflow_create_prompt("i1", Some(WorkflowScope::Global))
            .await
            .unwrap();

        let reqs = t.requested();
        assert_eq!(
            reqs[0].url,
            "http://hub/api/instances/i1/settings/workflow-runs/run%202/resume"
        );
        assert_eq!(reqs[0].method, Method::Post);
        assert_eq!(t.body(0), json!({ "sessionId": "sess 1", "name": "deploy" }));
        assert_eq!(
            reqs[1].url,
            "http://hub/api/instances/i1/settings/workflow-create-prompt?scope=global"
        );
    }

    #[tokio::test]
    async fn amend_run_settings_addresses_the_run_by_path_the_session_by_query() {
        let t = transport();
        t.push(ok_json(json!({
            "ok": true,
            "runId": "run-new",
            "toolCallId": "settings-1",
            "supersededRunId": "run-old"
        })));

        let res = client(&t)
            .amend_run_settings(
                "i1",
                "run 2",
                "sess 1",
                AmendRunSettingsBody {
                    subagent_model: Some(None),
                    max_concurrency: Some(Some(4)),
                },
            )
            .await
            .unwrap();

        let req = &t.requested()[0];
        assert_eq!(
            req.url,
            "http://hub/api/instances/i1/settings/workflow-runs/run%202/settings?sessionId=sess+1"
        );
        assert_eq!(req.method, Method::Post);
        // Key PRESENCE is the three-state discriminator — `null` must ride
        // the wire as a real JSON null, never be dropped or stringified.
        assert_eq!(
            t.body(0),
            json!({ "subagentModel": null, "maxConcurrency": 4 })
        );
        assert_eq!(res.run_id, "run-new");
        assert_eq!(res.superseded_run_id.as_deref(), Some("run-old"));
    }

    #[tokio::test]
    async fn a_workflow_refusal_surfaces_the_message_detail_over_the_reason_token() {
        let t = transport();
        // The workflow routes answer {ok:false, error:<token>, message:<detail>}
        // — for compile_failed the diagnostics ride in `message`, and
        // "compile_failed" alone would leave the user staring at a token.
        t.push(raw(
            422,
            r#"{"ok":false,"error":"compile_failed","message":"line 3: unexpected token"}"#,
        ));
        let err = client(&t)
            .workflow_start("i1", WorkflowScope::Project, "broken", None, None)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "line 3: unexpected token");
        assert_eq!(err.status(), Some(422));
    }

    #[tokio::test]
    async fn a_refusal_without_a_body_falls_back_to_the_status() {
        let t = transport();
        t.push(refused(409, "nonce is stale — refresh the reset card status"));
        let err = client(&t)
            .spend_reset_card(SpendResetCard {
                provider_id: "account:bigmodel-individual-coding-plan".into(),
                reset_type: ResetCardType::FiveHour,
                nonce: "n-1".into(),
                idempotency_key: "k-1".into(),
            })
            .await
            .unwrap_err();
        // The refused-write body IS the message (hub.ts refusal_detail).
        assert_eq!(
            err.to_string(),
            "nonce is stale — refresh the reset card status"
        );
    }

    #[tokio::test]
    async fn the_workflow_gate_probe_reads_the_per_instance_settings_snapshot() {
        let t = transport();
        // The machine-level /settings/all carries no gate verdict (its mount
        // has no backend), so the entry visibility must come from this
        // spelling.
        let mut body = settings_all_body();
        body["workflow"] = json!({ "enabled": true, "mode": "alwaysOn", "source": "remote" });
        t.push(ok_json(body));
        let all = client(&t).instance_settings_all("i1").await.unwrap();
        assert_eq!(
            all.workflow.and_then(|w| w.verdict()).map(|g| g.enabled),
            Some(true)
        );
        assert_eq!(t.urls()[0], "http://hub/api/instances/i1/settings/all");
    }

    #[tokio::test]
    async fn the_machine_level_degrade_block_still_decodes_the_whole_snapshot() {
        // The hub's /api/settings/all answers `workflow: {available:false}`
        // (settings-endpoint.ts:675). A strict WorkflowGateBlock failed the
        // ENTIRE snapshot decode on that shape, which broke the plan-quota
        // page's eligibility read (2026-10-22).
        let t = transport();
        let mut body = settings_all_body();
        body["workflow"] = json!({ "available": false });
        body["resetCards"]["providers"] =
            json!(["account:bigmodel-individual-coding-plan"]);
        t.push(ok_json(body));
        let all = client(&t).settings_all().await.unwrap();
        assert!(matches!(
            all.workflow,
            Some(crate::settings::SettingsWorkflowBlock::Unavailable { available: false })
        ));
        assert_eq!(
            all.reset_cards.providers,
            vec!["account:bigmodel-individual-coding-plan".to_string()]
        );
    }

    #[tokio::test]
    async fn unauthorized_is_its_own_answer() {
        let t = transport();
        t.push(raw(401, ""));
        let err = client(&t).settings_all().await.unwrap_err();
        assert_eq!(err, HubApiError::Unauthorized);
    }

    #[tokio::test]
    async fn a_transport_failure_is_a_network_error() {
        let t = transport();
        // No scripted response at all — the transport itself errors, the
        // "hub unreachable" state callers already treat as retry.
        let err = client(&t).settings_all().await.unwrap_err();
        assert!(matches!(err, HubApiError::Network(_)), "got {err:?}");
    }
}
