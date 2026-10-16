//! Hub REST client — the Rust port of the TS `HubClient` (`src/lib/hub.ts`):
//! bearer-authenticated discovery / history / filesystem routes with the
//! same error mapping (`HubApiError`), header-level deadlines (the
//! reconnect-wedge fix), verbatim cursor pagination, the fs line-window
//! contract, and the transparent 404 retry after a bridge upgrade.
//!
//! Scope note: only the routes the ported suite (`tests/hub-client.test.ts`)
//! exercises plus the ones the session flows need (`create_instance`) are
//! ported here; the settings/workflow REST families live in their own port.
//!
//! Transport: [`HttpTransport`] stands in for `fetch` — see `http.rs`. No
//! real HTTP stack is wired inside this crate (the UI-free dependency set is
//! fixed); the app layer supplies one.

use std::sync::Arc;

use serde_json::Value;

use crate::clock::Clock;
use crate::http::{
    encode_query_component, with_query, AbortSignal, HttpMethod, HttpRequest, HttpResponse,
    HttpTransport,
};
use crate::hub::{
    FsBrowseResult, FsListing, HubCreateInstanceResult, HubHistoryCursor, HubHistoryPage,
    HubInstance, HubProject,
};
use crate::settings::BackendRestartResult;
use crate::system_stats::SystemStatsResponse;

/// Hub API failure — the TS `HubApiError` field-for-field.
#[derive(Debug, Clone)]
pub struct HubApiError {
    pub message: String,
    /// HTTP status when the hub answered; `None` for network-level failures.
    pub status: Option<u16>,
    /// `true` = the fetch itself failed (hub unreachable) — an expected
    /// "offline" state, not an error.
    pub network: bool,
    /// The hub no longer knows / cannot reach the instance this request
    /// targeted (idle-exited bridge): the caller may respawn the instance.
    pub instance_dead: bool,
}

impl HubApiError {
    fn msg(message: impl Into<String>, status: Option<u16>) -> Self {
        Self {
            message: message.into(),
            status,
            network: false,
            instance_dead: false,
        }
    }
}

impl std::fmt::Display for HubApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HubApiError {}

/// Per-call deadline override (TS `opts.timeoutMs: number | null | undefined`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deadline {
    /// Unset — the default 10s header deadline.
    Default,
    /// `null` — wait indefinitely (slow-by-design routes: instance
    /// incubation, app-update install).
    Never,
    /// Explicit ms budget.
    After(u64),
}

impl Default for Deadline {
    fn default() -> Self {
        Deadline::Default
    }
}

const DEFAULT_TIMEOUT_MS: u64 = 10_000;

#[derive(Debug, Clone, Default)]
pub struct FetchOpts {
    pub deadline: Deadline,
    /// Statuses whose non-2xx BODY the caller needs (fs dead-instance
    /// sniffing); they ride back as raw responses instead of errors.
    pub passthrough: Vec<u16>,
}

/// Bearer-authenticated hub REST client. One instance per connection profile;
/// stateless beyond `(hub_url, token)`.
pub struct HubClient {
    hub_url: String,
    token: String,
    http: Arc<dyn HttpTransport>,
    clock: Arc<dyn Clock>,
}

