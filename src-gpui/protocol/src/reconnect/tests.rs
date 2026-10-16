//! Port of `tests/reconnect-wedge.test.ts` (vitest) — regression tests for
//! the "stuck on 重连中 after wake" wedge, against the supervisor state
//! machine (the connection slice of `src/store/appStore.ts`).
//!
//! The bug: a client waking from deep sleep can park its hub fetch on a
//! socket that never delivers a byte. The reconnect timer's first move is an
//! await of exactly such a fetch, the timer that called it had already fired
//! (leaving a stale handle), so the banner stuck on "reconnecting" until the
//! user exited and re-entered from the list. The fix has two layers, each
//! covered here: a header-level deadline on every hub fetch (turns the hang
//! into a network error the loop already knows how to retry), and a watchdog
//! that tears down any reconnect attempt that outlives every legitimate
//! budget (defense against unbounded awaits the deadline cannot reach).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::json;

use super::{ConnState, Supervisor};
use crate::clock::{BoxFuture, MockClock};
use crate::http::{AbortSignal, HttpError, HttpRequest, HttpResponse, HttpTransport};
use crate::hub::ConnectionProfile;
use crate::ws::fake::FakeWsFactory;

// -- discovery fetch double ----------------------------------------------------
//
// `hang` = never answers but honors the abort signal (what the 10s deadline
// converts into a network error); `zombie` = never answers and IGNORES the
// abort (the shape the watchdog exists for — any await that cannot be
// reached by a timeout).

#[derive(Default)]
struct KnobHttp {
    instances_hang: AtomicBool,
    instances_zombie: AtomicBool,
    /// Answer discovery with an EMPTY list (the instance is gone from the
    /// hub's view).
    instances_gone: AtomicBool,
    probe_calls: AtomicU32,
    requested: Mutex<Vec<String>>,
}

impl KnobHttp {
    fn ok_json(body: serde_json::Value) -> Result<HttpResponse, HttpError> {
        Ok(HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.to_string(),
        })
    }
}

impl HttpTransport for KnobHttp {
    fn fetch(
        &self,
        req: HttpRequest,
        abort: AbortSignal,
    ) -> BoxFuture<'static, Result<HttpResponse, HttpError>> {
        let url = req.url;
        self.requested.lock().unwrap().push(url.clone());
        if url.contains("/api/instances") {
            if url.contains("probe=1") {
                self.probe_calls.fetch_add(1, Ordering::SeqCst);
            }
            if self.instances_zombie.load(Ordering::SeqCst) {
                // Never settles, ignores the abort — the wedge itself.
                return Box::pin(std::future::pending());
            }
            if self.instances_hang.load(Ordering::SeqCst) {
                return Box::pin(async move {
                    abort.wait().await;
                    Err(HttpError::aborted())
                });
            }
            let _ = abort;
            let gone = self.instances_gone.load(Ordering::SeqCst);
            return Box::pin(async move {
                if gone {
                    Self::ok_json(json!([]))
                } else {
                    Self::ok_json(json!([{
                        "id": "inst1",
                        "workspace": "/w",
                        "sessions": [{ "sessionId": "sess_1", "updatedAt": 1 }],
                    }]))
                }
            });
        }
        let _ = abort;
        Box::pin(async move { Self::ok_json(json!({})) })
    }
}

struct Env {
    clock: Arc<MockClock>,
    http: Arc<KnobHttp>,
    factory: Arc<FakeWsFactory>,
    sup: Supervisor,
}

/// The reconnect-loop WS double: OPEN from birth, answers initialize,
/// records every sent frame, dies on demand. Everything else (session/load,
/// $/zcode/ping) goes unanswered — an unanswered ping is exactly the zombie
/// probe case.
fn env() -> Env {
    let clock = Arc::new(MockClock::new());
    let http = Arc::new(KnobHttp::default());
    let factory = FakeWsFactory::new(true, true);
    let sup = Supervisor::new(
        http.clone(),
        factory.clone(),
        clock.clone(),
        None,
    );
    Env { clock, http, factory, sup }
}

async fn fresh_session(e: &Env) {
    e.sup.disconnect_hub();
    e.sup
        .connect_to_hub(ConnectionProfile { hub_url: "http://hub".into(), token: "t".into() })
        .await;
    assert_eq!(e.sup.state().instances.len(), 1);
    e.sup.connect_instance("inst1", None, false).await;
    wait_until(e, |s| {
        s.conn_state == ConnState::Open && s.active_session_id.is_some()
    })
    .await;
}

async fn wait_until(e: &Env, cond: impl Fn(&super::SupervisorState) -> bool) {
    for _ in 0..200 {
        if cond(&e.sup.state()) {
            return;
        }
        e.clock.advance(1).await;
    }
    panic!("condition not reached; state: {:?}", e.sup.state());
}

// -- ports -------------------------------------------------------------------

#[tokio::test]
async fn the_reconnect_loop_survives_a_hung_discovery_fetch_the_wake_wedge() {
    let e = env();
    fresh_session(&e).await;
    e.http.instances_hang.store(true, Ordering::SeqCst);

    let ws = e.factory.current();
    ws.die();
    e.clock.advance(0).await; // close event processed → scheduleReconnect
    assert_eq!(e.sup.state().conn_state, ConnState::Reconnecting);
    e.clock.advance(1_000).await; // reconnect timer → tryReconnect → fetch hangs
    assert_eq!(e.sup.state().conn_state, ConnState::Reconnecting);
    // Pre-fix behavior: nothing else ever happens. Now the deadline fires…
    e.clock.advance(10_000).await;
    // …the abort becomes a network error, discovery keeps the stale list, and
    // the loop proceeds to a fresh connection attempt.
    e.clock.advance(1_000).await;
    assert_eq!(e.sup.state().conn_state, ConnState::Open);
}

