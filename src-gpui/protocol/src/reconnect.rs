//! Connection supervisor — the Rust port of the appStore's connection
//! machinery (`src/store/appStore.ts`: `openConnection`, `scheduleReconnect`,
//! `tryReconnect`, `wakeProbe`, `teardownZombieConnection`, plus
//! `connectInstance`'s reset/auto-attach and `loadSession`'s wire shape).
//!
//! Scope: the CONNECTION-level state machine the reconnect-wedge regression
//! covers — connState transitions, exponential backoff, probe-based
//! discovery, instance-gone teardown, the wake liveness probe (fresh-wire
//! suppression + unanswered-ping teardown) and the watchdog against attempts
//! wedged past every legitimate budget. Message-store application (replay
//! folding, permission dialogs, pending-queue flushing) is deliberately NOT
//! here; consumers observe updates through [`SupervisorEvent`] and build
//! those layers on top.
//!
//! Timers run on the injected [`Clock`]; unit tests drive [`MockClock`].

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::mpsc;

use crate::acp::SessionUpdate;
use crate::acp_client::{
    parse_session_load_result, session_load_params, AcpConnection, AcpConnectionState,
    AcpHandlers, ServerRequest, ServerResponder, SessionAttachInfo, TurnStateInfo,
    REPLAY_TAIL_LIMIT,
};
use crate::clock::Clock;
use crate::hub::{ConnectionProfile, HubInstance};
use crate::hub_client::HubClient;
use crate::http::HttpTransport;
use crate::session::LoadIntent;
use crate::ws::WsFactory;

/// A reconnect attempt that outlived every legitimate budget (10s discovery
/// fetch + 15s WS deadline) is wedged — the watchdog tears it down.
pub const RECONNECT_RUN_WEDGE_MS: u64 = 45_000;
/// Fresh wire traffic proves liveness better than any probe — the wake probe
/// skips its ping while a turn is streaming.
pub const WIRE_ACTIVITY_FRESH_MS: u64 = 30_000;
/// A connect attempt wedged past this is treated as a zombie too.
pub const CONNECTING_WEDGE_MS: u64 = 30_000;
/// Consecutive network-level discovery failures before the hub is called
/// offline (a phone's network stack takes a moment after launch).
pub const DISCOVERY_FAIL_THRESHOLD: u32 = 3;
/// Wake-probe ping budget: any settle (even a rejection) within it proves
/// the round-trip; only silence means zombie.
pub const PROBE_TIMEOUT_MS: u64 = 5_000;

/// The store's `connState` (the socket-level ACP states ride inside the
/// connection itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnState {
    #[default]
    Idle,
    Connecting,
    Reconnecting,
    Open,
}

/// Events the supervisor surfaces to its owner (the app-layer store).
#[derive(Debug, Clone)]
pub enum SupervisorEvent {
    State(ConnState),
    Update {
        session_id: String,
        update: SessionUpdate,
        meta: Option<Value>,
    },
    ServerRequest(ServerRequest, ServerResponder),
    CancelRequest(u64),
    TurnState {
        session_id: String,
        running: bool,
        turn: Option<TurnStateInfo>,
    },
    WorkflowListChanged,
    /// A `session/load` answer landed; the replay cursor block is already
    /// applied to the state snapshot.
    Attached(String, SessionAttachInfo),
    SessionLoadFailed(String, String),
    /// The hub proved the instance gone ("notice.instanceGone").
    InstanceGone,
}

/// Read-only snapshot of the supervised state (the zustand slice the ported
/// tests assert on).
#[derive(Debug, Clone)]
pub struct SupervisorState {
    pub conn_state: ConnState,
    pub profile: Option<ConnectionProfile>,
    pub instance_id: Option<String>,
    pub active_session_id: Option<String>,
    pub loading_session: bool,
    pub fs_capable: bool,
    pub instances: Vec<HubInstance>,
    pub hub_offline: bool,
    pub instances_error: Option<String>,
    pub replay_cursor: Option<String>,
    pub has_more: bool,
    pub turn_active: bool,
}

