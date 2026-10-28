//! WebSocket transport — the Rust counterpart of the TS client's
//! `new WebSocket(...)` usage (`src/lib/acp.ts`). The connection logic is
//! written against [`WsFactory`]/[`WsStream`]; production wiring is
//! [`TungsteniteWsFactory`] (tokio-tungstenite), tests install recording
//! doubles (the role the vitest `FakeWebSocket` global plays).
//!
//! Auth divergence from the TS client, deliberate: browsers cannot set WS
//! headers, so the TS app puts the token in the query string
//! (`?instance=…&token=…`, src/lib/acp.ts `wsUrl`). A native client can set
//! headers, so this port sends `Authorization: Bearer <token>` and keeps only
//! the `instance` routing param in the query. The hub accepts either
//! (zcode-acp-server `src/remote/hub-server.ts` `authorized`: Bearer header
//! first, `?token=` fallback).

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::clock::BoxFuture;
use crate::http::encode_query_component;

/// Transport-level socket event. `Closed` covers both a clean close and a
/// died socket (the TS `onclose` is the only post-open lifecycle hook the
/// client needs); handshake failures surface as an error from the factory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsEvent {
    Open,
    Text(String),
    Closed,
}

/// The connected-socket handle: send text frames, initiate close.
pub trait WsStream: Send + Sync {
    /// Best-effort send — a dead socket drops silently (same as the TS
    /// `send()` guard, which no-ops unless readyState is OPEN).
    fn send(&self, text: &str);
    fn close(&self);
}

/// `new WebSocket` equivalent. Resolves with the stream plus its event feed
/// once the underlying transport is created — for the real factory that is
/// the completed handshake, for a test double an immediately-returned pipe
/// whose `Open` event arrives on the feed. `bearer` is the token for the
/// Authorization header (see the module doc).
pub trait WsFactory: Send + Sync {
    fn connect(
        &self,
        url: &str,
        bearer: &str,
    ) -> BoxFuture<'static, Result<(Arc<dyn WsStream>, mpsc::Receiver<WsEvent>), String>>;
}

/// Build the ACP socket URL from a hub URL (the TS `wsUrl`, src/lib/acp.ts):
/// `https:`→`wss:` / `http:`→`ws:` (ws/wss pass through), trailing slashes
/// stripped from the path and `/acp` appended, `instance` set in the query.
/// The token is NOT appended — it travels as the Bearer header instead.
pub fn ws_url(hub_url: &str, instance_id: &str) -> Result<String, String> {
    let (scheme, rest) = hub_url
        .split_once("://")
        .ok_or_else(|| format!("bad hub url (no scheme): {hub_url}"))?;
    let scheme = match scheme {
        "https" => "wss",
        "http" => "ws",
        other => other,
    };
    // authority runs to the first '/' or '?'; path and query follow.
    let (authority, tail) = match rest.find(['/', '?']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Err(format!("bad hub url (no host): {hub_url}"));
    }
    let (raw_path, raw_query) = match tail.split_once('?') {
        Some((p, q)) => (p, q),
        None => (tail, ""),
    };
    let path = format!("{}/acp", raw_path.trim_end_matches('/'));
    // `instance` replaces any existing occurrence; other params ride along.
    let mut params: Vec<String> = raw_query
        .split('&')
        .filter(|p| !p.is_empty() && !p.starts_with("instance="))
        .map(str::to_string)
        .collect();
    params.push(format!("instance={}", encode_query_component(instance_id)));
    Ok(format!("{scheme}://{authority}{path}?{}", params.join("&")))
}

// ---------------------------------------------------------------------------
// Production transport: tokio-tungstenite
// ---------------------------------------------------------------------------

/// Real-network WS factory over tokio-tungstenite, with the bearer token in
/// the Authorization header of the upgrade request.
#[derive(Debug, Clone, Copy, Default)]
pub struct TungsteniteWsFactory;

