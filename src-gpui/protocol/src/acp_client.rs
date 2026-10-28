//! ACP-over-WebSocket connection — the Rust port of the TS `AcpConnection`
//! (`src/lib/acp.ts`): connect + `initialize` under one 15s deadline (zombie
//! sockets never fire WS events), JSON-RPC request/notification routing,
//! server-request (permission/elicitation) dispatch, streaming
//! `session/update` dispatch with the notification-level `_meta`, and the
//! `$/zcode/*` bridge extensions.
//!
//! One connection owns one socket, bound to one bridge instance for its
//! whole lifetime; reconnection means building a new one (REMOTE-CLIENTS.md).
//!
//! Desktop Presence: `initialize` carries
//! `clientInfo: { name: "zcode-acp-remote-desktop", version }` — the identity
//! the hub peeks off the first frame (zcode-acp-server
//! `src/remote/hub-server.ts` `DESKTOP_CLIENT_NAME`); while it is connected,
//! every NEW session incubates headless (docs/adr/0010). The mobile TS app
//! sends no clientInfo — this is the desktop-only addition.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use crate::acp::{ConfigOption, SessionUpdate};
use crate::clock::{BoxFuture, Clock};
use crate::session::LoadIntent;
use crate::ws::{WsEvent, WsFactory, WsStream};

/// The Desktop Presence identity (server hub-server.ts `DESKTOP_CLIENT_NAME`).
pub const DESKTOP_CLIENT_NAME: &str = "zcode-acp-remote-desktop";

/// Deadline for open + initialize; WS events alone can't be trusted to fire
/// (zombie sockets after deep sleep / network switch).
pub const CONNECT_TIMEOUT_MS: u64 = 15_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpConnectionState {
    Connecting,
    Open,
    Closed,
}

/// A server->client request (permission / elicitation).
#[derive(Debug, Clone)]
pub struct ServerRequest {
    pub id: u64,
    pub method: String,
    pub params: Value,
}

/// Turn usage/wall-time carried by the `$/zcode/turnState` END transition —
/// optional, both fields (bridge emits them so attach-only clients can paint
/// cache/speed readouts for turns driven from elsewhere).
#[derive(Debug, Clone, PartialEq)]
pub struct TurnStateInfo {
    pub usage: Option<Value>,
    pub turn_ms: Option<f64>,
}

/// Connection events, the TS `AcpHandlers` trait-for-trait.
pub trait AcpHandlers: Send + Sync {
    fn on_state(&self, state: AcpConnectionState);
    /// `meta` is the notification-level `_meta` (replay flags like
    /// `zcode.collapsed` ride there, not inside the update).
    fn on_update(&self, session_id: &str, update: &SessionUpdate, meta: Option<&Value>);
    fn on_server_request(&self, req: ServerRequest, respond: ServerResponder);
    fn on_cancel_request(&self, id: u64);
    /// `$/zcode/ask_settled` — the bridge's cross-client dismissal hint: an
    /// interaction ask (permission/elicitation) was decided on another
    /// client, or the bridge's wait broke. The losing client's card must
    /// dismiss. Default no-op: the ACP SDK never forwards
    /// `$/cancel_request` to losers, so this notification is the only
    /// working dismissal (zcode-acp-server `emitAskSettled`).
    fn on_ask_settled(&self, _session_id: &str, _kind: Option<&str>, _tool_call_id: Option<&str>) {}
    fn on_turn_state(&self, session_id: &str, running: bool, turn: Option<TurnStateInfo>);
    fn on_workflow_list_changed(&self) {}
    /// `$/zcode/session_closed` — the bridge's cross-client retirement
    /// notice: another client closed this conversation through the hub, and
    /// a serve bridge is about to self-exit when that was its last session
    /// (ADR-0006/0016). Holders drop the session's tab instead of reading
    /// the following disconnect as a dead backend to revive. One copy per
    /// session alias — route by the payload's session id. Default no-op.
    fn on_session_closed(&self, _session_id: &str) {}
}

/// Answer channel for a server request — carries the request id back on the
/// wire (`respondServerRequest`).
#[derive(Clone)]
pub struct ServerResponder {
    core: Arc<ConnCore>,
    id: u64,
}

