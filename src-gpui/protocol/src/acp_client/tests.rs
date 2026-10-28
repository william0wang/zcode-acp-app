//! Port of `tests/acp-connection.test.ts` (vitest) — transport-layer
//! coverage: the connect deadline (zombie sockets never fire WS events),
//! pending-request rejection on close, and the JSON-RPC routing paths
//! (server requests, cancel, update meta). The fake WS factory plays the
//! role of the test's `FakeWebSocket` global.

use std::future::Future;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::{
    load_result_session_closed, parse_session_load_result, parse_session_new_result,
    replay_meta, session_load_params, session_new_params, AcpConnection, AcpConnectionState,
    AcpHandlers, AcpInitializeResult, ConnectError, DESKTOP_CLIENT_NAME, REPLAY_TAIL_LIMIT,
};
use crate::clock::MockClock;
use crate::session::LoadIntent;
use crate::ws::fake::{FakeWs, FakeWsFactory};

// -- handler capture (the vi.fn() doubles) ------------------------------------

#[derive(Default)]
struct CaptureHandlers {
    states: Mutex<Vec<AcpConnectionState>>,
    updates: Mutex<Vec<(String, crate::acp::SessionUpdate, Option<Value>)>>,
    server_requests: Mutex<Vec<(super::ServerRequest, super::ServerResponder)>>,
    cancels: Mutex<Vec<u64>>,
    turns: Mutex<Vec<(String, bool, Option<super::TurnStateInfo>)>>,
    workflow_list_changes: Mutex<u32>,
    session_closes: Mutex<Vec<String>>,
}

impl AcpHandlers for CaptureHandlers {
    fn on_state(&self, state: AcpConnectionState) {
        self.states.lock().unwrap().push(state);
    }

    fn on_update(&self, session_id: &str, update: &crate::acp::SessionUpdate, meta: Option<&Value>) {
        self.updates
            .lock()
            .unwrap()
            .push((session_id.to_string(), update.clone(), meta.cloned()));
    }

    fn on_server_request(&self, req: super::ServerRequest, respond: super::ServerResponder) {
        self.server_requests.lock().unwrap().push((req, respond));
    }

    fn on_cancel_request(&self, id: u64) {
        self.cancels.lock().unwrap().push(id);
    }

    fn on_turn_state(&self, session_id: &str, running: bool, turn: Option<super::TurnStateInfo>) {
        self.turns
            .lock()
            .unwrap()
            .push((session_id.to_string(), running, turn));
    }

    fn on_workflow_list_changed(&self) {
        *self.workflow_list_changes.lock().unwrap() += 1;
    }

    fn on_session_closed(&self, session_id: &str) {
        self.session_closes.lock().unwrap().push(session_id.to_string());
    }
}

/// Poll a connect future once so the factory runs and the deadline sleep
/// registers, without waiting for it to settle.
async fn prime(
    fut: &mut (impl Future<Output = Result<(AcpConnection, AcpInitializeResult), ConnectError>> + Unpin),
) {
    tokio::select! {
        biased;
        result = fut => panic!("connect settled before open: {result:?}"),
        _ = tokio::task::yield_now() => {}
    }
}