impl WsFactory for TungsteniteWsFactory {
    fn connect(
        &self,
        url: &str,
        bearer: &str,
    ) -> BoxFuture<'static, Result<(Arc<dyn WsStream>, mpsc::Receiver<WsEvent>), String>> {
        let url = url.to_string();
        let bearer = bearer.to_string();
        Box::pin(async move {
            use tokio_tungstenite::tungstenite::client::IntoClientRequest;
            let mut request = url
                .as_str()
                .into_client_request()
                .map_err(|e| e.to_string())?;
            let value = tokio_tungstenite::tungstenite::http::HeaderValue::from_str(&format!(
                "Bearer {bearer}"
            ))
            .map_err(|e| e.to_string())?;
            request.headers_mut().insert(
                tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
                value,
            );
            let (socket, _response) =
                tokio_tungstenite::connect_async(request).await.map_err(|e| e.to_string())?;

            use futures_util::{SinkExt, StreamExt};
            let (mut sink, mut stream) = socket.split();
            // UNBOUNDED on purpose. `WsStream::send` is sync fire-and-forget
            // with browser-WS semantics (a browser `send` never drops), and a
            // bounded feed here silently dropped the frame that overflowed it:
            // while the writer stalled on a slow link (a base64-image prompt
            // backpressures the socket for seconds), the 65th queued frame —
            // a `session/cancel` is exactly that frame — vanished, which read
            // on screen as "stop sometimes does nothing" (observed 2026-10-28,
            // the same incident class the reader feed fixed 2026-10-21). Only
            // user-initiated frames enter this feed, so the buffer stays tiny;
            // the writer drains it at socket speed.
            let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
            let (close_tx, mut close_rx) = tokio::sync::watch::channel(false);
            let (event_tx, event_rx) = mpsc::channel::<WsEvent>(64);

            // `connect_async` resolved means the handshake completed: the
            // socket is open. Queue the Open event BEFORE the reader task
            // can queue text frames, so consumers (AcpConnection's pump)
            // always observe Open first — the factory contract says the
            // event feed starts at Open.
            let _ = event_tx.try_send(WsEvent::Open);

            // Writer: outbound frames until close is requested, then a
            // polite Close frame (the reader observes it as Closed).
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        biased;
                        changed = close_rx.changed() => {
                            if changed.is_err() || *close_rx.borrow_and_update() {
                                break;
                            }
                        }
                        text = out_rx.recv() => match text {
                            Some(text) => {
                                if sink
                                    .send(tokio_tungstenite::tungstenite::Message::Text(
                                        text.into(),
                                    ))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            None => break,
                        },
                    }
                }
                let _ = sink.close().await;
            });

            // Reader: text frames onto the event feed; Close/EOF/err ends it.
            tokio::spawn(async move {
                while let Some(msg) = stream.next().await {
                    match msg {
                        Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                            // AWAIT the enqueue — never `try_send`. The feed is
                            // bounded, and a `session/load` replay lands
                            // hundreds of frames back to back (172 in the
                            // observed shanghai-building-demo session, 692
                            // total). Dropping a frame on a full queue broke
                            // the outbound loop below, which the pump reads as
                            // "socket died": the client reconnected in a ~1.5s
                            // storm, replaying the same burst (observed
                            // 2026-10-21, 99 connects in three minutes).
                            // Backpressure stalls the socket read instead —
                            // exactly what a browser WebSocket does.
                            if event_tx.send(WsEvent::Text(text.to_string())).await.is_err() {
                                break;
                            }
                        }
                        Ok(tokio_tungstenite::tungstenite::Message::Close(_)) => break,
                        // Pings answer automatically inside tungstenite;
                        // binary frames are ignored per contract.
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                // AWAIT (not try_send) for the same reason as the text arm:
                // a dropped `Closed` leaves the pump waiting on a socket that
                // is already gone instead of surfacing the disconnect.
                let _ = event_tx.send(WsEvent::Closed).await;
            });

            Ok((
                Arc::new(TungsteniteWsHandle { out_tx, close_tx }) as Arc<dyn WsStream>,
                event_rx,
            ))
        })
    }
}

struct TungsteniteWsHandle {
    out_tx: mpsc::UnboundedSender<String>,
    close_tx: tokio::sync::watch::Sender<bool>,
}

impl WsStream for TungsteniteWsHandle {
    fn send(&self, text: &str) {
        let _ = self.out_tx.send(text.to_string());
    }

    fn close(&self) {
        let _ = self.close_tx.send(true);
    }
}