#[derive(Debug, Clone)]
struct Shared {
    conn_state: ConnState,
    profile: Option<ConnectionProfile>,
    instance_id: Option<String>,
    active_session_id: Option<String>,
    loading_session: bool,
    fs_capable: bool,
    instances: Vec<HubInstance>,
    hub_offline: bool,
    instances_error: Option<String>,
    replay_cursor: Option<String>,
    has_more: bool,
    turn_active: bool,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            conn_state: ConnState::Idle,
            profile: None,
            instance_id: None,
            active_session_id: None,
            loading_session: false,
            fs_capable: false,
            instances: Vec::new(),
            hub_offline: false,
            instances_error: None,
            replay_cursor: None,
            has_more: false,
            turn_active: false,
        }
    }
}

struct Sup {
    http: Arc<dyn HttpTransport>,
    factory: Arc<dyn WsFactory>,
    clock: Arc<dyn Clock>,
    events: Option<mpsc::UnboundedSender<SupervisorEvent>>,
    state: Mutex<Shared>,

    // Module singletons (the TS module-level lets): connection + timers live
    // outside the snapshot on purpose.
    conn: Mutex<Option<AcpConnection>>,
    /// Generation counter — events from superseded connections are ignored
    /// (their async close can fire after a new connection to the SAME
    /// instance was started).
    conn_seq: AtomicU64,
    /// Cancelled-timer generation; a scheduled timer only fires while the
    /// epoch still matches. `stop_reconnect` bumps it.
    timer_epoch: AtomicU64,
    /// TS-style timer handle: `Some` while a reconnect timer was scheduled
    /// (cleared only by `stop_reconnect` — a FIRED timer leaves it set, the
    /// stale-id shape the watchdog's run_since clause exists for).
    reconnect_timer: Mutex<Option<u64>>,
    reconnect_attempt: AtomicU32,
    /// When the in-flight open_connection attempt started (None = none).
    connecting_since: Mutex<Option<u64>>,
    /// >0 while a try_reconnect run is in flight.
    reconnect_run_since: Mutex<Option<u64>>,
    /// Last time the socket delivered a session/update (0 = never).
    last_wire_activity: AtomicU64,
    discovery_failures: AtomicU32,
}

impl Sup {
    fn now(&self) -> u64 {
        self.clock.now_ms()
    }

    fn emit(&self, event: SupervisorEvent) {
        if let Some(tx) = &self.events {
            let _ = tx.send(event);
        }
    }

    fn stop_reconnect(&self) {
        self.timer_epoch.fetch_add(1, Ordering::SeqCst);
        *self.reconnect_timer.lock().unwrap() = None;
    }

    /// Backoff: 1s doubling, capped at 30s. Sets `Reconnecting` and arms the
    /// one-shot timer that runs the reconnect attempt.
    fn schedule_reconnect(self: &Arc<Self>) {
        {
            let s = self.state.lock().unwrap();
            if s.profile.is_none() || s.instance_id.is_none() {
                return;
            }
        }
        self.stop_reconnect();
        let attempt = self.reconnect_attempt.fetch_add(1, Ordering::SeqCst);
        let delay = if attempt >= 16 {
            30_000
        } else {
            (1000u64 << attempt).min(30_000)
        };
        self.state.lock().unwrap().conn_state = ConnState::Reconnecting;
        self.emit(SupervisorEvent::State(ConnState::Reconnecting));
        let my_epoch = self.timer_epoch.load(Ordering::SeqCst);
        *self.reconnect_timer.lock().unwrap() = Some(my_epoch);
        let sup = Arc::clone(self);
        tokio::spawn(async move {
            sup.clock.sleep_ms(delay).await;
            if sup.timer_epoch.load(Ordering::SeqCst) != my_epoch {
                return; // cleared or superseded meanwhile
            }
            sup.run_reconnect().await;
        });
    }