impl HubClient {
    pub fn new(
        hub_url: impl Into<String>,
        token: impl Into<String>,
        http: Arc<dyn HttpTransport>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            hub_url: hub_url.into(),
            token: token.into(),
            http,
            clock,
        }
    }

    fn url(&self, path: &str) -> String {
        // TS: hubUrl.replace(/\/+$/, "") + path — trailing slashes collapse.
        format!("{}{}", self.hub_url.trim_end_matches('/'), path)
    }

    /// The shared fetch: bearer header, JSON body on writes, deadline
    /// watchdog, and the error mapping (401 / passthrough / body-as-message).
    async fn fetch(
        &self,
        path: &str,
        method: HttpMethod,
        body: Option<&Value>,
        opts: &FetchOpts,
    ) -> Result<HttpResponse, HubApiError> {
        let mut headers = vec![("authorization".to_string(), format!("Bearer {}", self.token))];
        let body_str = body.map(|v| v.to_string());
        if body_str.is_some() {
            headers.push(("content-type".to_string(), "application/json".to_string()));
        }
        let req = HttpRequest {
            url: self.url(path),
            method,
            headers,
            body: body_str,
        };

        let abort = AbortSignal::new();
        let deadline_ms = ms_of(opts.deadline);
        let outcome = match opts.deadline {
            Deadline::Never => self.http.fetch(req, abort).await,
            _ => {
                // Deadline watcher: abort the transport at the budget. The
                // sleep is created (registered) EAGERLY at call time —
                // setTimeout semantics — so a mock clock's `advance` cannot
                // race the watcher task's first poll. The await below
                // deliberately has no local escape hatch — a transport that
                // ignores the abort leaves it dangling, the exact zombie
                // shape the supervisor watchdog tears down.
                let sleep = self.clock.sleep_ms(deadline_ms);
                let abort_for_watcher = abort.clone();
                let watcher = tokio::spawn(async move {
                    sleep.await;
                    abort_for_watcher.fire();
                });
                let outcome = self.http.fetch(req, abort).await;
                watcher.abort();
                outcome
            }
        };

        match outcome {
            Err(e) => {
                let message = if e.aborted {
                    format!("network error: no response within {deadline_ms}ms ({path})")
                } else {
                    format!("network error: {}", e.message)
                };
                Err(HubApiError {
                    message,
                    status: None,
                    network: true,
                    instance_dead: false,
                })
            }
            Ok(res) => {
                if res.status == 401 {
                    return Err(HubApiError::msg("unauthorized: check token", Some(401)));
                }
                // Callers that inspect a non-2xx BODY (fetchFs sniffs the
                // hub's plain-text dead-instance answers) get the raw
                // response back instead.
                if opts.passthrough.contains(&res.status) {
                    return Ok(res);
                }
                if !(200..300).contains(&res.status) {
                    // A refused write answers with a REASON; the workflow
                    // routes carry `message` (detail) next to `error`
                    // (token) — prefer the detail when present.
                    let detail = res.json().ok().and_then(|b| {
                        let pick = |key: &str| {
                            b.get(key)
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                                .map(str::to_string)
                        };
                        pick("message").or_else(|| pick("error"))
                    });
                    return Err(HubApiError::msg(
                        detail.unwrap_or_else(|| format!("HTTP {}", res.status)),
                        Some(res.status),
                    ));
                }
                Ok(res)
            }
        }
    }

    /// Body is plain text "ok".
    pub async fn health(&self) -> Result<(), HubApiError> {
        let _ = self.fetch("/api/health", HttpMethod::Get, None, &FetchOpts::default()).await?;
        Ok(())
    }

    /// Registered instances; with `probe` the hub TCP-probes each bridge
    /// first (reconnect liveness checks want an honest list).
    pub async fn instances(&self, probe: bool) -> Result<Vec<HubInstance>, HubApiError> {
        let path = if probe {
            "/api/instances?probe=1"
        } else {
            "/api/instances"
        };
        let res = self.fetch(path, HttpMethod::Get, None, &FetchOpts::default()).await?;
        let data = res.json().map_err(|e| HubApiError::msg(e.to_string(), None))?;
        if !data.is_array() {
            return Err(HubApiError::msg("unexpected /api/instances payload", None));
        }
        serde_json::from_value(data)
            .map_err(|e| HubApiError::msg(format!("unexpected /api/instances payload: {e}"), None))
    }

    /// The one-line quota dock string; `None` = no quota data (hide, not an
    /// error).
    pub async fn quota_dock(&self) -> Result<Option<String>, HubApiError> {
        let res = self.fetch("/api/quota/dock", HttpMethod::Get, None, &FetchOpts::default()).await?;
        let data = res.json().map_err(|e| HubApiError::msg(e.to_string(), None))?;
        Ok(data
            .get("formatted")
            .and_then(Value::as_str)
            .map(str::to_string))
    }

    /// Create (or reuse) a serve bridge for a known project; since ADR-0016
    /// the spawn opens a visible terminal REPL (~20s registration budget) —
    /// no client-side deadline. `session_id` boots the spawned bridge
    /// straight into that session (resume); the answer is always the NEW
    /// instance.
    pub async fn create_instance(
        &self,
        workspace_path: &str,
        session_id: Option<&str>,
    ) -> Result<HubCreateInstanceResult, HubApiError> {
        let mut body = serde_json::json!({ "workspacePath": workspace_path });
        if let Some(sid) = session_id {
            body["sessionId"] = Value::String(sid.to_string());
        }
        let res = self
            .fetch(
                "/api/instances",
                HttpMethod::Post,
                Some(&body),
                &FetchOpts {
                    deadline: Deadline::Never,
                    ..FetchOpts::default()
                },
            )
            .await?;
        parse_body(res)
    }

    /// Known-project workspaces (`GET /api/projects`, server `KnownWorkspace`
    /// rows) — the history browser's project chooser (TS `projects()`).
    pub async fn projects(&self) -> Result<Vec<HubProject>, HubApiError> {
        let res = self.fetch("/api/projects", HttpMethod::Get, None, &FetchOpts::default()).await?;
        let data = res.json().map_err(|e| HubApiError::msg(e.to_string(), None))?;
        if !data.is_array() {
            return Err(HubApiError::msg("unexpected /api/projects payload", None));
        }
        serde_json::from_value(data)
            .map_err(|e| HubApiError::msg(format!("unexpected /api/projects payload: {e}"), None))
    }

    /// One page of a project's session history. `cursor` is the previous
    /// response's `nextCursor` passed back VERBATIM — the composite
    /// `{before, beforeId}` names the exact last row. A cold project's
    /// first page incubates its serve bridge (~12s) — no deadline.
    pub async fn project_sessions(
        &self,
        workspace_path: &str,
        cursor: Option<&HubHistoryCursor>,
    ) -> Result<HubHistoryPage, HubApiError> {
        let mut params = vec![format!(
            "workspacePath={}",
            encode_query_component(workspace_path)
        )];
        if let Some(c) = cursor {
            params.push(format!("before={}", c.before));
            params.push(format!("beforeId={}", encode_query_component(&c.before_id)));
        }
        let path = with_query("/api/projects/sessions", &params);
        let res = self
            .fetch(
                &path,
                HttpMethod::Get,
                None,
                &FetchOpts {
                    deadline: Deadline::Never,
                    ..FetchOpts::default()
                },
            )
            .await?;
        parse_body(res)
    }

    /// Soft-deletes (tombstones) a whole project (server ADR-0031): a hide,
    /// not a ban. Hub-level route; the workspace path rides as JSON so any
    /// path shape survives unencoded.
    pub async fn delete_project(&self, workspace_path: &str) -> Result<(), HubApiError> {
        let body = serde_json::json!({ "workspacePath": workspace_path });
        self.fetch(
            "/api/projects/delete",
            HttpMethod::Post,
            Some(&body),
            &FetchOpts::default(),
        )
        .await?;
        Ok(())
    }

    /// Retires a session from remote discovery (ADR-0006 — the "end session"
    /// action). Close, not delete: backend store and editor storage are
    /// untouched; an editor-side-still-open conversation self-heals back into
    /// discovery on its next use. Refused with 409 while a turn is running.
    /// Id encoding matches delete_session (instance ids are hub-numeric).
    pub async fn close_session(&self, instance_id: &str, session_id: &str) -> Result<(), HubApiError> {
        let path = format!(
            "/api/instances/{instance_id}/sessions/{}/close",
            encode_query_component(session_id)
        );
        self.fetch(&path, HttpMethod::Post, None, &FetchOpts::default())
            .await?;
        Ok(())
    }

    /// Renames a session (bridge 0.11.9). The title is set once automatically
    /// from the first prompt; this is the only later modifier — the bridge
    /// pins it (title_overridden) and broadcasts session_info_update so
    /// attached editors update live.
    pub async fn rename_session(
        &self,
        instance_id: &str,
        session_id: &str,
        title: &str,
    ) -> Result<(), HubApiError> {
        let path = format!(
            "/api/instances/{instance_id}/sessions/{}/rename",
            encode_query_component(session_id)
        );
        let body = serde_json::json!({ "title": title });
        self.fetch(&path, HttpMethod::Post, Some(&body), &FetchOpts::default())
            .await?;
        Ok(())
    }

    /// Soft-deletes a session from every listing (bridge >=0.53). The
    /// session id is encoded, the instance id rides raw — hub instance ids
    /// are hub-numeric (same shape as close/rename).
    pub async fn delete_session(&self, instance_id: &str, session_id: &str) -> Result<(), HubApiError> {
        let path = format!(
            "/api/instances/{instance_id}/sessions/{}/delete",
            encode_query_component(session_id)
        );
        self.fetch(&path, HttpMethod::Post, None, &FetchOpts::default())
            .await?;
        Ok(())
    }

    /// One directory level of the hub machine's filesystem. Absent `path`
    /// lists the home dir; navigate by the RESOLVED `path` in the answer.
    pub async fn fs_browse(&self, path: Option<&str>) -> Result<FsBrowseResult, HubApiError> {
        let full = match path {
            Some(p) => with_query("/api/fs/list", &[format!("path={}", encode_query_component(p))]),
            None => "/api/fs/list".to_string(),
        };
        let res = self.fetch(&full, HttpMethod::Get, None, &FetchOpts::default()).await?;
        parse_body(res)
    }

    /// Machine-level system status (`/api/system-stats`, bridge 0.62.0).
    pub async fn system_stats(&self) -> Result<SystemStatsResponse, HubApiError> {
        let res = self.fetch("/api/system-stats", HttpMethod::Get, None, &FetchOpts::default()).await?;
        parse_body(res)
    }

    /// Restart one bridge's backend so its `needs-restart` writes take
    /// effect (the `settings_client::restart_backend` semantics over the
    /// driver's REST handle): per-instance on purpose — the hub's own
    /// settings mount has no backend, so the restart must name the bridge it
    /// means to disturb. Stop + respawn of the sandboxed backend is
    /// seconds-scale, hence the 30s deadline instead of the 10s default.
    pub async fn restart_backend(
        &self,
        instance_id: &str,
    ) -> Result<BackendRestartResult, HubApiError> {
        let path = format!(
            "/api/instances/{}/backend/restart",
            crate::settings_client::encode_uri_component(instance_id)
        );
        let res = self
            .fetch(
                &path,
                HttpMethod::Post,
                None,
                &FetchOpts {
                    deadline: Deadline::After(30_000),
                    ..FetchOpts::default()
                },
            )
            .await?;
        parse_body(res)
    }

    // ---- Session Files (per-instance fs proxy) ----

    /// 404/502 bodies are needed here (dead-instance sniffing), so both
    /// statuses ride the passthrough; a 404 right after a bridge upgrade is
    /// retried once before being believed.
    async fn fetch_fs(&self, path: &str) -> Result<HttpResponse, HubApiError> {
        let opts = FetchOpts {
            passthrough: vec![404, 502],
            ..FetchOpts::default()
        };
        let mut res = self.fetch(path, HttpMethod::Get, None, &opts).await?;
        if res.status == 404 {
            // Bridge-upgrade race: the route briefly 404s around a respawn.
            self.clock.sleep_ms(400).await;
            res = self.fetch(path, HttpMethod::Get, None, &opts).await?;
        }
        if res.status == 404 || res.status == 502 {
            let text = res.body.clone();
            if text == "unknown instance" || text == "bridge unreachable" {
                return Err(HubApiError {
                    message: text,
                    status: Some(res.status),
                    network: false,
                    instance_dead: true,
                });
            }
            return Ok(res);
        }
        Ok(res)
    }

    /// One directory level of the active session; dotfiles included.
    pub async fn fs_list(
        &self,
        instance_id: &str,
        session_id: &str,
        path: &str,
    ) -> Result<FsListing, HubApiError> {
        let query = with_query(
            "",
            &[
                format!("sessionId={}", encode_query_component(session_id)),
                format!("path={}", encode_query_component(path)),
            ],
        );
        let res = self
            .fetch_fs(&format!("/api/instances/{instance_id}/fs/list{query}"))
            .await?;
        parse_body(res)
    }

    /// Text line window. `first_line` comes from the X-Zcode-First-Line
    /// header — always trust it over the request's `line`; the server's
    /// trailing newline is stripped so window stitching stays exact.
    pub async fn fs_file_text(
        &self,
        instance_id: &str,
        session_id: &str,
        path: &str,
        line: u64,
        limit: u64,
    ) -> Result<(u64, String), HubApiError> {
        let query = with_query(
            "",
            &[
                format!("sessionId={}", encode_query_component(session_id)),
                format!("path={}", encode_query_component(path)),
                format!("line={line}"),
                format!("limit={limit}"),
            ],
        );
        let res = self
            .fetch_fs(&format!("/api/instances/{instance_id}/fs/file{query}"))
            .await?;
        let first_line = res
            .header("x-zcode-first-line")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(line);
        let raw = res.body.clone();
        let text = raw.strip_suffix('\n').unwrap_or(&raw).to_string();
        Ok((first_line, text))
    }

    /// Absolute fs/file URL with the token in the query string — for image
    /// and download links, which cannot carry an Authorization header.
    pub fn fs_file_url(&self, instance_id: &str, session_id: &str, path: &str) -> String {
        let query = with_query(
            "",
            &[
                format!("sessionId={}", encode_query_component(session_id)),
                format!("path={}", encode_query_component(path)),
                format!("token={}", encode_query_component(&self.token)),
            ],
        );
        self.url(&format!("/api/instances/{instance_id}/fs/file{query}"))
    }

    /// The whole fs/file as raw bytes (image previews). Same URL shape as
    /// [`Self::fs_file_url`] — the token rides the query because the fetch
    /// cannot reuse `fetch`'s JSON body pipeline: its lossy UTF-8 body would
    /// corrupt a raster. The default header deadline still applies.
    pub async fn fs_file_bytes(
        &self,
        instance_id: &str,
        session_id: &str,
        path: &str,
    ) -> Result<Vec<u8>, HubApiError> {
        let req = HttpRequest {
            url: self.fs_file_url(instance_id, session_id, path),
            method: HttpMethod::Get,
            headers: Vec::new(),
            body: None,
        };
        let abort = AbortSignal::new();
        // Same watchdog shape as `fetch`: the sleep future is created eagerly
        // (mock clocks need the registration), the watcher fires the abort.
        let sleep = self.clock.sleep_ms(DEFAULT_TIMEOUT_MS);
        let watcher = {
            let abort = abort.clone();
            tokio::spawn(async move {
                sleep.await;
                abort.fire();
            })
        };
        let outcome = self.http.fetch_bytes(req, abort).await;
        watcher.abort();
        match outcome {
            Err(e) => {
                let message = if e.aborted {
                    format!("network error: no response within {DEFAULT_TIMEOUT_MS}ms (fs/file)")
                } else {
                    format!("network error: {}", e.message)
                };
                Err(HubApiError {
                    message,
                    status: None,
                    network: true,
                    instance_dead: false,
                })
            }
            Ok(res) => {
                if res.status == 401 {
                    return Err(HubApiError::msg("unauthorized: check token", Some(401)));
                }
                if !(200..300).contains(&res.status) {
                    return Err(HubApiError::msg(format!("HTTP {}", res.status), Some(res.status)));
                }
                Ok(res.body)
            }
        }
    }
}

fn ms_of(deadline: Deadline) -> u64 {
    match deadline {
        Deadline::After(ms) => ms,
        _ => DEFAULT_TIMEOUT_MS,
    }
}

fn parse_body<T: serde::de::DeserializeOwned>(res: HttpResponse) -> Result<T, HubApiError> {
    let status = res.status;
    res.json()
        .and_then(|value| serde_json::from_value(value).map_err(|e| e.into()))
        .map_err(|e| HubApiError::msg(e.to_string(), Some(status)))
}

#[cfg(test)]
mod tests;