impl std::fmt::Debug for ServerResponder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerResponder").field("id", &self.id).finish()
    }
}

impl ServerResponder {
    pub fn send(&self, result: Value) {
        self.core.send_json(&json!({ "jsonrpc": "2.0", "id": self.id, "result": result }));
    }
}

/// Request failure. Messages match the TS spellings verbatim — callers (and
/// the ported tests) match on them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpRequestError(pub String);

impl std::fmt::Display for AcpRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AcpRequestError {}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectError {
    #[error("connect timed out")]
    Timeout,
    #[error("connection closed before open")]
    ClosedBeforeOpen,
    #[error("WebSocket error (bad URL, token, or instance id)")]
    WebSocket,
    #[error("{0}")]
    Initialize(String),
}

/// `initialize` result — kept raw; helpers read the capability probes we
/// consume (`agentCapabilities._meta.zcode.fs`, `agentInfo.version`).
#[derive(Debug, Clone)]
pub struct AcpInitializeResult {
    pub raw: Value,
}

impl AcpInitializeResult {
    pub fn protocol_version(&self) -> Option<i64> {
        self.raw.get("protocolVersion").and_then(Value::as_i64)
    }

    /// The zcode fs capability (Session Files gating).
    pub fn fs_capability(&self) -> bool {
        self.raw
            .pointer("/agentCapabilities/_meta/zcode/fs")
            .and_then(Value::as_bool)
            == Some(true)
    }

    /// The bridge's advertised version (page-marker gating on the TS side).
    pub fn agent_version(&self) -> Option<&str> {
        self.raw.pointer("/agentInfo/version").and_then(Value::as_str)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Connecting,
    Open,
    Closed,
}

struct ConnCore {
    stream: Mutex<Option<Arc<dyn WsStream>>>,
    phase: Mutex<Phase>,
    socket_open: AtomicBool,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    closed_by_us: AtomicBool,
    handlers: Arc<dyn AcpHandlers>,
}

impl ConnCore {
    /// The TS `send()`: only while the socket is open, silently otherwise.
    fn send_json(&self, msg: &Value) {
        if !self.socket_open.load(Ordering::SeqCst) {
            return;
        }
        if let Some(stream) = self.stream.lock().unwrap().as_ref() {
            stream.send(&msg.to_string());
        }
    }

    fn respond(&self, id: u64, result: Value) {
        self.send_json(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    fn fail_all_pending(&self, message: &str) {
        let mut pending = self.pending.lock().unwrap();
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(message.to_string()));
        }
    }
}

#[derive(Clone)]
pub struct AcpConnection {
    core: Arc<ConnCore>,
}

impl std::fmt::Debug for AcpConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AcpConnection")
    }
}