    /// Stamp the run so `wake_probe` can recognize a wedged attempt: a fired
    /// timer leaves the handle set, so "!reconnect_timer" alone cannot see
    /// the wedge where the timer callback itself hangs.
    async fn run_reconnect(self: &Arc<Self>) {
        *self.reconnect_run_since.lock().unwrap() = Some(self.now());
        self.try_reconnect_inner().await;
        *self.reconnect_run_since.lock().unwrap() = None;
    }

    async fn try_reconnect_inner(self: &Arc<Self>) {
        let (profile, instance) = {
            let s = self.state.lock().unwrap();
            match (s.profile.clone(), s.instance_id.clone()) {
                (Some(p), Some(i)) => (p, i),
                _ => return,
            }
        };
        // Probe first: a hard-killed bridge lingers in the heartbeat view for
        // up to 30s, which would keep the loop retrying a dead instance.
        self.refresh_instances(true).await;
        // Re-read after the await: the hub/instance may have changed meanwhile.
        {
            let s = self.state.lock().unwrap();
            if s.profile.as_ref() != Some(&profile) || s.instance_id.as_deref() != Some(instance.as_str()) {
                return;
            }
            // Hub unreachable or discovery failed (phone just woke, network
            // not yet ready): instance liveness is UNKNOWN, not "gone" —
            // retry in place instead of bouncing the user out. Only a
            // successful listing that lacks the instance proves it gone.
            if s.hub_offline || s.instances_error.is_some() {
                drop(s);
                self.schedule_reconnect();
                return;
            }
            if !s.instances.iter().any(|i| i.id == instance) {
                drop(s);
                // Instance gone: its sessions are gone too (contract).
                self.conn_seq.fetch_add(1, Ordering::SeqCst);
                if let Some(conn) = self.conn.lock().unwrap().take() {
                    conn.close();
                }
                *self.connecting_since.lock().unwrap() = None;
                *self.reconnect_run_since.lock().unwrap() = None;
                {
                    let mut s = self.state.lock().unwrap();
                    s.conn_state = ConnState::Idle;
                    s.instance_id = None;
                    s.active_session_id = None;
                    s.fs_capable = false;
                    s.loading_session = false;
                    s.replay_cursor = None;
                    s.has_more = false;
                    s.turn_active = false;
                }
                self.reconnect_attempt.store(0, Ordering::SeqCst);
                self.emit(SupervisorEvent::State(ConnState::Idle));
                self.emit(SupervisorEvent::InstanceGone);
                return;
            }
        }
        self.open_connection(profile, instance).await;
    }

    /// Discovery refresh with the store's error semantics: hub-unreachable
    /// is an expected offline state (only after DISCOVERY_FAIL_THRESHOLD
    /// consecutive failures does it blank the list); other failures surface
    /// as `instances_error`.
    async fn refresh_instances(self: &Arc<Self>, probe: bool) {
        let profile = self.state.lock().unwrap().profile.clone();
        let Some(profile) = profile else { return };
        let client = HubClient::new(
            profile.hub_url.clone(),
            profile.token.clone(),
            Arc::clone(&self.http),
            Arc::clone(&self.clock),
        );
        let result = client.instances(probe).await;
        // Hub changed mid-flight: drop the answer.
        if self.state.lock().unwrap().profile.as_ref() != Some(&profile) {
            return;
        }
        match result {
            Ok(instances) => {
                self.discovery_failures.store(0, Ordering::SeqCst);
                let mut s = self.state.lock().unwrap();
                s.instances = instances;
                s.hub_offline = false;
                s.instances_error = None;
            }
            Err(e) if e.network => {
                let failures = self.discovery_failures.fetch_add(1, Ordering::SeqCst) + 1;
                if failures >= DISCOVERY_FAIL_THRESHOLD {
                    let mut s = self.state.lock().unwrap();
                    s.instances.clear();
                    s.hub_offline = true;
                }
            }
            Err(e) => {
                let mut s = self.state.lock().unwrap();
                s.instances_error = Some(e.message);
                s.hub_offline = false;
            }
        }
    }