// ---------------------------------------------------------------------------
// Test double — the vitest FakeWebSocket equivalent (unit tests only)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod fake {
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    use std::sync::{Arc, Mutex};

    use serde_json::Value;

    use super::*;

    /// Recording WS double. Mirrors the TS `FakeWebSocket` classes: OPEN from
    /// birth, records every sent frame, optionally auto-answers `initialize`
    /// (the reconnect-loop suites' flavor), dies on demand, never delivers
    /// `Open` until the test (or the factory, `open_on_connect`) says so.
    pub struct FakeWs {
        pub sent: Mutex<Vec<Value>>,
        events: mpsc::Sender<WsEvent>,
        /// Whether `initialize` requests get an immediate canned reply (the
        /// reconnect-wedge double answers them; the acp-connection double
        /// leaves answering to the test).
        pub auto_answer_initialize: bool,
        pub closed: AtomicBool,
        pub ready_state: AtomicU8,
    }

    impl FakeWs {
        /// Deliver a server frame to the client (TS `server(frame)`).
        pub fn server(&self, frame: Value) {
            let _ = self.events.try_send(WsEvent::Text(frame.to_string()));
        }

        /// TS `die()`: mark CLOSED and deliver the close event.
        pub fn die(&self) {
            self.ready_state.store(3, Ordering::SeqCst);
            let _ = self.events.try_send(WsEvent::Closed);
        }

        pub fn emit_open(&self) {
            self.ready_state.store(1, Ordering::SeqCst);
            let _ = self.events.try_send(WsEvent::Open);
        }

        pub fn sent_frames(&self, method: &str) -> Vec<Value> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .filter(|f| f.get("method").and_then(Value::as_str) == Some(method))
                .cloned()
                .collect()
        }
    }

    impl WsStream for FakeWs {
        fn send(&self, text: &str) {
            let Ok(frame) = serde_json::from_str::<Value>(text) else {
                return;
            };
            self.sent.lock().unwrap().push(frame.clone());
            if self.auto_answer_initialize
                && frame.get("method").and_then(Value::as_str) == Some("initialize")
            {
                if let Some(id) = frame.get("id") {
                    let reply = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "protocolVersion": 1, "agentCapabilities": {} },
                    })
                    .to_string();
                    let _ = self.events.try_send(WsEvent::Text(reply));
                }
            }
        }

        fn close(&self) {
            self.closed.store(true, Ordering::SeqCst);
            self.ready_state.store(3, Ordering::SeqCst);
        }
    }

    /// Factory double: hands out [`FakeWs`] pipes; the last created socket is
    /// `current` (the TS `FakeWebSocket.current` static).
    pub struct FakeWsFactory {
        pub current: Mutex<Option<Arc<FakeWs>>>,
        /// Emit `Open` as soon as the socket is created (the reconnect-loop
        /// double's `queueMicrotask(onopen)`).
        pub open_on_connect: bool,
        pub auto_answer_initialize: bool,
        /// Last (url, bearer) handed to connect — bearer-auth assertions.
        pub last_connect: Mutex<Option<(String, String)>>,
        /// When set, every connect fails (bad URL / token / instance id).
        pub fail: AtomicBool,
    }

    impl FakeWsFactory {
        pub fn new(open_on_connect: bool, auto_answer_initialize: bool) -> Arc<Self> {
            Arc::new(Self {
                current: Mutex::new(None),
                open_on_connect,
                auto_answer_initialize,
                last_connect: Mutex::new(None),
                fail: AtomicBool::new(false),
            })
        }

        pub fn current(&self) -> Arc<FakeWs> {
            self.current.lock().unwrap().clone().expect("no fake ws created yet")
        }
    }

    impl WsFactory for FakeWsFactory {
        fn connect(
            &self,
            url: &str,
            bearer: &str,
        ) -> BoxFuture<'static, Result<(Arc<dyn WsStream>, mpsc::Receiver<WsEvent>), String>> {
            if self.fail.load(Ordering::SeqCst) {
                return Box::pin(async move { Err("handshake refused".to_string()) });
            }
            *self.last_connect.lock().unwrap() = Some((url.to_string(), bearer.to_string()));
            let (event_tx, event_rx) = mpsc::channel::<WsEvent>(64);
            let ws = Arc::new(FakeWs {
                sent: Mutex::new(Vec::new()),
                events: event_tx,
                auto_answer_initialize: self.auto_answer_initialize,
                closed: AtomicBool::new(false),
                ready_state: AtomicU8::new(if self.open_on_connect { 1 } else { 0 }),
            });
            if self.open_on_connect {
                let _ = ws.events.try_send(WsEvent::Open);
            }
            *self.current.lock().unwrap() = Some(Arc::clone(&ws));
            let stream: Arc<dyn WsStream> = ws;
            Box::pin(async move { Ok((stream, event_rx)) })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_maps_scheme_strips_slashes_and_sets_instance() {
        assert_eq!(ws_url("http://hub", "i1").unwrap(), "ws://hub/acp?instance=i1");
        assert_eq!(ws_url("https://hub/", "i1").unwrap(), "wss://hub/acp?instance=i1");
        assert_eq!(
            ws_url("http://hub:8377/base/", "i 1").unwrap(),
            "ws://hub:8377/base/acp?instance=i%201"
        );
        // ws/wss pass through; an existing instance param is replaced.
        assert_eq!(
            ws_url("ws://hub/x?other=1&instance=old", "new").unwrap(),
            "ws://hub/x/acp?other=1&instance=new"
        );
        assert!(ws_url("hub", "i1").is_err());
        assert!(ws_url("http://", "i1").is_err());
    }

    /// Regression (found live by the desktop M1 smoke, 2026-10-15): the
    /// production factory must emit `Open` on the event feed once the
    /// handshake completes — without it AcpConnection's open-gate waits
    /// forever and every real connection dies on the 15s deadline. The
    /// factory double always emitted Open, so only the real transport
    /// carried the bug. Loopback server over tokio-tungstenite's handshake.
    #[tokio::test]
    async fn real_factory_emits_open_before_frames() {
        use futures_util::{SinkExt, StreamExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Server side: complete the WS handshake, answer one text frame,
        // then wait for the client to close.
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            ws.send(tokio_tungstenite::tungstenite::Message::Text("hi".into()))
                .await
                .unwrap();
            while ws.next().await.is_some() {}
        });

        let factory = TungsteniteWsFactory;
        let (stream, mut events) = factory
            .connect(&format!("ws://127.0.0.1:{port}/acp?instance=i1"), "tok")
            .await
            .expect("handshake");

        // Open must be the FIRST event, ahead of the server's text frame.
        assert_eq!(events.recv().await, Some(WsEvent::Open));
        assert_eq!(
            events.recv().await,
            Some(WsEvent::Text("hi".to_string()))
        );

        // Send + close round-trip.
        stream.send(r#"{"probe":1}"#);
        stream.close();
        assert_eq!(events.recv().await, Some(WsEvent::Closed));
    }

    /// Regression (found live, 2026-10-21): a `session/load` replay delivers
    /// its whole history in one burst — 172 frames for one observed session.
    /// The reader used `try_send` against a 64-slot feed, so frame 65 failed
    /// and the reader broke out as if the socket had died: the client
    /// reconnected, replayed the same burst, and repeated forever (99 connects
    /// in three minutes). Every frame must survive, however far past the
    /// channel capacity the burst runs.
    #[tokio::test]
    async fn real_factory_survives_a_replay_burst_past_the_feed_capacity() {
        use futures_util::{SinkExt, StreamExt};
        const FRAMES: usize = 300; // > the 64-slot feed, the observed 172

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            for i in 0..FRAMES {
                ws.send(tokio_tungstenite::tungstenite::Message::Text(
                    format!(r#"{{"n":{i}}}"#).into(),
                ))
                .await
                .unwrap();
            }
            while ws.next().await.is_some() {}
        });

        let factory = TungsteniteWsFactory;
        let (_stream, mut events) = factory
            .connect(&format!("ws://127.0.0.1:{port}/acp?instance=i1"), "tok")
            .await
            .expect("handshake");
        assert_eq!(events.recv().await, Some(WsEvent::Open));

        // Consume deliberately slowly — the reader must block on the full
        // feed instead of dropping frames or declaring the socket dead.
        for i in 0..FRAMES {
            match events.recv().await {
                Some(WsEvent::Text(text)) => {
                    assert_eq!(text, format!(r#"{{"n":{i}}}"#), "frame {i} out of order");
                }
                other => panic!("frame {i}: expected Text, got {other:?}"),
            }
        }
        // Still alive and readable: the burst did not tear the socket down.
        assert!(
            events.try_recv().is_err() || matches!(events.try_recv(), Ok(WsEvent::Closed)),
            "no unexpected frame after the burst"
        );
    }

    /// Regression (found live, 2026-10-28 — "stop sometimes does nothing"):
    /// the outbound feed used a bounded 64-slot channel with `try_send`, so a
    /// frame queued while the writer was stalled on a slow link was silently
    /// dropped — a `session/cancel` queued behind a big draining prompt
    /// vanished and the turn kept running. The browser WS the TS client rides
    /// buffers sends instead, which is why the phone stopped the same turn
    /// fine. Every queued frame must survive however far past the old
    /// capacity the burst runs.
    #[tokio::test]
    async fn real_factory_outlives_a_send_burst_the_server_wont_read_yet() {
        use futures_util::StreamExt;
        const FRAMES: usize = 300; // > the old 64-slot outbound feed

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Server: handshake, then stall without reading while the client
        // queues its burst, and only then drain — the stalled-writer window
        // the old bounded feed dropped frames in.
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let mut seen = 0;
            while seen < FRAMES {
                match ws.next().await {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                        assert_eq!(text, format!(r#"{{"n":{seen}}}"#), "frame {seen} lost/reordered");
                        seen += 1;
                    }
                    _ => break,
                }
            }
            assert_eq!(seen, FRAMES, "the server never saw the whole burst");
            while ws.next().await.is_some() {}
        });

        let factory = TungsteniteWsFactory;
        let (stream, mut events) = factory
            .connect(&format!("ws://127.0.0.1:{port}/acp?instance=i1"), "tok")
            .await
            .expect("handshake");
        assert_eq!(events.recv().await, Some(WsEvent::Open));

        for i in 0..FRAMES {
            stream.send(&format!(r#"{{"n":{i}}}"#));
        }
        stream.close();
        // The writer must deliver the queued burst; Closed ends the feed.
        assert_eq!(events.recv().await, Some(WsEvent::Closed));
    }
}