impl AcpConnection {
    /// Build the connection and start its handshake. Returns the handle
    /// immediately — the supervisor stores it BEFORE the handshake settles so
    /// a teardown mid-connect can close the socket (the TS store assigns
    /// `acp = conn` before `await conn.connect()`) — plus the connect future,
    /// which resolves once the socket is open and `initialize` completed.
    pub fn start(
        hub_url: &str,
        token: &str,
        instance_id: &str,
        handlers: Arc<dyn AcpHandlers>,
        factory: Arc<dyn WsFactory>,
        clock: Arc<dyn Clock>,
    ) -> (AcpConnection, BoxFuture<'static, Result<AcpInitializeResult, ConnectError>>) {
        handlers.on_state(AcpConnectionState::Connecting);
        let url = match crate::ws::ws_url(hub_url, instance_id) {
            Ok(url) => url,
            Err(_) => {
                let conn = AcpConnection {
                    core: Arc::new(ConnCore {
                        stream: Mutex::new(None),
                        phase: Mutex::new(Phase::Closed),
                        socket_open: AtomicBool::new(false),
                        next_id: AtomicU64::new(1),
                        pending: Mutex::new(HashMap::new()),
                        closed_by_us: AtomicBool::new(true),
                        handlers,
                    }),
                };
                let fut: BoxFuture<'static, _> = Box::pin(async move {
                    Err(ConnectError::WebSocket)
                });
                return (conn, fut);
            }
        };

        let core = Arc::new(ConnCore {
            // The factory future is awaited inside the connect future below,
            // but the handle must exist first — the stream slot fills in as
            // soon as the factory resolves; close() on a not-yet-created
            // socket marks the connection dead and the work short-circuits.
            stream: Mutex::new(None),
            phase: Mutex::new(Phase::Connecting),
            socket_open: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            closed_by_us: AtomicBool::new(false),
            handlers,
        });
        let conn = AcpConnection {
            core: Arc::clone(&core),
        };

        let token = token.to_string();
        let factory_url = url;
        let connect_fut = {
            let core = Arc::clone(&core);
            let factory = Arc::clone(&factory);
            let clock = Arc::clone(&clock);
            Box::pin(async move {
                // A zombie socket can sit open forever without ever answering
                // `initialize` — the deadline below is the only way out.
                let deadline = clock.sleep_ms(CONNECT_TIMEOUT_MS);
                let work = async {
                    let (stream, events) = factory
                        .connect(&factory_url, &token)
                        .await
                        .map_err(|_| ConnectError::WebSocket)?;
                    // A close() that raced the factory means the handshake is
                    // already moot.
                    if *core.phase.lock().unwrap() == Phase::Closed {
                        stream.close();
                        return Err(ConnectError::ClosedBeforeOpen);
                    }
                    *core.stream.lock().unwrap() = Some(stream);
                    let (open_tx, open_rx) = oneshot::channel::<Result<(), ConnectError>>();
                    tokio::spawn(pump(Arc::clone(&core), events, open_tx));

                    // Await open…
                    match open_rx.await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => return Err(e),
                        Err(_) => return Err(ConnectError::ClosedBeforeOpen),
                    }
                    // …then initialize (protocolVersion MUST be the number 1).
                    // The form capability makes the bridge route
                    // ExitPlanMode / AskUserQuestion through the richer
                    // elicitation/create form deterministically (capabilities
                    // OR-merge across attached clients). clientInfo is the
                    // Desktop Presence identity (module doc).
                    let params = json!({
                        "protocolVersion": 1,
                        "clientCapabilities": { "elicitation": { "form": true } },
                        "clientInfo": {
                            "name": DESKTOP_CLIENT_NAME,
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                    });
                    let result = core
                        .request_eager("initialize", params)
                        .map_err(|e| ConnectError::Initialize(e.0))?;
                    match result.await {
                        Ok(raw) => Ok(raw),
                        Err(e) => Err(ConnectError::Initialize(e.0)),
                    }
                };
                tokio::pin!(work);
                tokio::pin!(deadline);
                tokio::select! {
                    biased;
                    result = &mut work => match result {
                        Ok(raw) => {
                            *core.phase.lock().unwrap() = Phase::Open;
                            // TS order: settle(resolve) then onState("open").
                            core.handlers.on_state(AcpConnectionState::Open);
                            Ok(AcpInitializeResult { raw })
                        }
                        Err(e) => {
                            close_core(&core);
                            Err(e)
                        }
                    },
                    _ = &mut deadline => {
                        close_core(&core);
                        Err(ConnectError::Timeout)
                    }
                }
            })
        };
        (conn, connect_fut)
    }

    /// Convenience wrapper over [`AcpConnection::start`] for direct callers
    /// (the ported acp-connection tests and any single-shot consumer).
    pub async fn connect(
        hub_url: &str,
        token: &str,
        instance_id: &str,
        handlers: Arc<dyn AcpHandlers>,
        factory: Arc<dyn WsFactory>,
        clock: Arc<dyn Clock>,
    ) -> Result<(AcpConnection, AcpInitializeResult), ConnectError> {
        let (conn, fut) = Self::start(hub_url, token, instance_id, handlers, factory, clock);
        let init = fut.await?;
        Ok((conn, init))
    }

    /// JSON-RPC request. Registers the pending entry EAGERLY (at call time,
    /// like the TS promise executor) so a subsequent `close()` rejects it
    /// with "connection closed" rather than "connection not open". No
    /// timeout: `session/prompt` legitimately runs for minutes.
    pub fn request(
        &self,
        method: &str,
        params: Value,
    ) -> BoxFuture<'static, Result<Value, AcpRequestError>> {
        match self.core.request_eager(method, params) {
            Ok(fut) => fut,
            Err(err) => Box::pin(async move { Err(err) }),
        }
    }

    /// Notification (no id, no response).
    pub fn notify(&self, method: &str, params: Value) {
        self.core
            .send_json(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    pub fn respond_server_request(&self, id: u64, result: Value) {
        self.core.respond(id, result);
    }

    pub fn is_open(&self) -> bool {
        *self.core.phase.lock().unwrap() == Phase::Open
    }

    pub fn close(&self) {
        close_core(&self.core);
    }
}

impl ConnCore {
    /// Register + send a request, returning the boxed awaitable. `Err`
    /// carries the immediate failure ("connection not open" — the gate is
    /// the SOCKET state, like the TS `ws.readyState === OPEN` check, so the
    /// in-flight `initialize` itself passes it).
    fn request_eager(
        self: &Arc<Self>,
        method: &str,
        params: Value,
    ) -> Result<BoxFuture<'static, Result<Value, AcpRequestError>>, AcpRequestError> {
        if !self.socket_open.load(Ordering::SeqCst) {
            return Err(AcpRequestError("connection not open".to_string()));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.send_json(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        Ok(Box::pin(async move {
            match rx.await {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(message)) => Err(AcpRequestError(message)),
                // Pending entries are only dropped by fail_all_pending, so a
                // dropped channel always means "connection closed".
                Err(_) => Err(AcpRequestError("connection closed".to_string())),
            }
        }))
    }
}

/// TS `close()`: mark closed-by-us, fail pending, close the socket.
fn close_core(core: &Arc<ConnCore>) {
    core.closed_by_us.store(true, Ordering::SeqCst);
    core.fail_all_pending("connection closed");
    if let Some(stream) = core.stream.lock().unwrap().take() {
        stream.close();
    }
    core.socket_open.store(false, Ordering::SeqCst);
    *core.phase.lock().unwrap() = Phase::Closed;
}

/// Frame pump: routes socket events to the pending map / handlers. Runs for
/// the lifetime of the socket; on `Closed` it mirrors `ws.onclose` — fail
/// every pending request, surface `closed` state unless we closed ourselves.
async fn pump(
    core: Arc<ConnCore>,
    mut events: mpsc::Receiver<WsEvent>,
    open_tx: oneshot::Sender<Result<(), ConnectError>>,
) {
    let mut open_tx = Some(open_tx);
    loop {
        match events.recv().await {
            Some(WsEvent::Open) => {
                core.socket_open.store(true, Ordering::SeqCst);
                if let Some(tx) = open_tx.take() {
                    let _ = tx.send(Ok(()));
                }
            }
            Some(WsEvent::Text(text)) => route_message(&core, &text),
            Some(WsEvent::Closed) | None => break,
        }
    }
    core.socket_open.store(false, Ordering::SeqCst);
    // Pre-open close settles the connect attempt instead.
    if let Some(tx) = open_tx.take() {
        let _ = tx.send(Err(ConnectError::ClosedBeforeOpen));
    }
    core.fail_all_pending("connection closed");
    if !core.closed_by_us.load(Ordering::SeqCst) {
        core.handlers.on_state(AcpConnectionState::Closed);
    }
    *core.phase.lock().unwrap() = Phase::Closed;
}

/// One inbound text frame — the TS `handleMessage`.
fn route_message(core: &Arc<ConnCore>, data: &str) {
    let Ok(msg) = serde_json::from_str::<Value>(data) else {
        return; // Binary frames and unparseable payloads are ignored.
    };

    if let Some(method) = msg.get("method").and_then(Value::as_str) {
        let id = msg.get("id");
        if id.is_some() {
            // Server -> client request (permission / elicitation). Bridge ids
            // are JSON numbers; anything else is not ours to answer.
            if let Some(id) = id.and_then(Value::as_u64) {
                let req = ServerRequest {
                    id,
                    method: method.to_string(),
                    params: msg.get("params").cloned().unwrap_or_else(|| json!({})),
                };
                let respond = ServerResponder {
                    core: Arc::clone(core),
                    id,
                };
                core.handlers.on_server_request(req, respond);
            }
            return;
        }
        let params = msg.get("params");
        match method {
            "session/update" => {
                // The replay `_meta` (zcode.collapsed etc.) is TOP-LEVEL on
                // the notification per the wire contract; params-level is a
                // fallback.
                let session_id = params
                    .and_then(|p| p.get("sessionId"))
                    .and_then(Value::as_str);
                let update = params.and_then(|p| p.get("update")).filter(|u| u.is_object());
                let (Some(session_id), Some(update)) = (session_id, update) else {
                    return;
                };
                let Ok(update) = serde_json::from_value::<SessionUpdate>(update.clone()) else {
                    return;
                };
                let meta = msg
                    .get("_meta")
                    .or_else(|| params.and_then(|p| p.get("_meta")))
                    .filter(|m| m.is_object());
                core.handlers.on_update(session_id, &update, meta);
            }
            "$/zcode/turnState" => {
                let p = params;
                let session_id = p.and_then(|p| p.get("sessionId")).and_then(Value::as_str);
                let running = p.and_then(|p| p.get("running")).and_then(Value::as_bool);
                let (Some(session_id), Some(running)) = (session_id, running) else {
                    return;
                };
                let usage = p
                    .and_then(|p| p.get("usage"))
                    .filter(|u| !u.is_null())
                    .cloned();
                let turn_ms = p.and_then(|p| p.get("turnMs")).and_then(Value::as_f64);
                // The turn block exists only when the END transition carries
                // at least one of usage / turnMs.
                let turn = if usage.is_some() || turn_ms.is_some() {
                    Some(TurnStateInfo { usage, turn_ms })
                } else {
                    None
                };
                core.handlers.on_turn_state(session_id, running, turn);
            }
            "$/zcode/workflowListChanged" => core.handlers.on_workflow_list_changed(),
            "$/zcode/ask_settled" => {
                // Payload: {sessionId, kind: "permission"|"elicitation",
                // toolCallId?} — best-effort, missing fields fall back to
                // None (session-keyed dismissal).
                if let Some(session_id) = params
                    .and_then(|p| p.get("sessionId"))
                    .and_then(Value::as_str)
                {
                    let kind = params
                        .and_then(|p| p.get("kind"))
                        .and_then(Value::as_str);
                    let tool_call_id = params
                        .and_then(|p| p.get("toolCallId"))
                        .and_then(Value::as_str);
                    core.handlers
                        .on_ask_settled(session_id, kind, tool_call_id);
                }
            }
            "$/zcode/session_closed" => {
                // Payload: {sessionId} — one copy per session alias.
                if let Some(session_id) = params
                    .and_then(|p| p.get("sessionId"))
                    .and_then(Value::as_str)
                {
                    core.handlers.on_session_closed(session_id);
                }
            }
            "$/cancel_request" => {
                if let Some(id) = params
                    .and_then(|p| p.get("id"))
                    .and_then(Value::as_u64)
                {
                    core.handlers.on_cancel_request(id);
                }
            }
            _ => {}
        }
        return;
    }

    if let Some(id) = msg.get("id").and_then(Value::as_u64) {
        let mut pending = core.pending.lock().unwrap();
        if let Some(tx) = pending.remove(&id) {
            if let Some(err) = msg.get("error") {
                let message = err
                    .get("message")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("JSON-RPC error")
                    .to_string();
                let _ = tx.send(Err(message));
            } else {
                let _ = tx.send(Ok(msg.get("result").cloned().unwrap_or(Value::Null)));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// session/new and session/load wire shapes (src/store/appStore.ts)
// ---------------------------------------------------------------------------

/// Attach tail size: a `session/load` replays only the last
/// [`REPLAY_TAIL_LIMIT`] messages; older pages arrive via `session/load_earlier`.
pub const REPLAY_TAIL_LIMIT: u64 = 30;

/// `session/new` params. `cwd` is REQUIRED by the ACP schema — the serve-mode
/// bridge ignores the value in favor of the process cwd, but the request is
/// rejected -32602 without one.
pub fn session_new_params(cwd: &str) -> Value {
    json!({ "cwd": cwd, "mcpServers": [] })
}

/// `session/load` params. `cwd` is REQUIRED by the SDK schema (no default);
/// the bridge ignores the value on load (roots are backend-authoritative),
/// so an unknown workspace sends a "/" placeholder. The tail-replay limit
/// rides in `_meta` (top-level unknown keys are stripped by the SDK schema);
/// limit 0 is the bridge's metadata-only attach. `intent` (see
/// [`crate::session::LoadIntent`]): only `Auto` is sent — `User` keeps the
/// old wire shape (the field's absence is the bridge's "user" default).
pub fn session_load_params(
    session_id: &str,
    cwd: &str,
    limit: u64,
    intent: LoadIntent,
) -> Value {
    let mut zcode = json!({ "limit": limit });
    if intent == LoadIntent::Auto {
        zcode["intent"] = json!("auto");
    }
    json!({
        "sessionId": session_id,
        "cwd": cwd,
        "mcpServers": [],
        "_meta": { "zcode": zcode },
    })
}

/// The `_meta.zcode.sessionClosed` flag on a `session/load` answer: the
/// bridge retired this conversation on a remote close and — because the load
/// declared `auto` intent — replayed the history without re-arming
/// discovery. The caller retires its tab locally and does not retry
/// (REMOTE-CLIENTS "Closing a session").
pub fn load_result_session_closed(result: &Value) -> bool {
    result
        .pointer("/_meta/zcode/sessionClosed")
        .and_then(Value::as_bool)
        == Some(true)
}

/// The attach response's `replayMeta` (bridge additive; src/store/replay.ts
/// `ReplayMeta`). Cursor fields are absent on older bridges.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReplayMeta {
    pub cursor: Option<String>,
    pub has_more: Option<bool>,
    pub total_messages: Option<u64>,
    /// Id of the newest message at the slice's end anchor — the preferred
    /// reconcile key over `totalMessages` (which drifts stale after turns
    /// watched live).
    pub last_message_id: Option<String>,
    /// A turn is still in flight for this session (someone else's prompt, or
    /// one that survived our reconnect).
    pub turn_active: Option<bool>,
}

pub fn replay_meta(result: &Value) -> Option<ReplayMeta> {
    let meta = result.get("replayMeta")?;
    let read = |key: &str| meta.get(key);
    Some(ReplayMeta {
        cursor: read("cursor").and_then(Value::as_str).map(str::to_string),
        has_more: read("hasMore").and_then(Value::as_bool),
        total_messages: read("totalMessages").and_then(Value::as_u64),
        last_message_id: read("lastMessageId")
            .and_then(Value::as_str)
            .map(str::to_string),
        turn_active: read("turnActive").and_then(Value::as_bool),
    })
}

/// The session facts both `session/new` and `session/load` answers carry
/// (src/store/appStore.ts `createProjectSession` / `loadSession`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionAttachInfo {
    pub session_id: Option<String>,
    pub config_options: Vec<ConfigOption>,
    pub current_mode_id: Option<String>,
    pub replay: Option<ReplayMeta>,
}

pub fn parse_session_new_result(result: &Value) -> SessionAttachInfo {
    SessionAttachInfo {
        session_id: result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string),
        config_options: parse_config_options(result),
        current_mode_id: result
            .pointer("/modes/currentModeId")
            .and_then(Value::as_str)
            .map(str::to_string),
        replay: None, // session/new has no replay
    }
}

pub fn parse_session_load_result(result: &Value) -> SessionAttachInfo {
    SessionAttachInfo {
        session_id: result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string),
        config_options: parse_config_options(result),
        current_mode_id: result
            .pointer("/modes/currentModeId")
            .and_then(Value::as_str)
            .map(str::to_string),
        replay: replay_meta(result),
    }
}

fn parse_config_options(result: &Value) -> Vec<ConfigOption> {
    result
        .get("configOptions")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(|r| serde_json::from_value(r.clone()).ok()).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