    /// The TS `openConnection`: stop the loop, supersede prior connections,
    /// connect + initialize, then replay-load the active session (replay is
    /// the catch-up mechanism after any disconnect). A failed handshake
    /// hands the story back to the reconnect loop.
    async fn open_connection(self: &Arc<Self>, profile: ConnectionProfile, instance_id: String) {
        self.stop_reconnect();
        let my_seq = self.conn_seq.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(old) = self.conn.lock().unwrap().take() {
            old.close();
        }
        let handlers = Arc::new(SupHandlers {
            sup: Arc::clone(self),
            seq: my_seq,
            instance: instance_id.clone(),
        });
        let (conn, connect) = AcpConnection::start(
            &profile.hub_url,
            &profile.token,
            &instance_id,
            handlers,
            Arc::clone(&self.factory),
            Arc::clone(&self.clock),
        );
        // Store the handle before the handshake settles so a teardown
        // mid-connect can close the socket (TS: `acp = conn` pre-connect).
        *self.conn.lock().unwrap() = Some(conn);
        *self.connecting_since.lock().unwrap() = Some(self.now());
        match connect.await {
            Ok(init) => {
                if self.conn_seq.load(Ordering::SeqCst) != my_seq {
                    *self.connecting_since.lock().unwrap() = None;
                    return;
                }
                *self.connecting_since.lock().unwrap() = None;
                self.state.lock().unwrap().fs_capable = init.fs_capability();
                let active = self.state.lock().unwrap().active_session_id.clone();
                if let Some(active) = active {
                    self.load_session(&active).await;
                }
            }
            Err(_) => {
                *self.connecting_since.lock().unwrap() = None;
                let mine = {
                    let s = self.state.lock().unwrap();
                    s.instance_id.as_deref() == Some(instance_id.as_str())
                };
                if mine {
                    self.schedule_reconnect();
                }
            }
        }
    }

    /// The TS `loadSession` wire shape: attach (or re-attach) a session with
    /// a tail replay; the answer's replayMeta drives pagination state. The
    /// active session id is set BEFORE the request is sent, exactly like the
    /// store (its optimistic-UI contract).
    async fn load_session(self: &Arc<Self>, session_id: &str) {
        let conn = match self.conn.lock().unwrap().clone() {
            Some(conn) => conn,
            None => return,
        };
        {
            let mut s = self.state.lock().unwrap();
            s.active_session_id = Some(session_id.to_string());
            s.loading_session = true;
        }
        // cwd is REQUIRED by the SDK's session/load schema; the bridge
        // ignores the value on load, so an unknown workspace sends "/".
        let cwd = {
            let s = self.state.lock().unwrap();
            let instance = s.instance_id.clone();
            s.instances
                .iter()
                .find(|i| Some(i.id.as_str()) == instance.as_deref())
                .and_then(|i| i.workspace.clone())
                .unwrap_or_else(|| "/".to_string())
        };
        let request = conn.request(
            "session/load",
            // Auto intent: this IS the client-automatic re-attach (a closed
            // conversation answers sessionClosed instead of re-arming).
            session_load_params(session_id, &cwd, REPLAY_TAIL_LIMIT, LoadIntent::Auto),
        );
        match request.await {
            Ok(result) => {
                let info = parse_session_load_result(&result);
                {
                    let mut s = self.state.lock().unwrap();
                    s.loading_session = false;
                    if let Some(replay) = &info.replay {
                        s.replay_cursor = replay.cursor.clone();
                        s.has_more = replay.has_more.unwrap_or(false);
                        s.turn_active = replay.turn_active.unwrap_or(false);
                    }
                }
                self.emit(SupervisorEvent::Attached(session_id.to_string(), info));
            }
            Err(e) => {
                self.state.lock().unwrap().loading_session = false;
                self.emit(SupervisorEvent::SessionLoadFailed(
                    session_id.to_string(),
                    e.0,
                ));
            }
        }
    }