#[tokio::test]
async fn the_watchdog_tears_down_a_try_reconnect_wedged_past_every_budget() {
    let e = env();
    fresh_session(&e).await;

    e.http.instances_zombie.store(true, Ordering::SeqCst);
    e.http.probe_calls.store(0, Ordering::SeqCst);
    let ws = e.factory.current();
    ws.die();
    e.clock.advance(0).await;
    e.clock.advance(1_000).await; // tryReconnect starts; the await hangs
    assert_eq!(e.sup.state().conn_state, ConnState::Reconnecting);
    e.clock.advance(60_000).await; // far past fetch + WS deadlines
    // The wedge: the fired timer left a stale handle, and the zombie await
    // ignores the fetch deadline. Exactly one probe call ever happened.
    assert_eq!(e.http.probe_calls.load(Ordering::SeqCst), 1);
    // The periodic probe (20s tick in the app; called directly here) sees an
    // attempt that outlived every legitimate budget and forces a teardown.
    e.sup.wake_probe().await;
    e.clock.advance(0).await;
    assert_eq!(e.sup.state().conn_state, ConnState::Reconnecting);
    e.http.instances_zombie.store(false, Ordering::SeqCst);
    e.clock.advance(1_000).await; // fresh-backoff retry fires
    assert_eq!(e.http.probe_calls.load(Ordering::SeqCst), 2);
    e.clock.advance(2_000).await;
    assert_eq!(e.sup.state().conn_state, ConnState::Open);
}

#[tokio::test]
async fn streaming_traffic_suppresses_the_open_state_liveness_probe() {
    let e = env();
    fresh_session(&e).await;
    let ws = e.factory.current();
    let sid = e.sup.state().active_session_id.expect("attached session");

    // A live turn is streaming: wire activity proves the pipe.
    ws.server(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": sid,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "messageId": "msg_1",
                "content": { "type": "text", "text": "still streaming " },
            },
        },
    }));
    e.clock.advance(0).await;

    let sup = e.sup.clone();
    tokio::spawn(async move { sup.wake_probe().await });
    e.clock.advance(0).await;
    assert_eq!(e.sup.state().conn_state, ConnState::Open);
    assert_eq!(ws.sent_frames("$/zcode/ping").len(), 0);

    // After 30s of silence the probe runs; an unanswered ping (5s budget)
    // tears the zombie down so the reconnect loop takes over.
    e.clock.advance(31_000).await;
    let sup = e.sup.clone();
    tokio::spawn(async move { sup.wake_probe().await });
    e.clock.advance(0).await;
    assert_eq!(ws.sent_frames("$/zcode/ping").len(), 1);
    e.clock.advance(5_000).await;
    assert_eq!(e.sup.state().conn_state, ConnState::Reconnecting);
}

// -- extra coverage for branches the wedge suite touches only in passing ------

/// Hub offline for DISCOVERY_FAIL_THRESHOLD consecutive polls: the first
/// failures redial the WS in place (instance liveness unknown, stale list
/// kept); once the threshold trips, the loop keeps retrying in place
/// (Reconnecting) and never bounces the session out.
#[tokio::test]
async fn hub_unreachable_keeps_retrying_in_place() {
    let e = env();
    fresh_session(&e).await;
    e.http.instances_hang.store(true, Ordering::SeqCst);

    // Cycles 1 and 2: the fetch deadline turns the hung discovery into a
    // network error; the instance list is unknown (not gone) so the loop
    // retries the WS in place and recovers — only the failure count grows.
    for _ in 0..2 {
        e.factory.current().die();
        e.clock.advance(0).await; // close → scheduleReconnect
        assert_eq!(e.sup.state().conn_state, ConnState::Reconnecting);
        e.clock.advance(1_000).await; // timer → tryReconnect → fetch hangs
        e.clock.advance(10_000).await; // deadline → network error → redial
        wait_until(&e, |s| s.conn_state == ConnState::Open).await;
    }

    // Cycle 3: the threshold (3 consecutive network failures) trips — the
    // list blanks, the hub reads offline, and the loop retries IN PLACE
    // instead of proceeding or resetting to Idle.
    e.factory.current().die();
    e.clock.advance(0).await;
    e.clock.advance(1_000).await;
    e.clock.advance(10_000).await;
    let state = e.sup.state();
    assert_eq!(state.conn_state, ConnState::Reconnecting);
    assert!(state.hub_offline, "three consecutive network failures flag the hub offline");
    // The instance target survives: the session is never bounced out.
    assert_eq!(state.instance_id.as_deref(), Some("inst1"));
}

/// A successful listing that lacks the instance proves it gone: reset to
/// Idle with the instance/session cleared ("notice.instanceGone").
#[tokio::test]
async fn a_listing_without_the_instance_resets_to_idle() {
    let e = env();
    fresh_session(&e).await;
    e.http.instances_gone.store(true, Ordering::SeqCst);
    let ws = e.factory.current();
    ws.die();
    e.clock.advance(0).await; // close → scheduleReconnect
    e.clock.advance(1_000).await; // tryReconnect → discovery ok but empty → gone
    let state = e.sup.state();
    assert_eq!(state.conn_state, ConnState::Idle);
    assert_eq!(state.instance_id, None);
    assert_eq!(state.active_session_id, None);
}