/// Drive a mid-handshake connect future forward alongside the (spawned)
/// pump task, so the initialize frame is actually sent. Unlike a spawned
/// task, a locally-held future is only polled at await points — advance(0)
/// alone would leave it parked forever.
async fn nudge(
    fut: &mut (impl Future<Output = Result<(AcpConnection, AcpInitializeResult), ConnectError>> + Unpin),
) {
    for _ in 0..8 {
        tokio::select! {
            biased;
            result = &mut *fut => panic!("connect settled early: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
    }
}

/// Runs connect() and completes the initialize handshake — the suite's
/// `opened()` helper.
async fn opened(
    handlers: Arc<CaptureHandlers>,
) -> (AcpConnection, Arc<FakeWsFactory>, Arc<MockClock>, Arc<FakeWs>) {
    let factory = FakeWsFactory::new(false, false);
    let clock = Arc::new(MockClock::new());
    let mut fut = Box::pin(AcpConnection::connect(
        "http://hub",
        "tok",
        "inst",
        handlers,
        factory.clone(),
        clock.clone(),
    ));
    prime(&mut fut).await;
    let ws = factory.current();
    ws.emit_open();
    nudge(&mut fut).await; // pump routes Open; initialize goes out
    let init_id = ws.sent.lock().unwrap()[0].get("id").cloned().unwrap();
    ws.server(json!({ "jsonrpc": "2.0", "id": init_id, "result": { "protocolVersion": 1, "agentCapabilities": {} } }));
    let (conn, _init) = fut.await.expect("connect");
    (conn, factory, clock, ws)
}

// -- ports -------------------------------------------------------------------

#[tokio::test]
async fn connect_resolves_once_initialize_answers_and_reports_state_open() {
    let handlers = Arc::new(CaptureHandlers::default());
    let factory = FakeWsFactory::new(false, false);
    let clock = Arc::new(MockClock::new());
    let mut fut = Box::pin(AcpConnection::connect(
        "http://hub",
        "tok",
        "inst",
        handlers.clone(),
        factory.clone(),
        clock.clone(),
    ));
    prime(&mut fut).await;
    let ws = factory.current();
    ws.emit_open();
    nudge(&mut fut).await;

    let frames = ws.sent.lock().unwrap().clone();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["method"], "initialize");
    assert_eq!(frames[0]["params"]["protocolVersion"], 1);
    // Plan approval / AskUserQuestion route through elicitation/create only
    // for clients that declare the form capability — assert we do.
    assert_eq!(frames[0]["params"]["clientCapabilities"]["elicitation"]["form"], true);
    // Desktop Presence: the clientInfo identity the hub peeks off this
    // first frame (server DESKTOP_CLIENT_NAME).
    assert_eq!(frames[0]["params"]["clientInfo"]["name"], DESKTOP_CLIENT_NAME);
    assert!(frames[0]["params"]["clientInfo"]["version"].is_string());

    ws.server(json!({
        "jsonrpc": "2.0",
        "id": frames[0]["id"],
        "result": { "protocolVersion": 1, "agentCapabilities": {} },
    }));
    let (conn, init) = fut.await.expect("connect");
    assert_eq!(init.protocol_version(), Some(1));
    assert!(conn.is_open());
    assert!(handlers.states.lock().unwrap().contains(&AcpConnectionState::Open));
}

#[tokio::test]
async fn connect_times_out_when_the_socket_never_opens_zombie() {
    let handlers = Arc::new(CaptureHandlers::default());
    let factory = FakeWsFactory::new(false, false);
    let clock = Arc::new(MockClock::new());
    let mut fut = Box::pin(AcpConnection::connect(
        "http://hub",
        "tok",
        "inst",
        handlers.clone(),
        factory.clone(),
        clock.clone(),
    ));
    prime(&mut fut).await;
    let ws = factory.current();
    // No open ever arrives — the deadline is the only way out.
    clock.advance(super::CONNECT_TIMEOUT_MS).await;
    let err = fut.await.unwrap_err();
    assert!(matches!(err, ConnectError::Timeout), "unexpected error: {err:?}");
    assert!(ws.closed.load(std::sync::atomic::Ordering::SeqCst));
    assert!(!handlers.states.lock().unwrap().contains(&AcpConnectionState::Open));
}

#[tokio::test]
async fn requests_reject_with_connection_closed_when_we_close() {
    let (conn, _factory, _clock, _ws) = opened(Arc::new(CaptureHandlers::default())).await;
    let pending = conn.request("session/prompt", json!({ "sessionId": "s" }));
    conn.close();
    let err = pending.await.unwrap_err();
    assert_eq!(err.0, "connection closed");
}

#[tokio::test]
async fn peer_side_close_rejects_pending_and_reports_state_closed() {
    let handlers = Arc::new(CaptureHandlers::default());
    let (conn, _factory, _clock, ws) = opened(handlers.clone()).await;
    let pending = conn.request("session/prompt", json!({ "sessionId": "s" }));
    // Peer-side close: we did not close it, so state "closed" must surface.
    ws.die();
    let err = pending.await.unwrap_err();
    assert_eq!(err.0, "connection closed");
    assert!(handlers.states.lock().unwrap().contains(&AcpConnectionState::Closed));
}

#[tokio::test]
async fn requests_reject_with_connection_not_open_before_open() {
    let (conn, _factory, _clock, _ws) = opened(Arc::new(CaptureHandlers::default())).await;
    conn.close();
    let err = conn.request("session/prompt", json!({})).await.unwrap_err();
    assert_eq!(err.0, "connection not open");
}

#[tokio::test]
async fn server_requests_route_to_the_handler_and_the_answer_carries_the_id() {
    let handlers = Arc::new(CaptureHandlers::default());
    let (_conn, _factory, clock, ws) = opened(handlers.clone()).await;
    ws.server(json!({
        "jsonrpc": "2.0",
        "id": 42,
        "method": "session/request_permission",
        "params": { "sessionId": "s", "options": [] },
    }));
    clock.advance(0).await; // let the pump route it
    let requests = handlers.server_requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0.method, "session/request_permission");
    let respond = requests[0].1.clone();
    drop(requests);
    respond.send(json!({ "outcome": { "outcome": "selected", "optionId": "once" } }));
    let answer = ws
        .sent
        .lock()
        .unwrap()
        .iter()
        .find(|f| f.get("id") == Some(&json!(42)))
        .cloned()
        .expect("answer frame");
    assert_eq!(
        answer["result"],
        json!({ "outcome": { "outcome": "selected", "optionId": "once" } })
    );
}