    /// Ping the pipe: the bridge's SDK answers unknown methods with an
    /// immediate -32601, so ANY settle within the budget proves the
    /// round-trip. Only silence — or a locally-refused send — means zombie.
    async fn probe_alive(&self, timeout_ms: u64) -> bool {
        let conn = match self.conn.lock().unwrap().clone() {
            Some(conn) => conn,
            None => return false,
        };
        let request = conn.request("$/zcode/ping", serde_json::json!({}));
        let sleep = self.clock.sleep_ms(timeout_ms);
        tokio::pin!(request);
        tokio::pin!(sleep);
        tokio::select! {
            biased;
            result = &mut request => match result {
                Ok(_) => true,
                Err(e) => e.0 != "connection not open",
            },
            _ = &mut sleep => false,
        }
    }

    /// Hard teardown of whatever is (supposedly) running: bump the
    /// generation (every pending event goes stale), close the socket, and
    /// restart the backoff from the bottom so recovery is a 1s retry, not
    /// another 30s.
    fn teardown_zombie_connection(self: &Arc<Self>) {
        self.conn_seq.fetch_add(1, Ordering::SeqCst);
        if let Some(conn) = self.conn.lock().unwrap().take() {
            conn.close();
        }
        *self.connecting_since.lock().unwrap() = None;
        *self.reconnect_run_since.lock().unwrap() = None;
        self.reconnect_attempt.store(0, Ordering::SeqCst);
        self.schedule_reconnect();
    }