#[tokio::test]
async fn cancel_request_routes_to_on_cancel_request() {
    let handlers = Arc::new(CaptureHandlers::default());
    let (_conn, _factory, clock, ws) = opened(handlers.clone()).await;
    ws.server(json!({ "jsonrpc": "2.0", "method": "$/cancel_request", "params": { "id": 9 } }));
    clock.advance(0).await;
    assert_eq!(*handlers.cancels.lock().unwrap(), vec![9]);
}

#[tokio::test]
async fn session_update_forwards_the_update_and_the_notification_level_meta() {
    let handlers = Arc::new(CaptureHandlers::default());
    let (_conn, _factory, clock, ws) = opened(handlers.clone()).await;
    ws.server(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": "s",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "x" },
            },
        },
        "_meta": { "zcode": { "collapsed": true } },
    }));
    clock.advance(0).await;
    let updates = handlers.updates.lock().unwrap();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].0, "s");
    assert_eq!(updates[0].1.session_update, "agent_message_chunk");
    assert_eq!(updates[0].2.as_ref(), Some(&json!({ "zcode": { "collapsed": true } })));
}

// -- bridge extensions (not in the TS suite; same routing layer) -------------

#[tokio::test]
async fn turn_state_routes_with_the_optional_turn_block() {
    let handlers = Arc::new(CaptureHandlers::default());
    let (_conn, _factory, clock, ws) = opened(handlers.clone()).await;
    // END transition carrying usage + wall time.
    ws.server(json!({
        "jsonrpc": "2.0",
        "method": "$/zcode/turnState",
        "params": { "sessionId": "s", "running": false, "usage": { "inputTokens": 5 }, "turnMs": 2400 },
    }));
    // START carries neither — no turn block.
    ws.server(json!({
        "jsonrpc": "2.0",
        "method": "$/zcode/turnState",
        "params": { "sessionId": "s", "running": true },
    }));
    clock.advance(0).await;
    let turns = handlers.turns.lock().unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].0, "s");
    assert!(!turns[0].1);
    let turn = turns[0].2.clone().expect("turn block on END");
    assert_eq!(turn.usage, Some(json!({ "inputTokens": 5 })));
    assert_eq!(turn.turn_ms, Some(2400.0));
    assert!(turns[1].2.is_none());
}

#[tokio::test]
async fn workflow_list_changed_routes() {
    let handlers = Arc::new(CaptureHandlers::default());
    let (_conn, _factory, clock, ws) = opened(handlers.clone()).await;
    ws.server(json!({ "jsonrpc": "2.0", "method": "$/zcode/workflowListChanged" }));
    clock.advance(0).await;
    assert_eq!(*handlers.workflow_list_changes.lock().unwrap(), 1);
}