    /// Liveness probe for wake. Belt-and-braces for wedged attempts in
    /// mid-(re)connect: an attempt that outlived its deadline, or a
    /// "reconnecting" with no timer left, gets the same teardown a zombie
    /// socket gets — without it the connect deadline is the only way out.
    async fn wake_probe(self: &Arc<Self>) {
        let (has_profile, conn_state) = {
            let s = self.state.lock().unwrap();
            (s.profile.is_some() && s.instance_id.is_some(), s.conn_state)
        };
        if !has_profile {
            return;
        }
        match conn_state {
            ConnState::Open => {
                // Fresh traffic proves the pipe; probe only after silence.
                let now = self.now();
                let last = self.last_wire_activity.load(Ordering::SeqCst);
                if now.saturating_sub(last) < WIRE_ACTIVITY_FRESH_MS {
                    return;
                }
                if self.probe_alive(PROBE_TIMEOUT_MS).await {
                    return;
                }
                if self.state.lock().unwrap().conn_state != ConnState::Open {
                    return; // closed meanwhile — the loop has it
                }
                self.teardown_zombie_connection();
            }
            ConnState::Connecting => {
                let since = *self.connecting_since.lock().unwrap();
                if since.is_some_and(|t| self.now().saturating_sub(t) > CONNECTING_WEDGE_MS) {
                    self.teardown_zombie_connection();
                }
            }
            ConnState::Reconnecting => {
                let no_timer = self.reconnect_timer.lock().unwrap().is_none();
                let run_wedged = self
                    .reconnect_run_since
                    .lock()
                    .unwrap()
                    .is_some_and(|t| self.now().saturating_sub(t) > RECONNECT_RUN_WEDGE_MS);
                if no_timer || run_wedged {
                    self.teardown_zombie_connection();
                }
            }
            ConnState::Idle => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Public handle
// ---------------------------------------------------------------------------

/// Cloneable handle; the machinery lives in one shared [`Sup`].
#[derive(Clone)]
pub struct Supervisor {
    inner: Arc<Sup>,
}

impl Supervisor {
    pub fn new(
        http: Arc<dyn HttpTransport>,
        factory: Arc<dyn WsFactory>,
        clock: Arc<dyn Clock>,
        events: Option<mpsc::UnboundedSender<SupervisorEvent>>,
    ) -> Self {
        Self {
            inner: Arc::new(Sup {
                http,
                factory,
                clock,
                events,
                state: Mutex::new(Shared::default()),
                conn: Mutex::new(None),
                conn_seq: AtomicU64::new(0),
                timer_epoch: AtomicU64::new(0),
                reconnect_timer: Mutex::new(None),
                reconnect_attempt: AtomicU32::new(0),
                connecting_since: Mutex::new(None),
                reconnect_run_since: Mutex::new(None),
                last_wire_activity: AtomicU64::new(0),
                discovery_failures: AtomicU32::new(0),
            }),
        }
    }

    pub fn state(&self) -> SupervisorState {
        let s = self.inner.state.lock().unwrap();
        SupervisorState {
            conn_state: s.conn_state,
            profile: s.profile.clone(),
            instance_id: s.instance_id.clone(),
            active_session_id: s.active_session_id.clone(),
            loading_session: s.loading_session,
            fs_capable: s.fs_capable,
            instances: s.instances.clone(),
            hub_offline: s.hub_offline,
            instances_error: s.instances_error.clone(),
            replay_cursor: s.replay_cursor.clone(),
            has_more: s.has_more,
            turn_active: s.turn_active,
        }
    }

    /// Activate a hub profile and prime discovery once (the TS
    /// `connectToHub` minus the 4s UI poll loop and book persistence, which
    /// are app-layer concerns).
    pub async fn connect_to_hub(&self, profile: ConnectionProfile) {
        self.disconnect_hub();
        self.inner.state.lock().unwrap().profile = Some(profile);
        self.inner.refresh_instances(false).await;
    }

    /// Tear the whole supervised connection down (the TS `disconnectHub` /
    /// `teardownConnection`): stop timers, close the socket, reset state.
    pub fn disconnect_hub(&self) {
        let inner = &self.inner;
        inner.stop_reconnect();
        inner.conn_seq.fetch_add(1, Ordering::SeqCst);
        if let Some(conn) = inner.conn.lock().unwrap().take() {
            conn.close();
        }
        *inner.connecting_since.lock().unwrap() = None;
        *inner.reconnect_run_since.lock().unwrap() = None;
        inner.reconnect_attempt.store(0, Ordering::SeqCst);
        inner.last_wire_activity.store(0, Ordering::SeqCst);
        inner.discovery_failures.store(0, Ordering::SeqCst);
        {
            let mut s = inner.state.lock().unwrap();
            s.profile = None;
            s.instance_id = None;
            s.active_session_id = None;
            s.conn_state = ConnState::Idle;
            s.instances.clear();
            s.hub_offline = false;
            s.instances_error = None;
            s.fs_capable = false;
            s.loading_session = false;
            s.replay_cursor = None;
            s.has_more = false;
            s.turn_active = false;
        }
        inner.emit(SupervisorEvent::State(ConnState::Idle));
    }

    /// Open (or re-open) a bridge instance connection and attach: the TS
    /// `connectInstance` reset + `openConnection` + auto-attach. Fire and
    /// forget — the flow (including its `session/load`) may legitimately run
    /// for a long time; observers watch [`SupervisorEvent`] / [`Supervisor::state`].
    pub async fn connect_instance(&self, instance_id: &str, attach_session_id: Option<&str>, no_attach: bool) {
        let profile = match self.inner.state.lock().unwrap().profile.clone() {
            Some(p) => p,
            None => return,
        };
        let inner = &self.inner;
        inner.stop_reconnect();
        inner.reconnect_attempt.store(0, Ordering::SeqCst);
        {
            let mut s = inner.state.lock().unwrap();
            s.instance_id = Some(instance_id.to_string());
            s.conn_state = ConnState::Connecting;
            s.active_session_id = None;
            s.loading_session = false;
            s.fs_capable = false;
            s.replay_cursor = None;
            s.has_more = false;
            s.turn_active = false;
        }
        inner.emit(SupervisorEvent::State(ConnState::Connecting));

        let sup = Arc::clone(inner);
        let instance_id = instance_id.to_string();
        let attach = attach_session_id.map(str::to_string);
        let no_attach = no_attach;
        tokio::spawn(async move {
            sup.open_connection(profile, instance_id.clone()).await;
            // Attach to the requested session, else the most recently
            // updated one. no_attach (remote session-create): the caller
            // opens a FRESH session itself — never auto-attach a recycled
            // instance's history.
            let target = {
                let s = sup.state.lock().unwrap();
                if !no_attach
                    && s.conn_state == ConnState::Open
                    && s.active_session_id.is_none()
                {
                    attach.or_else(|| {
                        s.instances
                            .iter()
                            .find(|i| i.id == instance_id)
                            .and_then(|i| {
                                i.sessions
                                    .iter()
                                    .max_by_key(|sess| sess.updated_at.unwrap_or(0))
                                    .map(|sess| sess.session_id.clone())
                            })
                    })
                } else {
                    None
                }
            };
            if let Some(target) = target {
                sup.load_session(&target).await;
            }
        });
    }

    /// Re-attach a session on the current connection (the TS `loadSession`;
    /// switching sessions is a `session/load` on the same connection).
    pub async fn load_session(&self, session_id: &str) {
        self.inner.load_session(session_id).await;
    }

    /// The wake liveness probe / wedge watchdog (the TS `wakeProbe`).
    pub async fn wake_probe(&self) {
        self.inner.wake_probe().await;
    }
}

// ---------------------------------------------------------------------------
// Connection event handlers (the store's AcpHandlers wiring)
// ---------------------------------------------------------------------------

struct SupHandlers {
    sup: Arc<Sup>,
    seq: u64,
    instance: String,
}

impl SupHandlers {
    fn stale(&self) -> bool {
        self.sup.conn_seq.load(Ordering::SeqCst) != self.seq
    }
}

impl AcpHandlers for SupHandlers {
    fn on_state(&self, state: AcpConnectionState) {
        if self.stale() {
            return;
        }
        match state {
            AcpConnectionState::Open => {
                self.sup.reconnect_attempt.store(0, Ordering::SeqCst);
                *self.sup.connecting_since.lock().unwrap() = None;
                self.sup.last_wire_activity.store(self.sup.now(), Ordering::SeqCst);
                // Activity is broadcast-only: a fresh connection knows
                // nothing until events arrive (the active session's state
                // is restored by load_session's replay).
                self.sup.state.lock().unwrap().conn_state = ConnState::Open;
                self.sup.emit(SupervisorEvent::State(ConnState::Open));
            }
            AcpConnectionState::Closed => {
                let mine = {
                    let s = self.sup.state.lock().unwrap();
                    s.instance_id.as_deref() == Some(self.instance.as_str())
                };
                if mine {
                    // The pending permission/elicitation requests die with
                    // the socket (app-layer clears its dialogs on this
                    // transition) — back into the loop.
                    self.sup.schedule_reconnect();
                }
            }
            AcpConnectionState::Connecting => {}
        }
    }

    fn on_update(&self, session_id: &str, update: &SessionUpdate, meta: Option<&Value>) {
        if self.stale() {
            return;
        }
        self.sup.last_wire_activity.store(self.sup.now(), Ordering::SeqCst);
        self.sup.emit(SupervisorEvent::Update {
            session_id: session_id.to_string(),
            update: update.clone(),
            meta: meta.cloned(),
        });
    }

    fn on_server_request(&self, req: ServerRequest, respond: ServerResponder) {
        if self.stale() {
            return;
        }
        self.sup.emit(SupervisorEvent::ServerRequest(req, respond));
    }

    fn on_cancel_request(&self, id: u64) {
        if self.stale() {
            return;
        }
        self.sup.emit(SupervisorEvent::CancelRequest(id));
    }

    fn on_turn_state(&self, session_id: &str, running: bool, turn: Option<TurnStateInfo>) {
        if self.stale() {
            return;
        }
        self.sup.emit(SupervisorEvent::TurnState {
            session_id: session_id.to_string(),
            running,
            turn,
        });
    }

    fn on_workflow_list_changed(&self) {
        if self.stale() {
            return;
        }
        self.sup.emit(SupervisorEvent::WorkflowListChanged);
    }
}

#[cfg(test)]
mod tests;