#[tokio::test]
async fn session_closed_routes_by_payload_session_id() {
    let handlers = Arc::new(CaptureHandlers::default());
    let (_conn, _factory, clock, ws) = opened(handlers.clone()).await;
    ws.server(json!({
        "jsonrpc": "2.0",
        "method": "$/zcode/session_closed",
        "params": { "sessionId": "s" },
    }));
    // A copy without the id drops instead of misrouting.
    ws.server(json!({ "jsonrpc": "2.0", "method": "$/zcode/session_closed", "params": {} }));
    clock.advance(0).await;
    assert_eq!(
        *handlers.session_closes.lock().unwrap(),
        vec!["s".to_string()]
    );
}

// -- session/new & session/load wire shapes ------------------------------------

#[test]
fn session_new_params_shape() {
    let params = session_new_params("/w/proj");
    assert_eq!(params, json!({ "cwd": "/w/proj", "mcpServers": [] }));
}

#[test]
fn session_load_params_shape() {
    // User intent is the wire default: no key at all (the old shape an old
    // bridge expects).
    let params = session_load_params("sess_1", "/w", REPLAY_TAIL_LIMIT, LoadIntent::User);
    assert_eq!(
        params,
        json!({
            "sessionId": "sess_1",
            "cwd": "/w",
            "mcpServers": [],
            "_meta": { "zcode": { "limit": 30 } },
        })
    );
    // Metadata-only attach reconcile (limit 0) keeps the same shape.
    assert_eq!(
        session_load_params("sess_1", "/", 0, LoadIntent::User)["_meta"]["zcode"]["limit"],
        json!(0)
    );
    // Auto intent rides alongside the limit (the reconnect re-attach).
    let auto = session_load_params("sess_1", "/w", REPLAY_TAIL_LIMIT, LoadIntent::Auto);
    assert_eq!(
        auto["_meta"]["zcode"],
        json!({ "limit": 30, "intent": "auto" })
    );
}

#[test]
fn load_result_session_closed_flag() {
    // The disclosure (REMOTE-CLIENTS "Closing a session"): an AUTO load of a
    // bridge-retired conversation replays history but answers
    // `_meta.zcode.sessionClosed: true` — retire the tab, do not retry.
    assert!(load_result_session_closed(&json!({
        "replayMeta": { "cursor": "c", "hasMore": false, "totalMessages": 3 },
        "_meta": { "zcode": { "sessionClosed": true } },
    })));
    // Absent (live conversation, or a user load — which re-arms) and
    // misspelled/false copies read as open.
    assert!(!load_result_session_closed(&json!({
        "replayMeta": { "cursor": "c" },
    })));
    assert!(!load_result_session_closed(&json!({
        "_meta": { "zcode": { "sessionClosed": false } },
    })));
    assert!(!load_result_session_closed(&json!({
        "_meta": { "zcode": { "sessionclosed": true } },
    })));
}

#[test]
fn replay_meta_and_attach_parsing() {
    let result = json!({
        "sessionId": "sess_1",
        "configOptions": [{ "id": "model" }],
        "modes": { "currentModeId": "plan" },
        "replayMeta": {
            "cursor": "cur-9",
            "hasMore": true,
            "totalMessages": 120,
            "lastMessageId": "m-last",
            "turnActive": true,
        },
    });
    let meta = replay_meta(&result).expect("replayMeta");
    assert_eq!(meta.cursor.as_deref(), Some("cur-9"));
    assert_eq!(meta.has_more, Some(true));
    assert_eq!(meta.total_messages, Some(120));
    assert_eq!(meta.last_message_id.as_deref(), Some("m-last"));
    assert_eq!(meta.turn_active, Some(true));

    let attach = parse_session_load_result(&result);
    assert_eq!(attach.current_mode_id.as_deref(), Some("plan"));
    assert_eq!(attach.config_options.len(), 1);
    assert!(attach.replay.is_some());

    // session/new carries no replayMeta.
    let created = parse_session_new_result(&json!({
        "sessionId": "sess_2",
        "configOptions": [],
    }));
    assert_eq!(created.session_id.as_deref(), Some("sess_2"));
    assert!(created.replay.is_none());

    // Absent replayMeta decodes as None (older bridges).
    assert!(replay_meta(&json!({})).is_none());
}
