//! Session data semantics state machine — port of the session slice of
//! `src/store/appStore.ts` (loadSession / loadEarlier / runPrompt / the
//! pending queue / the replay cache snapshot), driven on top of the pure
//! helpers in [`crate::replay`].
//!
//! The TS store interleaves wire I/O with state transitions inside async
//! functions; this core is the same logic with the I/O factored out as an
//! outbox. Every method that would send an ACP request pushes a
//! [`WireRequest`] onto the outbox ([`SessionCore::take_wire`]); the runtime
//! layer sends it and feeds the response back through the matching
//! `on_*_result` / `on_*_failed` method. Timers (the 120ms update-coalescing
//! batch, the 120ms late-page coalescer, the legacy earlier-window poll) are
//! the caller's job: it calls [`SessionCore::flush_update_batch`],
//! [`SessionCore::flush_late_page`] and [`SessionCore::poll_earlier_window`]
//! when its timer fires. `now_ms` parameters stand in for `Date.now()`.
//!
//! Not ported (UI/transport concerns outside the named semantics): notices,
//! toasts, permission/elicitation dialogs, quota refresh, reconnect
//! scheduling. `pending_prompts` lives in memory here; the app layer persists
//! it the way the TS app persists it to localStorage.

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::acp::{ConfigOption, ContextUsage, SessionUpdate, SlashCommand};
use crate::chat::{AttachmentDraft, ChatMessage, ChatPart, ChatRole, PromptDraft, TurnStats};
use crate::replay::{
    apply_one, bridge_supports_page_marker, is_earlier_page_meta, is_local_message_id,
    read_replay_meta, snapshot_tail_id, ApplyPatch, MessageFactory, PlanEntry, ReplayMeta,
    SessionCache, SessionSnapshot, TextPartKind, EARLIER_PAGE_LIMIT, REPLAY_TAIL_LIMIT,
};

/// Legacy earlier-window timing (pre-0.44.1 bridges send unmarked pages, and
/// the load_earlier response can overtake its own notifications on the wire):
/// the collection window stays open until the burst goes quiet, a hard cap
/// fires, or our own prompt starts.
pub const EARLIER_WINDOW_QUIET_MS: u64 = 600;
pub const EARLIER_WINDOW_CAP_MS: u64 = 20_000;

/// A request the runtime layer must put on the wire. Responses come back
/// through the matching `on_*_result` / `on_*_failed` method, handing the
/// same request back for correlation.
#[derive(Debug, Clone, PartialEq)]
pub enum WireRequest {
    /// `session/load` with `_meta.zcode.limit` — 0 is the metadata-only
    /// reconcile attach, [`REPLAY_TAIL_LIMIT`] the full tail replay.
    Load(LoadRequest),
    LoadEarlier(LoadEarlierRequest),
    Prompt(PromptRequest),
    /// `session/cancel` notification (no response).
    CancelTurn { session_id: String },
    /// `session/set_config_option` (model / mode / thought): the answer's
    /// `configOptions` array replaces the whole set; the bridge also
    /// broadcasts `config_option_update` / `current_mode_update`, which land
    /// through the regular update patch.
    SetConfig {
        session_id: String,
        config_id: String,
        value: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadRequest {
    pub session_id: String,
    pub limit: u32,
    /// Correlation token: only the currently in-flight load's response is
    /// applied (a newer load_session supersedes an older in-flight one).
    pub seq: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadEarlierRequest {
    pub session_id: String,
    /// The stored replay cursor, passed back VERBATIM — never recomputed
    /// client-side.
    pub before: String,
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PromptRequest {
    pub session_id: String,
    /// The wire prompt content blocks (text + image attachments).
    pub prompt: Vec<Value>,
    pub started_at_ms: u64,
    /// The optimistic user bubble `run_prompt` inserted for this request, so
    /// a server-side rejection (the turn never ran) can retract it when the
    /// draft goes back to the queue.
    pub bubble_id: String,
}

/// Per-session display state (session-list badges). Not persisted: after a
/// reconnect other sessions read as idle until the next broadcast event.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionActivity {
    pub running: bool,
    pub awaiting_permission: bool,
    pub finished_at: Option<u64>,
}

/// What `send_prompt` did with the draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// Sent now (request on the outbox).
    Ran,
    /// Parked in the pending queue (turn running, replay loading, or the
    /// connection is down).
    Queued,
    /// No active session.
    NoSession,
}

/// What `force_send_pending` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForceSendOutcome {
    /// Target moved to the front and our own running turn was cancelled —
    /// the cancelled prompt's settle-flush then sends the reordered head.
    ReorderedAndCancelled,
    /// Target sent immediately: no turn was running, or the running turn
    /// was foreign/restored (cancelled first, never waited on — its settle
    /// does not come back to this connection).
    Sent,
    /// Refused: no session, empty queue, replay in flight, or disconnected
    /// (run_prompt would silently swallow the draft).
    Refused,
}

/// Outcome of feeding a `session/load_earlier` response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EarlierOutcome {
    /// The page was assembled and prepended (`true`), or the response landed
    /// for a session the store has since switched away from.
    Applied(bool),
    /// Legacy bridge: the collection window stays open — keep delivering
    /// updates and poll [`SessionCore::poll_earlier_window`] until it
    /// settles.
    AwaitingQuiet,
}

/// Outcome of one legacy earlier-window poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EarlierPoll {
    StillCollecting,
    Applied(bool),
    Idle,
}

#[derive(Debug)]
struct QueuedUpdate {
    session_id: String,
    update: SessionUpdate,
    meta: Option<Value>,
}

#[derive(Debug)]
struct PendingLoad {
    seq: u64,
    session_id: String,
    /// The snapshot painted for this load (present iff the reconcile attach,
    /// limit 0, was issued from a cache hit).
    snapshot: Option<SessionSnapshot>,
}

/// The session-scoped half of the app store. Field-for-field port of the
/// store's session state plus the module-level singletons that shape its
/// semantics (update batch, earlier window, local-prompt flag, message
/// counter, replay cache).
#[derive(Debug, Default)]
pub struct SessionCore {
    pub conn_open: bool,
    pub active_session_id: Option<String>,
    pub messages: Vec<ChatMessage>,
    pub plan_entries: Option<Vec<PlanEntry>>,
    pub is_running: bool,
    /// Follow-ups typed while a turn is running or a replay is still loading;
    /// flushed in order the moment the prompt settles / the session attaches.
    /// Keyed by session id so drafts survive session switches.
    pub pending_prompts: HashMap<String, Vec<PromptDraft>>,
    /// Pagination state from the attach response's replayMeta.
    pub replay_cursor: Option<String>,
    pub has_more: bool,
    pub total_messages: Option<u64>,
    pub loading_earlier: bool,
    pub loading_session: bool,
    pub config_options: Vec<ConfigOption>,
    pub current_mode_id: Option<String>,
    pub usage: Option<ContextUsage>,
    /// Last turn's prompt-cache hit rate (percent). None until a turn
    /// reports cache numbers.
    pub cache_hit: Option<u32>,
    pub turn_stats: Option<TurnStats>,
    /// Slash commands from available_commands_update (overwrite semantics).
    pub available_commands: Vec<SlashCommand>,
    /// Per-session running/permission state from bridge-wide broadcasts.
    pub session_states: HashMap<String, SessionActivity>,
    /// Bridge marks load_earlier page updates `_meta.zcode.earlierPage`
    /// (zcode-acp-server 0.44.1+); gates the marker-based page routing.
    pub page_marker_capable: bool,

    // ---- module-level singletons from the TS closure ----
    /// True while a session/prompt WE sent is in flight. Turn-end
    /// notifications ($/zcode/turnState) arrive before the prompt response;
    /// those turns settle in the prompt's own finally, so the notification
    /// path must not double-settle. The quit dialog reads it to tell turns
    /// this client started from other clients' turns.
    pub local_prompt_active: bool,
    factory: MessageFactory,
    cache: SessionCache,
    /// The 120ms update-coalescing batch (drained by flush_update_batch).
    update_queue: Vec<QueuedUpdate>,
    /// While a session/load_earlier request is in flight its updates must be
    /// PREPENDED, not appended — buffered here until the request resolves.
    collecting_earlier: bool,
    earlier_buffer: Vec<QueuedUpdate>,
    earlier_session: Option<String>,
    earlier_stream_last_at: u64,
    earlier_cap_at: u64,
    pending_earlier_result: Option<Value>,
    /// Page updates (marked earlierPage) arriving OUTSIDE the collection
    /// window — the load_earlier response already landed. They are replay
    /// content by construction, never live, so they must not fall into the
    /// update batch (that lands them at the tail). Held briefly so a burst
    /// coalesces into ONE prepended segment.
    late_page_buffer: Vec<QueuedUpdate>,
    outbox: Vec<WireRequest>,
    load_seq: u64,
    pending_load: Option<PendingLoad>,
}

impl SessionCore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies the initialize result's agentInfo.version (drives
    /// `page_marker_capable`).
    pub fn on_initialize(&mut self, agent_version: Option<&str>) {
        self.page_marker_capable = bridge_supports_page_marker(agent_version);
    }

    /// Drains the requests the core needs on the wire.
    pub fn take_wire(&mut self) -> Vec<WireRequest> {
        std::mem::take(&mut self.outbox)
    }

    /// The replay cache (for direct inspection; loads go through
    /// load_session).
    pub fn read_cache(&mut self, session_id: &str) -> Option<SessionSnapshot> {
        self.cache.read(session_id)
    }

    /// Whether the replay cache currently holds this session.
    pub fn cache_contains(&self, session_id: &str) -> bool {
        self.cache.contains(session_id)
    }

    pub fn remove_cache(&mut self, session_id: &str) {
        self.cache.remove(session_id);
    }

    // ---- update application ----

    /// Routes one `session/update`. Page updates ride the earlier buffer;
    /// live-turn updates streaming during the page request append normally
    /// instead of being prepended with the page. On a marker-capable bridge
    /// (0.44.1+) the marker alone routes the stragglers into the late-page
    /// buffer (prepended); an older bridge buffers everything during the
    /// collection window.
    pub fn on_update(
        &mut self,
        now_ms: u64,
        session_id: &str,
        update: SessionUpdate,
        meta: Option<Value>,
    ) {
        let marked = is_earlier_page_meta(update.rest.get("_meta"))
            || is_earlier_page_meta(meta.as_ref());
        let active = self.active_session_id.as_deref() == Some(session_id);
        if self.collecting_earlier && active && (!self.page_marker_capable || marked) {
            self.earlier_buffer.push(QueuedUpdate {
                session_id: session_id.to_owned(),
                update,
                meta,
            });
            self.earlier_stream_last_at = now_ms;
            return;
        }
        if marked {
            self.late_page_buffer.push(QueuedUpdate {
                session_id: session_id.to_owned(),
                update,
                meta,
            });
            return;
        }
        self.update_queue.push(QueuedUpdate {
            session_id: session_id.to_owned(),
            update,
            meta,
        });
    }

    /// Applies (or re-applies) whatever arrived since the last batch — the
    /// 120ms coalescing timer's body, called synchronously by
    /// flush_pending, which must not let the optimistic bubble overtake
    /// already-received turn content. Ends with a cache re-snapshot: a
    /// replay's final chunks typically sit here when the load response
    /// resolves, and snapshotting before this flush would write them out of
    /// the cache while totalMessages (from the response meta) already
    /// counted them.
    pub fn flush_update_batch(&mut self, now_ms: u64) {
        let batch = std::mem::take(&mut self.update_queue);
        if batch.is_empty() {
            return;
        }
        for q in &batch {
            let sid = match &self.active_session_id {
                Some(s) => s.as_str(),
                None => continue,
            };
            if q.session_id != sid {
                continue;
            }
            let patch = apply_one(
                &mut self.messages,
                &q.update,
                q.meta.as_ref(),
                &mut self.factory,
                now_ms,
            );
            self.apply_patch(patch);
        }
        self.cache_active_snapshot();
    }

    fn apply_patch(&mut self, patch: ApplyPatch) {
        match patch {
            ApplyPatch::Messages => {}
            ApplyPatch::Plan(entries) => self.plan_entries = entries,
            ApplyPatch::ConfigOptions(opts) => self.config_options = opts,
            ApplyPatch::CurrentMode(mode) => self.current_mode_id = Some(mode),
            ApplyPatch::Usage(usage) => self.usage = Some(usage),
            ApplyPatch::AvailableCommands(commands) => self.available_commands = commands,
            ApplyPatch::Nothing => {}
        }
    }

    /// Prepends whatever late page updates accumulated as its own
    /// oldest-first segment (the 120ms late-page coalescer's body). Stale
    /// sessions are dropped.
    pub fn flush_late_page(&mut self, now_ms: u64) {
        let page = std::mem::take(&mut self.late_page_buffer);
        if page.is_empty() {
            return;
        }
        let sid = match &self.active_session_id {
            Some(s) => s.clone(),
            None => return,
        };
        let mut segment: Vec<ChatMessage> = Vec::new();
        let mut mine = false;
        for q in &page {
            if q.session_id != sid {
                continue;
            }
            mine = true;
            apply_one(
                &mut segment,
                &q.update,
                q.meta.as_ref(),
                &mut self.factory,
                now_ms,
            );
        }
        if !mine || segment.is_empty() {
            return;
        }
        let mut rest = std::mem::take(&mut self.messages);
        if !rest.is_empty() && segment.last().map(|m| m.id.as_str()) == rest.first().map(|m| m.id.as_str())
        {
            // Seam dedupe: the same message split across pages.
            let seam = segment.pop().expect("segment non-empty");
            if segment.is_empty() {
                // The whole page was the seam: nothing to prepend, keep the
                // current head as is.
                self.messages = rest;
                return;
            }
            let head = rest.remove(0);
            let mut merged = seam;
            merged.parts.extend(head.parts);
            rest.insert(0, merged);
        }
        segment.extend(rest);
        self.messages = segment;
    }

    fn drop_queued_updates(&mut self) {
        self.update_queue.clear();
        self.collecting_earlier = false;
        self.earlier_buffer.clear();
        self.late_page_buffer.clear();
        self.pending_earlier_result = None;
        self.earlier_session = None;
    }

    // ---- cache snapshot ----

    /// Snapshots the active session into the replay cache. Skipped while a
    /// turn is in flight (ours or foreign): mid-turn messages are partial,
    /// and the optimistic prompt bubble may never reach the bridge. Also
    /// skipped while loading_session — a replay/reconcile is mid-flight and
    /// the store holds a half-applied history. Both writers that settle a
    /// turn (turn-state end, the prompt settle) re-snapshot through their
    /// own flush paths.
    fn cache_active_snapshot(&mut self) {
        let sid = match &self.active_session_id {
            Some(s) => s.clone(),
            None => return,
        };
        if self.loading_session {
            return;
        }
        if self.is_running || self.local_prompt_active {
            return;
        }
        self.cache.write(
            &sid,
            SessionSnapshot {
                messages: self.messages.clone(),
                plan_entries: self.plan_entries.clone(),
                replay_cursor: self.replay_cursor.clone(),
                has_more: self.has_more,
                total_messages: self.total_messages,
                last_message_id: snapshot_tail_id(&self.messages),
                config_options: self.config_options.clone(),
                current_mode_id: self.current_mode_id.clone(),
                usage: self.usage,
                cache_hit: self.cache_hit,
                turn_stats: self.turn_stats,
            },
        );
    }

    // ---- turn usage readouts ----

    /// Folds a turn's usage block (prompt response, or the turn-end
    /// broadcast a NEW bridge attaches for attach-only clients) into the
    /// title-bar readouts. Turns without cache numbers keep the previous
    /// rate; tps needs output tokens plus a wall time (absent on foreign
    /// turns from an old bridge).
    fn apply_turn_usage(&mut self, session_id: &str, usage: &Value, seconds: Option<f64>) {
        let hit = cache_hit_rate(usage);
        let out_tok = usage
            .get("outputTokens")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        if hit.is_none() && !(out_tok > 0.0 && seconds.is_some()) {
            return;
        }
        if self.active_session_id.as_deref() != Some(session_id) {
            return;
        }
        if let Some(h) = hit {
            self.cache_hit = Some(h);
        }
        if out_tok > 0.0 {
            if let Some(secs) = seconds.filter(|s| *s > 0.0) {
                self.turn_stats = Some(TurnStats {
                    tps: (out_tok / secs).round(),
                    output_tokens: out_tok.round() as u64,
                    seconds: (secs * 10.0).round() / 10.0,
                });
            }
        }
    }

    /// Bridge-wide turn-state broadcast: session-list activity plus, for the
    /// active session, the running UI. End-of-turn arrives BEFORE the prompt
    /// response for locally sent turns (the bridge notifies in its finally,
    /// responds on return) — those own their state in the prompt's settle.
    /// Restored/foreign turns have no local prompt in flight: settle here
    /// and flush.
    pub fn on_turn_state(
        &mut self,
        now_ms: u64,
        session_id: &str,
        running: bool,
        turn: Option<&Value>,
    ) {
        let turn = turn.filter(|t| !t.is_null());
        let turn_usage = turn
            .and_then(|t| t.get("usage"))
            .filter(|u| !u.is_null());
        let turn_ms = turn
            .and_then(|t| t.get("turnMs"))
            .and_then(Value::as_f64);

        // Session-list activity (awaiting > running > just finished > idle).
        let prev = self
            .session_states
            .get(session_id)
            .cloned()
            .unwrap_or_default();
        let next = if running {
            SessionActivity {
                running: true,
                awaiting_permission: false,
                finished_at: None,
            }
        } else {
            SessionActivity {
                running: false,
                awaiting_permission: false,
                finished_at: Some(now_ms),
            }
        };
        if next != prev {
            self.session_states.insert(session_id.to_owned(), next);
        }

        if self.active_session_id.as_deref() != Some(session_id) {
            return;
        }
        if running {
            self.is_running = true;
            return;
        }
        if let Some(usage) = turn_usage {
            let seconds = turn_ms.map(|ms| (ms / 1000.0).max(0.1));
            self.apply_turn_usage(session_id, usage, seconds);
        }
        if self.local_prompt_active {
            return;
        }
        self.is_running = false;
        self.cache_active_snapshot();
        self.flush_pending(now_ms);
    }

    // ---- pending queue ----

    /// Single flush point for the pending queue: after a local turn settles,
    /// after a restored (bridged) turn ends, and when an attach lands. The
    /// batch flush is synchronous, so concurrent callers stay safe.
    fn flush_pending(&mut self, now_ms: u64) {
        // The optimistic bubble inserted below must see every ARRIVED
        // update: a turn's trailing chunks precede its settle notification
        // on the wire but may still sit in the coalescing batch — inserting
        // first would land the queued message in the middle of the finished
        // turn.
        self.flush_update_batch(now_ms);
        if !self.conn_open {
            return;
        }
        let sid = match &self.active_session_id {
            Some(s) => s.clone(),
            None => return,
        };
        if self.loading_session || self.is_running {
            return;
        }
        let next = match self.pending_prompts.get(&sid).and_then(|q| q.first()) {
            Some(d) => d.clone(),
            None => return,
        };
        let rest = self
            .pending_prompts
            .get(&sid)
            .map(|q| q[1..].to_vec())
            .unwrap_or_default();
        self.set_queue(&sid, rest);
        self.run_prompt(now_ms, next);
    }

    /// Every queue mutation goes through here so callers can persist the map
    /// (drafts must survive switches/restarts). Empty queues drop the key.
    fn set_queue(&mut self, sid: &str, next: Vec<PromptDraft>) {
        if next.is_empty() {
            self.pending_prompts.remove(sid);
        } else {
            self.pending_prompts.insert(sid.to_owned(), next);
        }
    }

    /// ACP allows one prompt at a time: while a turn is running, history is
    /// still replaying, or the connection is down, new text queues as
    /// pending instead — the post-reconnect attach's load flushes it. The
    /// composer is cleared before this runs, so dropping instead of queueing
    /// would lose the draft for good.
    pub fn send_prompt(&mut self, now_ms: u64, text: &str, images: Vec<crate::chat::AttachmentDraft>) -> SendOutcome {
        let sid = match &self.active_session_id {
            Some(s) => s.clone(),
            None => return SendOutcome::NoSession,
        };
        let draft = PromptDraft {
            text: text.to_owned(),
            images,
        };
        if !self.conn_open || self.is_running || self.loading_session {
            let mut queue = self.pending_prompts.remove(&sid).unwrap_or_default();
            queue.push(draft);
            self.set_queue(&sid, queue);
            return SendOutcome::Queued;
        }
        self.run_prompt(now_ms, draft);
        SendOutcome::Ran
    }

    /// Inserts the optimistic user bubble (live turns never echo the user's
    /// message back — only replay does) and puts the prompt on the wire.
    /// Always a NEW message: two prompts sent back-to-back are two turns
    /// even when the first produced no assistant output before the queue
    /// flushed — appending to the previous user bubble merged them into one
    /// and garbled the transcript.
    fn run_prompt(&mut self, now_ms: u64, draft: PromptDraft) {
        let sid = match &self.active_session_id {
            Some(s) => s.clone(),
            None => return,
        };
        if !self.conn_open || self.loading_session {
            return;
        }
        let mut msg = self.factory.new_message(ChatRole::User, now_ms);
        let bubble_id = msg.id.clone();
        if !draft.text.is_empty() {
            append_text(&mut msg, &draft.text);
        }
        for img in &draft.images {
            // Display form is the data URL; the wire block stays base64.
            msg.parts.push(ChatPart::Image {
                image: format!("data:{};base64,{}", img.mime_type, img.data),
            });
        }
        self.messages.push(msg);

        let mut prompt: Vec<Value> = Vec::new();
        if !draft.text.is_empty() {
            prompt.push(json!({ "type": "text", "text": draft.text }));
        }
        for img in &draft.images {
            prompt.push(json!({
                "type": "image",
                "data": img.data,
                "mimeType": img.mime_type,
            }));
        }
        self.is_running = true;
        self.local_prompt_active = true;
        self.outbox.push(WireRequest::Prompt(PromptRequest {
            session_id: sid,
            prompt,
            started_at_ms: now_ms,
            bubble_id,
        }));
    }

    /// The prompt response landed: fold the turn usage, then run the shared
    /// settle (the TS `finally`).
    pub fn on_prompt_result(&mut self, now_ms: u64, req: &PromptRequest, result: &Value) {
        if let Some(usage) = result.get("usage").filter(|u| !u.is_null()) {
            let seconds = ((now_ms.saturating_sub(req.started_at_ms)) as f64 / 1000.0).max(0.1);
            self.apply_turn_usage(&req.session_id, usage, Some(seconds));
        }
        self.settle_prompt(now_ms);
    }

    /// The prompt request failed. Transport failures ("connection closed" /
    /// "connection not open") keep the drop-and-replay behavior — the turn
    /// may have run, and re-sending could duplicate it. Any OTHER failure is
    /// the server explicitly rejecting the prompt (compaction busy window,
    /// "a prompt is already running"): the turn definitely never ran, so the
    /// optimistic bubble is retracted and the draft goes back to the HEAD of
    /// the queue instead of vanishing — the next settle (the compaction's
    /// running:false, the next turn end) or a force-send flushes it. The
    /// rejected settle does NOT flush: a still-rejecting bridge would
    /// livelock the queue into a reject-resend loop.
    pub fn on_prompt_failed(&mut self, now_ms: u64, req: Option<&PromptRequest>, error: &str) {
        if let Some(req) = req.filter(|_| !is_transport_error(error)) {
            let sid = req.session_id.clone();
            self.messages.retain(|m| m.id != req.bubble_id);
            let mut queue = self.pending_prompts.remove(&sid).unwrap_or_default();
            queue.insert(0, draft_from_prompt(&req.prompt));
            self.set_queue(&sid, queue);
            self.local_prompt_active = false;
            self.is_running = false;
            self.cache_active_snapshot();
            return;
        }
        self.settle_prompt(now_ms);
    }

    /// The prompt settle: clear the local-turn flag, snapshot, and flush the
    /// queue (auto-flush: FIFO, each settled turn sends the next).
    fn settle_prompt(&mut self, now_ms: u64) {
        self.local_prompt_active = false;
        self.is_running = false;
        self.cache_active_snapshot();
        self.flush_pending(now_ms);
    }

    /// Interrupts the running turn so a queued follow-up goes out
    /// immediately. With an index, the targeted draft moves to the front
    /// first — that bubble's button sends THAT message, not the queue head.
    pub fn force_send_pending(&mut self, now_ms: u64, index: Option<usize>) -> ForceSendOutcome {
        let sid = match &self.active_session_id {
            Some(s) => s.clone(),
            None => return ForceSendOutcome::Refused,
        };
        let queue = match self.pending_prompts.get(&sid) {
            Some(q) if !q.is_empty() => q.clone(),
            _ => return ForceSendOutcome::Refused,
        };
        // During replay there is no turn to interrupt; the queue fires on
        // load. Disconnected: run_prompt would silently swallow the draft —
        // the post-reconnect flush owns the queue until the socket is back.
        if self.loading_session || !self.conn_open {
            return ForceSendOutcome::Refused;
        }
        let i = index.unwrap_or(0).min(queue.len() - 1);
        let mut reordered = vec![queue[i].clone()];
        reordered.extend(
            queue
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, d)| d.clone()),
        );
        if self.is_running {
            self.set_queue(&sid, reordered);
            self.outbox.push(WireRequest::CancelTurn {
                session_id: sid.clone(),
            });
            if self.local_prompt_active {
                // Own turn: the cancelled prompt's response settles here and
                // the settle-flush sends the reordered head.
                ForceSendOutcome::ReorderedAndCancelled
            } else {
                // Foreign/restored turn: its prompt response goes to the
                // connection that prompted it, and a turn-end broadcast may
                // never arrive here either — waiting for a settle dead-ends
                // the queue (force-send from an attached client never sent).
                // The cancel stops the turn; if it is still draining when the
                // prompt lands, the bridge preempts in-flight turns on
                // session/prompt anyway, so send the head immediately.
                self.is_running = false;
                self.cache_active_snapshot();
                self.flush_pending(now_ms);
                ForceSendOutcome::Sent
            }
        } else {
            let head = reordered.remove(0);
            self.set_queue(&sid, reordered);
            self.run_prompt(now_ms, head);
            ForceSendOutcome::Sent
        }
    }

    /// Queue a config pick (appStore `setConfigOption`): the tab toolbar's
    /// model / mode / thought write. Silently swallowed without a live
    /// session — the chips only render with one.
    pub fn set_config_option(&mut self, config_id: &str, value: &str) {
        let Some(session_id) = self.active_session_id.clone() else {
            return;
        };
        if !self.conn_open {
            return;
        }
        self.outbox.push(WireRequest::SetConfig {
            session_id,
            config_id: config_id.to_owned(),
            value: value.to_owned(),
        });
    }

    /// The set_config_option answer (appStore `setConfigOption`): a present
    /// `configOptions` array replaces the whole set — even an empty one, the
    /// TS `Array.isArray` guard verbatim.
    pub fn on_set_config_result(&mut self, result: &Value) {
        if let Some(arr) = result.get("configOptions").and_then(Value::as_array) {
            self.config_options = arr
                .iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect();
        }
    }

    /// Drops one queued draft.
    pub fn discard_pending(&mut self, index: usize) {
        let sid = match &self.active_session_id {
            Some(s) => s.clone(),
            None => return,
        };
        let queue = match self.pending_prompts.get(&sid) {
            Some(q) => q.clone(),
            None => return,
        };
        if index >= queue.len() {
            return;
        }
        let rest: Vec<PromptDraft> = queue
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != index)
            .map(|(_, d)| d.clone())
            .collect();
        self.set_queue(&sid, rest);
    }

    /// Returns a queued draft to the composer for editing: it leaves the
    /// queue and re-enters the input as a prefill (text + staged images).
    pub fn edit_pending(&mut self, index: usize) -> Option<PromptDraft> {
        let sid = self.active_session_id.clone()?;
        let queue = self.pending_prompts.get(&sid)?.clone();
        if index >= queue.len() {
            return None;
        }
        let draft = queue[index].clone();
        let rest: Vec<PromptDraft> = queue
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != index)
            .map(|(_, d)| d.clone())
            .collect();
        self.set_queue(&sid, rest);
        Some(draft)
    }

    // ---- load flows ----

    /// `session/load`: replay is the catch-up mechanism after any
    /// disconnect. A cache hit paints the snapshot right away (the composer
    /// stays locked via loading_session) and issues a metadata-only
    /// reconcile (limit 0) instead of replaying the tail from scratch.
    pub fn load_session(&mut self, session_id: &str) {
        if !self.conn_open {
            return;
        }
        // Replay arrives as session/update notifications; reset first.
        self.drop_queued_updates();
        let cached = self.cache.read(session_id);
        match &cached {
            Some(snap) => {
                self.messages = snap.messages.clone();
                self.plan_entries = snap.plan_entries.clone();
                self.replay_cursor = snap.replay_cursor.clone();
                self.has_more = snap.has_more;
                self.total_messages = snap.total_messages;
                self.config_options = snap.config_options.clone();
                self.current_mode_id = snap.current_mode_id.clone();
                self.usage = snap.usage;
                self.cache_hit = snap.cache_hit;
                self.turn_stats = snap.turn_stats;
            }
            None => {
                self.messages.clear();
                self.plan_entries = None;
                self.replay_cursor = None;
                self.has_more = false;
                self.total_messages = None;
                self.config_options.clear();
                self.current_mode_id = None;
                self.usage = None;
                self.cache_hit = None;
                self.turn_stats = None;
            }
        }
        self.active_session_id = Some(session_id.to_owned());
        self.available_commands.clear();
        self.loading_earlier = false;
        self.loading_session = true;
        self.is_running = false;

        self.load_seq += 1;
        let limit = if cached.is_some() {
            0
        } else {
            REPLAY_TAIL_LIMIT
        };
        self.pending_load = Some(PendingLoad {
            seq: self.load_seq,
            session_id: session_id.to_owned(),
            snapshot: cached,
        });
        self.outbox.push(WireRequest::Load(LoadRequest {
            session_id: session_id.to_owned(),
            limit,
            seq: self.load_seq,
        }));
    }

    /// A `session/load` response landed. For the reconcile attach (limit 0)
    /// this either keeps the painted snapshot (history unchanged) or clears
    /// it and issues the full tail replay; for the tail replay it applies
    /// the pagination metadata and restores a still-running turn.
    pub fn on_load_result(&mut self, now_ms: u64, req: &LoadRequest, result: &Value) {
        let pending = match self.pending_load.take() {
            Some(p) if p.seq == req.seq && p.session_id == req.session_id => p,
            _ => return, // superseded by a newer load
        };
        let meta: ReplayMeta = read_replay_meta(result).unwrap_or_default();
        let still_active = self.active_session_id.as_deref() == Some(req.session_id.as_str());

        if req.limit == 0 {
            // Reconcile the painted snapshot: "did anything happen while we
            // were gone". The tail id (bridge additive) is the primary key —
            // counts only refresh on replay, so a turn watched live leaves
            // the cached totalMessages stale and a count-only check would
            // force a full replay of an unchanged tail on every re-entry.
            // Ids compare verbatim: a thought-tailed turn has `thought_x` on
            // both sides. Bridges without the field, an id-less tail, or a
            // LOCAL optimistic tail fall back to the count check.
            let cached = pending.snapshot.expect("limit-0 loads issue from a cache hit");
            let server_tail = meta.last_message_id.as_ref();
            let cached_tail = if is_local_message_id(cached.last_message_id.as_deref()) {
                None
            } else {
                cached.last_message_id.as_ref()
            };
            let unchanged = match (server_tail, cached_tail) {
                (Some(s), Some(c)) => s == c,
                _ => meta.total_messages.is_some() && meta.total_messages == cached.total_messages,
            };
            let config_options = parse_config_options(result);
            let current_mode_id = parse_current_mode(result);
            if unchanged {
                if still_active {
                    // Unchanged: keep the painted messages/cursor/hasMore —
                    // they stay valid for load_earlier. The metadata-only
                    // cursor anchors at history's END (a different
                    // convention than the tail slice's START), so it must
                    // not be stored.
                    self.config_options = config_options.clone();
                    self.current_mode_id = current_mode_id.clone();
                    self.loading_session = false;
                    // A turn that survived the reconnect is still running on
                    // the bridge — restore the running UI, not the composer.
                    self.is_running = meta.turn_active == Some(true);
                }
                self.set_activity_running(req.session_id.as_str(), meta.turn_active == Some(true));
                let mut updated = cached;
                // Keep the count from rotting even though the id carried the
                // verdict — an old-bridge fallback reconcile reads it next.
                if let Some(t) = meta.total_messages {
                    updated.total_messages = Some(t);
                }
                updated.config_options = config_options;
                updated.current_mode_id = current_mode_id;
                self.cache.write(req.session_id.as_str(), updated);
                self.flush_pending(now_ms);
                return;
            }
            // History moved while away: drop the painted snapshot and replay
            // the tail wholesale — merging the replay stream into the cached
            // messages would double the chunk parts of every shared message.
            if still_active {
                self.messages.clear();
                self.plan_entries = None;
                self.replay_cursor = None;
                self.has_more = false;
                self.total_messages = None;
            }
            // Issue the full tail replay; its own response applies below.
            self.load_seq += 1;
            let session_id = req.session_id.clone();
            self.pending_load = Some(PendingLoad {
                seq: self.load_seq,
                session_id: session_id.clone(),
                snapshot: None,
            });
            self.outbox.push(WireRequest::Load(LoadRequest {
                session_id,
                limit: REPLAY_TAIL_LIMIT,
                seq: self.load_seq,
            }));
            return;
        }

        // Tail replay applied: pagination metadata + restored running turn.
        self.replay_cursor = meta.cursor;
        self.has_more = meta.has_more.unwrap_or(false);
        self.total_messages = meta.total_messages;
        self.config_options = parse_config_options(result);
        self.current_mode_id = parse_current_mode(result);
        self.loading_session = false;
        // A turn that survived the reconnect is still running on the bridge
        // (replayMeta.turnActive): restore the running UI instead of the
        // idle composer, which would let a prompt collide with the turn.
        self.is_running = meta.turn_active == Some(true);
        self.set_activity_running(req.session_id.as_str(), meta.turn_active == Some(true));
        self.cache_active_snapshot();
        // History is live: send whatever queued while the replay streamed
        // (flush_pending no-ops while a restored turn is running — its
        // turnState end event does the flushing then).
        self.flush_pending(now_ms);
    }

    /// A `session/load` failed. The in-flight batch AND the
    /// partially-applied replay are dropped: the wire may have delivered
    /// half the tail notifications before the failure, and flushing (or
    /// keeping) them paints a mid-conversation stub that reads as reordered
    /// history until the next reload. The reconnect replay is the only
    /// catch-up — until it lands, show nothing rather than a half. A failed
    /// metadata-only reconcile (limit 0) keeps the painted snapshot: it is a
    /// complete, self-consistent view, and the next reconnect retries the
    /// load. Superseded requests (a newer load is in flight) are ignored.
    pub fn on_load_failed(&mut self, req: &LoadRequest) {
        let matches = self
            .pending_load
            .as_ref()
            .map(|p| p.seq == req.seq && p.session_id == req.session_id)
            .unwrap_or(false);
        if matches {
            self.pending_load = None;
        }
        self.drop_queued_updates();
        self.loading_session = false;
        if req.limit == REPLAY_TAIL_LIMIT
            && self.active_session_id.as_deref() == Some(req.session_id.as_str())
        {
            self.messages.clear();
        }
    }

    /// Leaves the open session but keeps the instance connection semantics
    /// intact: the replay cache and pending queues survive so re-opening
    /// paints immediately.
    pub fn close_session(&mut self) {
        self.drop_queued_updates();
        self.active_session_id = None;
        self.messages.clear();
        self.plan_entries = None;
        self.replay_cursor = None;
        self.has_more = false;
        self.total_messages = None;
        self.loading_earlier = false;
        self.config_options.clear();
        self.current_mode_id = None;
        self.usage = None;
        self.cache_hit = None;
        self.turn_stats = None;
        self.available_commands.clear();
        self.loading_session = false;
        self.is_running = false;
    }

    // ---- load_earlier ----

    /// Fetches one page of older history. The stored cursor is passed back
    /// VERBATIM as `before` — the bridge owns cursor computation; the client
    /// never recomputes it.
    pub fn load_earlier(&mut self, now_ms: u64) {
        if !self.conn_open {
            return;
        }
        let sid = match &self.active_session_id {
            Some(s) => s.clone(),
            None => return,
        };
        // Mid-reconcile/replay: the cursor belongs to the about-to-be
        // replaced view; the page would land on the wrong history.
        let before = match &self.replay_cursor {
            Some(c) if self.has_more => c.clone(),
            _ => return,
        };
        if self.loading_session || self.loading_earlier {
            return;
        }
        self.loading_earlier = true;
        self.collecting_earlier = true;
        self.earlier_buffer.clear();
        self.earlier_session = Some(sid.clone());
        self.earlier_stream_last_at = now_ms;
        self.outbox.push(WireRequest::LoadEarlier(LoadEarlierRequest {
            session_id: sid,
            before,
            limit: EARLIER_PAGE_LIMIT,
        }));
    }

    /// The `session/load_earlier` response landed. On a marker-capable
    /// bridge the window closes now: stragglers carry the marker and take
    /// the late-page buffer (still a prepend). A legacy bridge cannot mark
    /// its pages and the response can overtake them on the wire — hold the
    /// window until the burst settles ([`EarlierOutcome::AwaitingQuiet`]).
    pub fn on_load_earlier_result(&mut self, now_ms: u64, result: &Value) -> EarlierOutcome {
        if self.page_marker_capable {
            self.collecting_earlier = false;
            EarlierOutcome::Applied(self.finish_load_earlier(now_ms, result))
        } else {
            self.earlier_cap_at = now_ms + EARLIER_WINDOW_CAP_MS;
            self.pending_earlier_result = Some(result.clone());
            EarlierOutcome::AwaitingQuiet
        }
    }

    /// One legacy window poll (the TS 200ms settle loop step). On the
    /// settling transition the buffered page assembles and prepends.
    pub fn poll_earlier_window(&mut self, now_ms: u64) -> EarlierPoll {
        if !self.collecting_earlier {
            return EarlierPoll::Idle;
        }
        let settled = now_ms.saturating_sub(self.earlier_stream_last_at) >= EARLIER_WINDOW_QUIET_MS
            || now_ms >= self.earlier_cap_at
            || self.local_prompt_active;
        if !settled {
            return EarlierPoll::StillCollecting;
        }
        self.collecting_earlier = false;
        let result = self.pending_earlier_result.take().unwrap_or(Value::Null);
        EarlierPoll::Applied(self.finish_load_earlier(now_ms, &result))
    }

    /// Builds the buffered page as its own oldest-first segment, then
    /// prepends it. plan/config/usage in old pages are stale: messages only.
    /// The response's cursor becomes the new pagination cursor (again
    /// verbatim — the server computed it).
    fn finish_load_earlier(&mut self, now_ms: u64, result: &Value) -> bool {
        let page = std::mem::take(&mut self.earlier_buffer);
        let requested_session = self.earlier_session.clone();
        let meta: ReplayMeta = read_replay_meta(result).unwrap_or_default();

        if let (Some(active), Some(requested)) = (&self.active_session_id, &requested_session) {
            if active == requested {
                let mut segment: Vec<ChatMessage> = Vec::new();
                for q in &page {
                    if q.session_id != *requested {
                        continue;
                    }
                    apply_one(
                        &mut segment,
                        &q.update,
                        q.meta.as_ref(),
                        &mut self.factory,
                        now_ms,
                    );
                }
                let mut rest = std::mem::take(&mut self.messages);
                if !segment.is_empty()
                    && !rest.is_empty()
                    && segment.last().map(|m| m.id.as_str()) == rest.first().map(|m| m.id.as_str())
                {
                    // Seam dedupe: the same message split across pages.
                    let seam = segment.pop().expect("segment non-empty");
                    let head = rest.remove(0);
                    let mut merged = seam;
                    merged.parts.extend(head.parts);
                    rest.insert(0, merged);
                }
                segment.extend(rest);
                self.messages = segment;
                self.replay_cursor = meta.cursor;
                self.has_more = meta.has_more.unwrap_or(false);
                self.loading_earlier = false;
            }
            // else: the store switched away mid-flight — the page's
            // messages/cursor land nowhere (the next load re-bases), exactly
            // the TS no-op branch.
        }
        self.cache_active_snapshot();
        true
    }

    /// The `session/load_earlier` request failed. "cursor expired" means
    /// history shrank (compaction): a fresh tail attach rebuilds from the
    /// metadata — `true` is returned when that reload was issued (the load
    /// request is on the outbox).
    pub fn on_load_earlier_failed(&mut self, session_id: &str, message: &str) -> bool {
        self.collecting_earlier = false;
        self.earlier_buffer.clear();
        self.pending_earlier_result = None;
        self.loading_earlier = false;
        if message.contains("cursor expired") {
            self.load_session(session_id);
            return true;
        }
        false
    }

    // ---- internals ----

    /// loadSession's activity write: only `running` is patched (a stale
    /// awaiting badge or finishedAt survives, matching the TS partial).
    fn set_activity_running(&mut self, session_id: &str, running: bool) {
        let prev = self
            .session_states
            .get(session_id)
            .cloned()
            .unwrap_or_default();
        if prev.running == running {
            return;
        }
        let mut next = prev;
        next.running = running;
        self.session_states.insert(session_id.to_owned(), next);
    }

    /// The approval slice's activity write (appStore.ts `setActivity(sid, {
    /// awaitingPermission })`): only `awaiting_permission` is patched — a
    /// pending permission/elicitation ask sets it, the answer / race loss /
    /// settle clears it.
    pub fn set_activity_awaiting(&mut self, session_id: &str, awaiting: bool) {
        let prev = self
            .session_states
            .get(session_id)
            .cloned()
            .unwrap_or_default();
        if prev.awaiting_permission == awaiting {
            return;
        }
        let mut next = prev;
        next.awaiting_permission = awaiting;
        self.session_states.insert(session_id.to_owned(), next);
    }
}

/// appStore's `cacheHitRate`: cachedRead / (input + cachedRead + cachedWrite)
/// — the ACP de-facto convention (inputTokens is cache-exclusive; the bridge
/// normalizes the backend's inclusive counts). None when the turn reported
/// no cache numbers at all (the caller keeps the previous rate then).
pub fn cache_hit_rate(usage: &Value) -> Option<u32> {
    if !usage.is_object() {
        return None;
    }
    let cached_read = usage.get("cachedReadTokens").and_then(Value::as_f64)?;
    let input = usage
        .get("inputTokens")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let write = usage
        .get("cachedWriteTokens")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let denom = input + cached_read + write;
    if denom > 0.0 {
        Some((cached_read / denom * 100.0).round() as u32)
    } else {
        None
    }
}

fn append_text(msg: &mut crate::chat::ChatMessage, text: &str) {
    crate::replay::append_text_part(msg, text, TextPartKind::Text);
}

/// Transport-level prompt failures: the request never got a server answer,
/// so the turn may or may not have run — replay decides, never a re-send.
/// The strings are AcpRequestError's two non-server shapes (acp_client.rs).
fn is_transport_error(error: &str) -> bool {
    error == "connection closed" || error == "connection not open"
}

/// Rebuild the queue draft from the wire prompt blocks `run_prompt` sent
/// (one text block, then image attachments).
fn draft_from_prompt(prompt: &[Value]) -> PromptDraft {
    let mut text = String::new();
    let mut images = Vec::new();
    for block in prompt {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(t);
                }
            }
            Some("image") => {
                let data = block.get("data").and_then(Value::as_str).unwrap_or_default();
                let mime_type = block
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !data.is_empty() {
                    images.push(AttachmentDraft {
                        mime_type: mime_type.to_owned(),
                        data: data.to_owned(),
                    });
                }
            }
            _ => {}
        }
    }
    PromptDraft { text, images }
}

fn parse_config_options(result: &Value) -> Vec<ConfigOption> {
    result
        .get("configOptions")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| serde_json::from_value::<ConfigOption>(v.clone()).ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn parse_current_mode(result: &Value) -> Option<String> {
    result
        .get("modes")
        .and_then(|m| m.get("currentModeId"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::AttachmentDraft;
    use serde_json::json;

    // =====================================================================
    // Equivalence anchors — ports of the TS app's vitest suites that pin
    // these semantics (tests/pending-queue.test.ts,
    // tests/pending-flush-race.test.ts, tests/cache-snapshot-race.test.ts,
    // tests/session-cache.test.ts, tests/earlier-page-routing.test.ts,
    // tests/earlier-page-fallback.test.ts, tests/earlier-late-page.test.ts,
    // tests/load-failure-stale-half.test.ts, tests/cache-hit.test.ts).
    // The FakeWebSocket doubles become direct drives: a held response is a
    // load request popped off the outbox and settled later; the 120ms
    // coalescing timer becomes an explicit flush_update_batch call.
    // =====================================================================

    fn text_update(kind: &str, mid: Option<&str>, text: &str) -> SessionUpdate {
        let mut rest = serde_json::Map::new();
        rest.insert("content".to_owned(), json!({ "type": "text", "text": text }));
        if let Some(mid) = mid {
            rest.insert("messageId".to_owned(), json!(mid));
        }
        SessionUpdate::with_rest(kind, rest)
    }

    /// Marks the update itself `_meta.zcode.earlierPage` (same placement as
    /// the tool-fold flags).
    fn marked(mut u: SessionUpdate) -> SessionUpdate {
        u.rest.insert(
            "_meta".to_owned(),
            json!({ "zcode": { "earlierPage": true } }),
        );
        u
    }

    #[test]
    fn set_config_option_guards_and_round_trip() {
        let mut core = SessionCore::new();

        // No live session: swallowed (the chips do not render without one).
        core.set_config_option("model", "gpt-5");
        assert!(core.take_wire().is_empty());

        core.conn_open = true;
        core.active_session_id = Some("sess_1".to_owned());
        core.set_config_option("model", "gpt-5");
        match core.take_wire().as_slice() {
            [WireRequest::SetConfig { session_id, config_id, value }] => {
                assert_eq!(
                    (session_id.as_str(), config_id.as_str(), value.as_str()),
                    ("sess_1", "model", "gpt-5")
                );
            }
            other => panic!("expected SetConfig, got {other:?}"),
        }

        // The answer's configOptions replace the whole set.
        core.on_set_config_result(&json!({
            "configOptions": [
                { "id": "model", "currentValue": "glm-4.7",
                  "options": [ { "value": "glm-4.7", "name": "GLM-4.7" } ] },
                { "id": "thought", "currentValue": "medium" }
            ]
        }));
        assert_eq!(core.config_options.len(), 2);
        assert_eq!(core.config_options[0].current_value.as_deref(), Some("glm-4.7"));

        // Absent key: no-op (a bare error object must not wipe the chips).
        core.on_set_config_result(&json!({ "error": "x" }));
        assert_eq!(core.config_options.len(), 2);
    }

    fn expect_load(core: &mut SessionCore) -> LoadRequest {
        let wire = core.take_wire();
        assert_eq!(wire.len(), 1, "expected one wire request, got {wire:?}");
        match wire.into_iter().next().expect("one request") {
            WireRequest::Load(req) => req,
            other => panic!("expected Load, got {other:?}"),
        }
    }

    fn expect_load_earlier(core: &mut SessionCore) -> LoadEarlierRequest {
        let wire = core.take_wire();
        assert_eq!(wire.len(), 1, "expected one wire request, got {wire:?}");
        match wire.into_iter().next().expect("one request") {
            WireRequest::LoadEarlier(req) => req,
            other => panic!("expected LoadEarlier, got {other:?}"),
        }
    }

    fn find_prompt(core: &mut SessionCore) -> PromptRequest {
        core.take_wire()
            .into_iter()
            .find_map(|w| match w {
                WireRequest::Prompt(p) => Some(p),
                _ => None,
            })
            .expect("a Prompt request on the wire")
    }

    fn idx_of(core: &SessionCore, needle: &str) -> Option<usize> {
        core.messages
            .iter()
            .position(|m| serde_json::to_string(&m.parts).unwrap().contains(needle))
    }

    fn ids(core: &SessionCore) -> Vec<String> {
        core.messages.iter().map(|m| m.id.clone()).collect()
    }

    fn queue_texts(core: &SessionCore, sid: &str) -> Vec<String> {
        core.pending_prompts
            .get(sid)
            .map(|q| q.iter().map(|d| d.text.clone()).collect())
            .unwrap_or_default()
    }

    fn usage_cached() -> Value {
        json!({
            "totalTokens": 10500,
            "inputTokens": 1000,
            "outputTokens": 500,
            "cachedReadTokens": 8000,
            "cachedWriteTokens": 1000,
        })
    }

    /// connectedStore(): open connection, marker-capable bridge, sess_1
    /// attached with an empty `{}` load result (no replayMeta).
    fn connected_core() -> SessionCore {
        let mut core = SessionCore::new();
        core.conn_open = true;
        core.on_initialize(Some("0.44.1"));
        core.load_session("sess_1");
        let req = expect_load(&mut core);
        assert_eq!(req.limit, REPLAY_TAIL_LIMIT);
        core.on_load_result(1000, &req, &json!({}));
        assert!(!core.loading_session);
        assert!(core.take_wire().is_empty());
        core
    }

    fn fresh_core_with_replay(result: Value) -> SessionCore {
        let mut core = SessionCore::new();
        core.conn_open = true;
        core.on_initialize(Some("0.44.1"));
        core.load_session("sess_1");
        let req = expect_load(&mut core);
        core.on_load_result(1000, &req, &result);
        assert!(!core.loading_session);
        core
    }

    /// earlier-late-page.test.ts's boot(): a tail of msg_x then msg_y loaded
    /// through a full replay carrying the pagination cursor.
    fn booted_with_tail(version: &str, tail_cursor: &str) -> SessionCore {
        let mut core = SessionCore::new();
        core.conn_open = true;
        core.on_initialize(Some(version));
        core.load_session("sess_1");
        let req = expect_load(&mut core);
        for (kind, mid, text) in [
            ("user_message_chunk", "msg_x", "recent q "),
            ("agent_message_chunk", "msg_x", "recent a "),
            ("user_message_chunk", "msg_y", "latest q "),
            ("agent_message_chunk", "msg_y", "latest a "),
        ] {
            core.on_update(1000, "sess_1", text_update(kind, Some(mid), text), None);
        }
        core.on_load_result(
            1001,
            &req,
            &json!({ "sessionId": "sess_1", "replayMeta": { "cursor": tail_cursor, "hasMore": true, "totalMessages": 10 } }),
        );
        assert_eq!(ids(&core), vec!["msg_x", "msg_y"]);
        core
    }

    /// The older page: two exchanges (msg_a older than msg_b), marked only on
    /// a marker-capable bridge.
    fn page_updates(marker: bool) -> Vec<SessionUpdate> {
        let rows = [
            ("user_message_chunk", "msg_a", "old q "),
            ("agent_message_chunk", "msg_a", "old a "),
            ("user_message_chunk", "msg_b", "older q "),
            ("agent_message_chunk", "msg_b", "older a "),
        ];
        rows.iter()
            .map(|(kind, mid, text)| {
                let u = text_update(kind, Some(mid), text);
                if marker { marked(u) } else { u }
            })
            .collect()
    }

    // ---- tests/pending-queue.test.ts ----

    #[test]
    fn prompt_sent_while_disconnected_parks_and_flushes_after_reconnect() {
        let mut core = connected_core();

        // The phone-slept state: the socket is gone but the session
        // selection survives for the respawn.
        core.conn_open = false;
        assert_eq!(
            core.send_prompt(2000, "typed while asleep", vec![]),
            SendOutcome::Queued
        );
        assert_eq!(queue_texts(&core, "sess_1"), vec!["typed while asleep"]);

        // Force-send while disconnected must NOT dequeue (run_prompt would
        // swallow the draft silently).
        assert_eq!(
            core.force_send_pending(2000, None),
            ForceSendOutcome::Refused
        );
        assert_eq!(core.pending_prompts["sess_1"].len(), 1);

        // Reconnect path: attach -> loadSession -> flush. The cached sess_1
        // paints and reconciles; `{}` again means no meta -> moved history ->
        // the full tail replay -> Done -> flush.
        core.conn_open = true;
        core.load_session("sess_1");
        let req0 = expect_load(&mut core);
        assert_eq!(req0.limit, 0);
        core.on_load_result(2100, &req0, &json!({}));
        let req30 = expect_load(&mut core);
        assert_eq!(req30.limit, REPLAY_TAIL_LIMIT);
        core.on_load_result(2200, &req30, &json!({}));
        let prompt = find_prompt(&mut core);
        assert!(serde_json::to_string(&prompt.prompt)
            .unwrap()
            .contains("typed while asleep"));
        assert!(core.pending_prompts.get("sess_1").is_none());
    }

    #[test]
    fn edit_pending_returns_the_draft_and_keeps_queue_order() {
        let mut core = connected_core();

        core.is_running = true;
        assert_eq!(
            core.send_prompt(
                2000,
                "first",
                vec![AttachmentDraft {
                    mime_type: "image/png".into(),
                    data: "aGk=".into(),
                }],
            ),
            SendOutcome::Queued
        );
        assert_eq!(core.send_prompt(2100, "second", vec![]), SendOutcome::Queued);
        core.is_running = false;

        let prefill = core.edit_pending(0).expect("draft returned");
        assert_eq!(queue_texts(&core, "sess_1"), vec!["second"]);
        assert_eq!(prefill.text, "first");
        assert_eq!(prefill.images.len(), 1);
        assert_eq!(prefill.images[0].data, "aGk=");
        assert_eq!(prefill.images[0].mime_type, "image/png");
    }

    #[test]
    fn force_send_pending_index_targets_that_bubble_not_the_queue_head() {
        let mut core = connected_core();

        // A runs (response held by the fake bridge); B and C queue behind it.
        assert_eq!(core.send_prompt(2000, "A", vec![]), SendOutcome::Ran);
        assert!(core.is_running);
        let req_a = find_prompt(&mut core);
        core.send_prompt(2100, "B", vec![]);
        core.send_prompt(2200, "C", vec![]);

        // Target the THIRD bubble: C moves to the front and the running turn
        // cancels; A's settle then flushes C out, B stays queued.
        assert_eq!(
            core.force_send_pending(2300, Some(2)),
            ForceSendOutcome::ReorderedAndCancelled
        );
        assert!(matches!(
            core.take_wire().as_slice(),
            [WireRequest::CancelTurn { .. }]
        ));
        assert_eq!(queue_texts(&core, "sess_1"), vec!["C", "B"]);

        core.on_prompt_result(2400, &req_a, &json!({}));
        let req_c = find_prompt(&mut core);
        assert!(serde_json::to_string(&req_c.prompt).unwrap().contains("C"));
        assert_eq!(queue_texts(&core, "sess_1"), vec!["B"]);
    }

    #[test]
    fn force_send_pending_flushes_now_under_a_foreign_turn() {
        let mut core = connected_core();

        // A restored/attached turn runs on ANOTHER connection: running flag
        // up, but no prompt of ours in flight — its settle (prompt response
        // or a cx-targeted turnState end) never reaches this client.
        core.is_running = true;
        assert_eq!(core.send_prompt(2100, "B", vec![]), SendOutcome::Queued);
        assert_eq!(core.send_prompt(2200, "C", vec![]), SendOutcome::Queued);

        // Force-send B: cancel the foreign turn AND put B on the wire now —
        // waiting for a settle that cannot arrive here dead-ended the queue.
        assert_eq!(core.force_send_pending(2300, None), ForceSendOutcome::Sent);
        let wire = core.take_wire();
        assert!(matches!(
            wire.as_slice(),
            [WireRequest::CancelTurn { .. }, WireRequest::Prompt(_)]
        ));
        let req_b = match wire.into_iter().nth(1) {
            Some(WireRequest::Prompt(p)) => p,
            other => panic!("expected B's prompt after the cancel, got {other:?}"),
        };
        assert!(core.is_running && core.local_prompt_active);
        assert_eq!(queue_texts(&core, "sess_1"), vec!["C"]);

        // The new turn is ours: its own settle flushes C and the queue
        // drains without any foreign signal.
        core.on_prompt_result(2400, &req_b, &json!({}));
        let req_c = find_prompt(&mut core);
        assert!(serde_json::to_string(&req_c.prompt).unwrap().contains("C"));
        assert!(core.pending_prompts.get("sess_1").is_none());
    }

    #[test]
    fn rejected_prompt_requeues_the_draft_instead_of_dropping_it() {
        let mut core = connected_core();

        // The prompt goes out (compaction busy window answers with a server
        // error — the turn never ran); B queues behind it.
        assert_eq!(core.send_prompt(2000, "keep me", vec![]), SendOutcome::Ran);
        let req = find_prompt(&mut core);
        let bubble_id = req.bubble_id.clone();
        core.send_prompt(2100, "queued behind", vec![]);

        core.on_prompt_failed(2200, Some(&req), "auto-compact busy");

        // The optimistic bubble is retracted, the draft back at the HEAD
        // (ahead of the queued follow-up), and NOTHING re-sent yet — the
        // next settle flushes it.
        assert!(core.messages.iter().all(|m| m.id != bubble_id));
        assert_eq!(queue_texts(&core, "sess_1"), vec!["keep me", "queued behind"]);
        assert!(!core.is_running && !core.local_prompt_active);
        assert!(core.take_wire().is_empty());

        // The compaction settles: the turn-end broadcast flushes the head.
        core.on_turn_state(2300, "sess_1", false, None);
        let req_retry = find_prompt(&mut core);
        assert!(serde_json::to_string(&req_retry.prompt).unwrap().contains("keep me"));
        assert_eq!(queue_texts(&core, "sess_1"), vec!["queued behind"]);
    }

    #[test]
    fn transport_failed_prompt_keeps_the_drop_and_replay_behavior() {
        let mut core = connected_core();

        assert_eq!(core.send_prompt(2000, "maybe ran", vec![]), SendOutcome::Ran);
        let req = find_prompt(&mut core);
        core.on_prompt_failed(2100, Some(&req), "connection closed");

        // No re-queue: the turn may have run — the reconnect replay rebuilds
        // the truth, a re-send could duplicate it.
        assert!(core.pending_prompts.get("sess_1").is_none());
        assert!(!core.is_running);
    }

    #[test]
    fn back_to_back_prompts_stay_separate_bubbles_even_with_no_assistant_turn_between() {
        let mut core = connected_core();

        // A goes out; B queues while A's turn is held open by the fake bridge.
        assert_eq!(core.send_prompt(2000, "A", vec![]), SendOutcome::Ran);
        let req_a = find_prompt(&mut core);
        assert!(core.is_running);
        assert_eq!(core.send_prompt(2100, "B", vec![]), SendOutcome::Queued);
        assert_eq!(core.pending_prompts["sess_1"].len(), 1);

        // A settles with ZERO assistant output: the flush must still open a
        // NEW user bubble for B — not append B onto A's bubble.
        core.on_prompt_result(2200, &req_a, &json!({}));
        let req_b = find_prompt(&mut core);
        // Let B's own settle land, then read the painted transcript.
        core.on_prompt_result(2300, &req_b, &json!({}));

        let users: Vec<&ChatMessage> = core
            .messages
            .iter()
            .filter(|m| m.role == ChatRole::User)
            .collect();
        assert_eq!(users.len(), 2);
        let first = serde_json::to_string(&users[0].parts).unwrap();
        let second = serde_json::to_string(&users[1].parts).unwrap();
        assert!(first.contains("A"));
        assert!(second.contains("B"));
        assert_ne!(users[0].id, users[1].id);
    }

    // ---- tests/pending-flush-race.test.ts ----

    #[test]
    fn queued_followup_bubble_must_land_after_the_running_turns_content() {
        let mut core = connected_core();

        // Turn A in flight (response held by the fake bridge).
        assert_eq!(core.send_prompt(2000, "A", vec![]), SendOutcome::Ran);
        let req_a = find_prompt(&mut core);
        assert!(core.is_running);

        // Queue B while the turn runs.
        assert_eq!(core.send_prompt(2100, "B", vec![]), SendOutcome::Queued);
        assert_eq!(queue_texts(&core, "sess_1"), vec!["B"]);

        // One wire burst — the bridge emits chunks, the turn-end notify, then
        // the prompt response back-to-back, all inside the coalescing window
        // (the chunks carry no messageId, so they merge into ONE assistant
        // message exactly like the TS fixture).
        core.on_update(
            2200,
            "sess_1",
            text_update("agent_message_chunk", None, "HEAD-OF-TURN-A"),
            None,
        );
        core.on_update(
            2201,
            "sess_1",
            text_update("agent_message_chunk", None, "TAIL-OF-TURN-A"),
            None,
        );
        core.on_turn_state(2202, "sess_1", false, None);
        core.on_prompt_result(2203, &req_a, &json!({}));

        // B was flushed and sent; let its own settle land.
        let req_b = find_prompt(&mut core);
        core.on_prompt_result(2210, &req_b, &json!({}));

        let user_a = idx_of(&core, "\"A\"").expect("A bubble");
        let user_b = idx_of(&core, "\"B\"").expect("B bubble");
        let head_a = idx_of(&core, "HEAD-OF-TURN-A").expect("head chunk");
        let tail_a = idx_of(&core, "TAIL-OF-TURN-A").expect("tail chunk");

        // B must be its own message (not merged into A's bubble), and it
        // must come after ALL of turn A's content.
        assert_eq!(user_a, 0);
        assert!(user_b > user_a);
        assert!(user_b > head_a);
        assert!(user_b > tail_a);
    }

    // ---- tests/cache-snapshot-race.test.ts ----

    #[test]
    fn queued_tail_chunk_when_the_load_response_lands_still_reaches_the_cache() {
        // First attach: full tail replay, held. EARLY flushes through the
        // coalescer; FINAL is queued and the response lands immediately
        // after — inside FINAL's coalescing window.
        let mut core = SessionCore::new();
        core.conn_open = true;
        core.on_initialize(Some("0.44.1"));
        core.load_session("sess_1");
        let req = expect_load(&mut core);
        assert_eq!(req.limit, REPLAY_TAIL_LIMIT);
        core.on_update(
            1000,
            "sess_1",
            text_update("agent_message_chunk", Some("m_EARLY"), "EARLY"),
            None,
        );
        core.flush_update_batch(1100); // EARLY past its flush tick
        assert!(idx_of(&core, "EARLY").is_some());
        core.on_update(
            1200,
            "sess_1",
            text_update("agent_message_chunk", Some("m_FINAL"), "FINAL"),
            None,
        ); // still queued when the response resolves
        core.on_load_result(
            1201,
            &req,
            &json!({ "replayMeta": { "cursor": "c0", "hasMore": false, "totalMessages": 2 } }),
        );
        assert!(!core.loading_session);

        // The live core applied FINAL through flush_pending's batch-first
        // seam...
        assert!(idx_of(&core, "FINAL").is_some());
        // ...and the cache (what a wake-reconnect paints) must agree with it.
        let cached = core.read_cache("sess_1").expect("snapshot cached");
        assert!(cached
            .messages
            .iter()
            .any(|m| serde_json::to_string(&m.parts).unwrap().contains("FINAL")));
        assert_eq!(cached.total_messages, Some(2));

        // Wake-reconnect shape: cache paint + metadata-only reconcile that
        // reports the SAME totalMessages (it counted FINAL all along) — the
        // painted view must still show FINAL.
        core.load_session("sess_1");
        assert!(core.loading_session);
        assert!(idx_of(&core, "FINAL").is_some()); // painted from the cache
        let req0 = expect_load(&mut core);
        assert_eq!(req0.limit, 0);
        core.on_load_result(
            1300,
            &req0,
            &json!({ "replayMeta": { "cursor": "end", "hasMore": false, "totalMessages": 2, "turnActive": false } }),
        );
        assert!(!core.loading_session);
        assert!(idx_of(&core, "FINAL").is_some());
        assert!(core.take_wire().is_empty()); // no tail replay was sent
    }

    // ---- tests/session-cache.test.ts ----

    #[test]
    fn cache_hit_paints_reconciles_metadata_only_moved_history_replays_and_survives_close() {
        let mut core = SessionCore::new();
        core.conn_open = true;
        core.on_initialize(Some("0.44.1"));

        // First attach: a full tail replay (limit 30) with one replayed
        // message.
        core.load_session("sess_1");
        let req30 = expect_load(&mut core);
        assert_eq!(req30.limit, REPLAY_TAIL_LIMIT);
        core.on_update(
            1000,
            "sess_1",
            text_update("agent_message_chunk", Some("m_SEED"), "SEED"),
            None,
        );
        core.on_load_result(
            1001,
            &req30,
            &json!({ "replayMeta": { "cursor": "c0", "hasMore": true, "totalMessages": 99 } }),
        );
        assert!(!core.loading_session);
        assert!(idx_of(&core, "SEED").is_some());

        // Wake-reconnect re-attach: the cached snapshot paints
        // synchronously...
        core.load_session("sess_1");
        assert!(core.loading_session);
        assert!(idx_of(&core, "SEED").is_some());
        let req0 = expect_load(&mut core);
        // ...then a metadata-only reconcile (limit 0) confirms nothing
        // changed.
        assert_eq!(req0.limit, 0);
        core.on_load_result(
            1100,
            &req0,
            &json!({
                "replayMeta": { "cursor": "end", "hasMore": true, "totalMessages": 99, "turnActive": false },
                "modes": { "currentModeId": "code" },
                "configOptions": [],
            }),
        );
        assert!(!core.loading_session);
        assert!(idx_of(&core, "SEED").is_some()); // zero replay: painted messages kept
        assert_eq!(core.total_messages, Some(99));
        assert!(core.take_wire().is_empty()); // no tail replay was sent

        // History moved while away: the painted snapshot is dropped and the
        // tail replays the traditional way.
        core.load_session("sess_1");
        let req0b = expect_load(&mut core);
        core.on_load_result(
            1200,
            &req0b,
            &json!({ "replayMeta": { "cursor": "end", "hasMore": true, "totalMessages": 100, "turnActive": false } }),
        );
        let req30b = expect_load(&mut core);
        assert_eq!(req30b.limit, REPLAY_TAIL_LIMIT);
        assert!(core.messages.is_empty()); // painted cache dropped
        core.on_update(
            1300,
            "sess_1",
            text_update("agent_message_chunk", Some("m_NEW"), "NEW"),
            None,
        );
        core.on_load_result(
            1301,
            &req30b,
            &json!({ "replayMeta": { "cursor": "c1", "hasMore": false, "totalMessages": 100 } }),
        );
        assert!(!core.loading_session);
        assert!(idx_of(&core, "NEW").is_some());
        assert!(idx_of(&core, "SEED").is_none()); // replaced by the fresh replay
        assert_eq!(core.total_messages, Some(100));

        // closeSession leaves the session view but keeps the cache:
        // re-opening paints immediately and reconciles metadata-only again.
        core.close_session();
        assert!(core.messages.is_empty());
        assert!(core.active_session_id.is_none());
        core.load_session("sess_1");
        assert!(idx_of(&core, "NEW").is_some()); // painted before any response
        let req0c = expect_load(&mut core);
        assert_eq!(req0c.limit, 0);
        core.on_load_result(
            1400,
            &req0c,
            &json!({ "replayMeta": { "cursor": "end", "hasMore": false, "totalMessages": 100, "turnActive": false } }),
        );
        assert!(!core.loading_session);
    }

    #[test]
    fn cache_is_lru_bounded_the_oldest_session_falls_back_to_a_full_replay() {
        let mut core = SessionCore::new();
        core.conn_open = true;
        core.on_initialize(Some("0.44.1"));
        core.load_session("sess_1");
        let req = expect_load(&mut core);
        core.on_load_result(
            1000,
            &req,
            &json!({ "replayMeta": { "cursor": "c", "hasMore": false, "totalMessages": 1 } }),
        );

        // Answer every load synchronously; each attach caches its session.
        for id in ["lr_1", "lr_2", "lr_3", "lr_4", "lr_5", "lr_6"] {
            core.load_session(id);
            let req = expect_load(&mut core);
            core.on_load_result(
                1100,
                &req,
                &json!({ "replayMeta": { "cursor": "c", "hasMore": false, "totalMessages": 1 } }),
            );
            assert!(!core.loading_session);
        }
        // Cache now holds sess_1 (oldest) + lr_1..lr_6 = 7 > 6: sess_1 was
        // evicted.
        assert!(core.cache_contains("lr_6"));
        assert!(!core.cache_contains("sess_1"));

        core.load_session("sess_1");
        let req = expect_load(&mut core);
        assert_eq!(req.limit, REPLAY_TAIL_LIMIT); // cache miss
        core.on_load_result(
            1200,
            &req,
            &json!({ "replayMeta": { "cursor": "c", "hasMore": false, "totalMessages": 1 } }),
        );
        assert!(!core.loading_session);
    }

    #[test]
    fn a_turn_watched_live_reenters_without_a_full_replay_tail_id_reconcile() {
        // Base: a fresh replay left totalMessages=1, messages empty.
        let mut core = fresh_core_with_replay(json!({
            "replayMeta": { "cursor": "c", "hasMore": false, "totalMessages": 1 }
        }));

        // A foreign turn runs while we watch: the live chunk lands, the turn
        // ends, and the end-of-turn snapshot caches it — with a
        // totalMessages count that still predates the turn.
        core.on_turn_state(2000, "sess_1", true, None);
        assert!(core.is_running);
        core.on_update(
            2001,
            "sess_1",
            text_update("agent_message_chunk", Some("m_LIVE1"), "LIVE1"),
            None,
        );
        core.flush_update_batch(2100); // the 120ms coalescer fires
        core.on_turn_state(2101, "sess_1", false, None);
        assert!(!core.is_running);
        assert!(idx_of(&core, "LIVE1").is_some());

        // Re-entry: the cache paints instantly; the metadata-only reconcile
        // reports the turn's messages (count moved) but the SAME tail id —
        // the id carries the verdict and the unchanged tail must NOT replay
        // from scratch.
        core.load_session("sess_1");
        assert!(idx_of(&core, "LIVE1").is_some()); // painted before any response
        let req0 = expect_load(&mut core);
        assert_eq!(req0.limit, 0);
        core.on_load_result(
            2200,
            &req0,
            &json!({ "replayMeta": { "cursor": "c", "hasMore": false, "totalMessages": 2, "lastMessageId": "m_LIVE1", "turnActive": false } }),
        );
        assert!(!core.loading_session);
        assert!(idx_of(&core, "LIVE1").is_some()); // kept, not dropped for a replay
        assert!(core.take_wire().is_empty()); // and no full replay was sent

        // Old bridge (no lastMessageId in the meta): the fallback is the
        // count check, and a moved count still forces the traditional full
        // replay.
        core.load_session("sess_1");
        let req0b = expect_load(&mut core);
        assert_eq!(req0b.limit, 0);
        core.on_load_result(
            2300,
            &req0b,
            &json!({ "replayMeta": { "cursor": "c", "hasMore": false, "totalMessages": 3, "turnActive": false } }),
        );
        let req30 = expect_load(&mut core);
        assert_eq!(req30.limit, REPLAY_TAIL_LIMIT); // full replay
        core.on_load_result(
            2400,
            &req30,
            &json!({ "replayMeta": { "cursor": "c3", "hasMore": false, "totalMessages": 3 } }),
        );
        assert!(!core.loading_session);
    }

    #[test]
    fn thought_tailed_turn_reconciles_on_the_raw_thought_id_local_tail_falls_back_to_count() {
        let mut core = fresh_core_with_replay(json!({
            "replayMeta": { "cursor": "c3", "hasMore": false, "totalMessages": 3 }
        }));

        // Foreign turn ending on a thought: the journal's newest entry — and
        // so the bridge's lastMessageId — is the thought's own
        // `thought_`-prefixed id.
        core.on_turn_state(2000, "sess_1", true, None);
        core.on_update(
            2001,
            "sess_1",
            text_update("agent_message_chunk", Some("m_t1"), "T1"),
            None,
        );
        core.on_update(
            2002,
            "sess_1",
            text_update("agent_thought_chunk", Some("thought_m_t1"), "TH1"),
            None,
        );
        core.on_turn_state(2100, "sess_1", false, None);
        assert!(idx_of(&core, "TH1").is_some());

        // Re-entry: both tails are the RAW thought id — equal ids keep the
        // painted snapshot. (The old prefix-stripping compare mismatched this
        // and forced a full tail replay on every re-entry.)
        core.load_session("sess_1");
        let req0 = expect_load(&mut core);
        assert_eq!(req0.limit, 0);
        core.on_load_result(
            2200,
            &req0,
            &json!({ "replayMeta": { "cursor": "c", "hasMore": false, "totalMessages": 2, "lastMessageId": "thought_m_t1", "turnActive": false } }),
        );
        assert!(!core.loading_session);
        assert!(idx_of(&core, "TH1").is_some()); // kept, not dropped for a replay
        assert!(core.take_wire().is_empty()); // and no full replay was sent

        // A turn whose only trace is the optimistic prompt bubble (cancelled
        // or failed before any backend content) leaves a LOCAL m-counter
        // tail id that can never match a backend id: the count check
        // decides — equal count keeps the snapshot, still no replay.
        assert_eq!(core.send_prompt(2300, "PROMPT", vec![]), SendOutcome::Ran);
        let req_p = find_prompt(&mut core);
        core.on_prompt_result(2310, &req_p, &json!({ "stopReason": "end_turn" }));
        assert!(!core.is_running);
        assert!(idx_of(&core, "PROMPT").is_some());

        core.load_session("sess_1");
        let req0b = expect_load(&mut core);
        core.on_load_result(
            2400,
            &req0b,
            &json!({ "replayMeta": { "cursor": "c", "hasMore": false, "totalMessages": 3, "lastMessageId": "m_t1", "turnActive": false } }),
        );
        assert!(!core.loading_session);
        assert!(idx_of(&core, "PROMPT").is_some()); // count check carried the verdict
        assert!(core.take_wire().is_empty()); // no full replay

        // ...but a moved count under a local tail still forces the full
        // replay.
        core.load_session("sess_1");
        let req0c = expect_load(&mut core);
        core.on_load_result(
            2500,
            &req0c,
            &json!({ "replayMeta": { "cursor": "c", "hasMore": false, "totalMessages": 9, "lastMessageId": "m_t1", "turnActive": false } }),
        );
        let req30 = expect_load(&mut core);
        assert_eq!(req30.limit, REPLAY_TAIL_LIMIT);
        core.on_load_result(
            2600,
            &req30,
            &json!({ "replayMeta": { "cursor": "c9", "hasMore": false, "totalMessages": 9 } }),
        );
        assert!(!core.loading_session);
    }

    // ---- tests/earlier-page-routing.test.ts ----

    #[test]
    fn live_updates_during_a_page_request_append_only_marked_updates_prepend() {
        let mut core = booted_with_tail("0.44.1", "c0");

        // Seed one live message (applies normally, before any paging).
        core.on_update(
            2000,
            "sess_1",
            text_update("agent_message_chunk", Some("m_SEED"), "SEED"),
            None,
        );
        core.flush_update_batch(2100);
        assert!(idx_of(&core, "SEED").is_some());

        // Fire the page request; it stays in flight until settled.
        core.load_earlier(2200);
        let req = expect_load_earlier(&mut core);
        assert_eq!(req.before, "c0"); // cursor passed back verbatim
        assert_eq!(req.limit, EARLIER_PAGE_LIMIT);

        // In flight: one MARKED page update + one UNMARKED live update. The
        // live one must NOT be captured into the page buffer (the pre-fix
        // gate buffered everything and prepended it on top of the running
        // turn).
        core.on_update(
            2210,
            "sess_1",
            marked(text_update(
                "agent_message_chunk",
                Some("m_OLD-PAGE"),
                "OLD-PAGE",
            )),
            None,
        );
        core.on_update(
            2211,
            "sess_1",
            text_update("agent_message_chunk", Some("m_LIVE-TAIL"), "LIVE-TAIL"),
            None,
        );

        assert_eq!(
            core.on_load_earlier_result(
                2212,
                &json!({ "replayMeta": { "cursor": "c1", "hasMore": false } })
            ),
            EarlierOutcome::Applied(true)
        );
        core.flush_update_batch(2213); // the live update's coalescing window
        assert_eq!(core.replay_cursor.as_deref(), Some("c1"));
        assert!(!core.has_more);
        assert!(!core.loading_earlier);

        let old = idx_of(&core, "OLD-PAGE").expect("page applied");
        let seed = idx_of(&core, "SEED").expect("seed present");
        let live = idx_of(&core, "LIVE-TAIL").expect("live tail present");
        // The page lands on top; the live update stays at the bottom.
        assert!(old < seed);
        assert!(live > seed);
    }

    // ---- tests/earlier-page-fallback.test.ts ----

    #[test]
    fn an_old_bridge_without_markers_buffers_the_whole_window() {
        let mut core = booted_with_tail("0.44.0", "c0");

        core.on_update(
            2000,
            "sess_1",
            text_update("agent_message_chunk", Some("m_SEED"), "SEED"),
            None,
        );
        core.flush_update_batch(2100);

        core.load_earlier(2200);
        let req = expect_load_earlier(&mut core);
        assert_eq!(req.before, "c0");

        // In flight: one MARKED page update + one UNMARKED live update — on
        // a legacy bridge BOTH ride the buffer (the pre-marker behavior).
        core.on_update(
            2210,
            "sess_1",
            marked(text_update(
                "agent_message_chunk",
                Some("m_OLD-PAGE"),
                "OLD-PAGE",
            )),
            None,
        );
        core.on_update(
            2211,
            "sess_1",
            text_update("agent_message_chunk", Some("m_LIVE-TAIL"), "LIVE-TAIL"),
            None,
        );

        assert_eq!(
            core.on_load_earlier_result(
                2212,
                &json!({ "replayMeta": { "cursor": "c1", "hasMore": false } })
            ),
            EarlierOutcome::AwaitingQuiet
        );
        // The collection window survives the response and settles only after
        // the burst goes quiet.
        assert_eq!(core.poll_earlier_window(2300), EarlierPoll::StillCollecting);
        assert_eq!(core.poll_earlier_window(2812), EarlierPoll::Applied(true));

        let old = idx_of(&core, "OLD-PAGE").expect("page applied");
        let live = idx_of(&core, "LIVE-TAIL").expect("live tail present");
        let seed = idx_of(&core, "SEED").expect("seed present");
        // Fallback: the whole window rode the buffer, so the page prepends
        // and the live update rides it on top of SEED.
        assert!(old < live);
        assert!(live < seed);
    }

    // ---- tests/earlier-late-page.test.ts ----

    #[test]
    fn ordered_pages_still_work_notifications_then_response() {
        // Marker-capable bridge: pages arrive before the response.
        let mut core = booted_with_tail("0.44.1", "c_tail");
        core.load_earlier(2000);
        let req = expect_load_earlier(&mut core);
        assert_eq!(req.before, "c_tail");
        for u in page_updates(true) {
            core.on_update(2001, "sess_1", u, None);
        }
        assert_eq!(
            core.on_load_earlier_result(
                2002,
                &json!({ "replayMeta": { "cursor": "c_page", "hasMore": false, "replayedMessages": 2 } })
            ),
            EarlierOutcome::Applied(true)
        );
        assert_eq!(ids(&core), vec!["msg_a", "msg_b", "msg_x", "msg_y"]);
        assert_eq!(core.replay_cursor.as_deref(), Some("c_page"));
        assert!(!core.loading_earlier);

        // Legacy bridge: the same ordered page, unmarked — the window settles
        // after the quiet period and still prepends.
        let mut core = booted_with_tail("0.43.0", "c_tail");
        core.load_earlier(2000);
        let _req = expect_load_earlier(&mut core);
        for u in page_updates(false) {
            core.on_update(2001, "sess_1", u, None);
        }
        assert_eq!(
            core.on_load_earlier_result(
                2002,
                &json!({ "replayMeta": { "cursor": "c_page", "hasMore": false, "replayedMessages": 2 } })
            ),
            EarlierOutcome::AwaitingQuiet
        );
        assert_eq!(core.poll_earlier_window(2603), EarlierPoll::Applied(true));
        assert_eq!(ids(&core), vec!["msg_a", "msg_b", "msg_x", "msg_y"]);
        assert_eq!(core.replay_cursor.as_deref(), Some("c_page"));
        assert!(!core.loading_earlier);
    }

    #[test]
    fn late_page_updates_prepend_before_the_tail_never_land_at_the_end() {
        let mut core = booted_with_tail("0.44.1", "c_tail");
        core.load_earlier(2000);
        let _req = expect_load_earlier(&mut core);

        // Response-first: the page applied as empty (nothing collected
        // in-window).
        assert_eq!(
            core.on_load_earlier_result(
                2001,
                &json!({ "replayMeta": { "cursor": "c_page", "hasMore": false, "replayedMessages": 2 } })
            ),
            EarlierOutcome::Applied(true)
        );
        assert_eq!(ids(&core), vec!["msg_x", "msg_y"]);

        // The page's marked updates arrive after the response — outside the
        // collection window. They must still PREPEND (older content, top),
        // not append after the newest messages.
        for u in page_updates(true) {
            core.on_update(2100, "sess_1", u, None);
        }
        core.flush_late_page(2300); // the 120ms late-page coalescer fires
        assert_eq!(ids(&core), vec!["msg_a", "msg_b", "msg_x", "msg_y"]);
    }

    #[test]
    fn legacy_bridge_unmarked_page_arriving_after_the_response_still_prepends() {
        let mut core = booted_with_tail("0.43.0", "c_tail");
        core.load_earlier(2000);
        let _req = expect_load_earlier(&mut core);

        // The response lands first (window stays open for a legacy bridge).
        assert_eq!(
            core.on_load_earlier_result(
                2001,
                &json!({ "replayMeta": { "cursor": "c_page", "hasMore": false } })
            ),
            EarlierOutcome::AwaitingQuiet
        );
        // The page's unmarked frames follow 300ms later.
        for u in page_updates(false) {
            core.on_update(2300, "sess_1", u, None);
        }
        // The settle window holds until the burst goes quiet, then prepends.
        assert_eq!(core.poll_earlier_window(2400), EarlierPoll::StillCollecting);
        assert_eq!(core.poll_earlier_window(2901), EarlierPoll::Applied(true));
        assert_eq!(ids(&core), vec!["msg_a", "msg_b", "msg_x", "msg_y"]);
    }

    #[test]
    fn seam_message_split_across_pages_merges_parts() {
        // The seam-dedupe branch of the page assembly: the same message id on
        // both sides of the page boundary merges its parts instead of
        // duplicating the message.
        let mut core = booted_with_tail("0.44.1", "c_tail");
        core.load_earlier(2000);
        let _req = expect_load_earlier(&mut core);
        // The page carries msg_x chunks (the seam with the loaded tail).
        core.on_update(
            2001,
            "sess_1",
            marked(text_update("user_message_chunk", Some("msg_x"), "older q ")),
            None,
        );
        core.on_update(
            2002,
            "sess_1",
            marked(text_update("agent_message_chunk", Some("msg_x"), "older a ")),
            None,
        );
        assert_eq!(
            core.on_load_earlier_result(
                2003,
                &json!({ "replayMeta": { "cursor": "c_page", "hasMore": false } })
            ),
            EarlierOutcome::Applied(true)
        );
        assert_eq!(ids(&core), vec!["msg_x", "msg_y"]); // still one msg_x
        let merged_parts = serde_json::to_string(&core.messages[0].parts).unwrap();
        assert!(merged_parts.contains("older q "));
        assert!(merged_parts.contains("recent q "));
    }

    #[test]
    fn earlier_failure_with_cursor_expired_rebuilds_from_a_fresh_tail_attach() {
        let mut core = booted_with_tail("0.44.1", "c0");
        core.load_earlier(2000);
        let _req = expect_load_earlier(&mut core);
        assert!(core.loading_earlier);

        // History shrank (compaction): the cursor is stale server-side.
        let reloaded = core.on_load_earlier_failed("sess_1", "load_earlier failed: cursor expired");
        assert!(reloaded);
        assert!(!core.loading_earlier);
        // A fresh tail attach is on the wire (cache hit -> limit 0).
        let req0 = expect_load(&mut core);
        assert_eq!(req0.limit, 0);
        core.on_load_result(
            2100,
            &req0,
            &json!({ "replayMeta": { "cursor": "c0", "hasMore": false, "totalMessages": 10, "turnActive": false } }),
        );
        assert!(!core.loading_session);
    }

    // ---- tests/load-failure-stale-half.test.ts ----

    #[test]
    fn a_mid_load_disconnect_must_not_keep_or_flush_the_partial_replay() {
        let mut core = SessionCore::new();
        core.conn_open = true;
        core.on_initialize(Some("0.44.1"));
        core.load_session("sess_1");
        let req = expect_load(&mut core);

        // The bridge streams part of the tail (three messages, four chunks
        // each), then the phone's link dies before the load response
        // arrives. The held request rejects transiently ("connection
        // closed" — notices are not ported, so the transient flag only
        // shaped the notice).
        for m in 1..=3u64 {
            for ch in 1..=4u64 {
                core.on_update(
                    1000 + m * 10 + ch,
                    "sess_1",
                    text_update(
                        "agent_message_chunk",
                        Some(&format!("msg_{m}")),
                        &format!("m{m}-chunk{ch} "),
                    ),
                    None,
                );
            }
        }
        core.on_load_failed(&req);

        // Let any armed coalescing timer fire — the exact window where the
        // stale half used to land.
        core.flush_update_batch(1100);
        assert!(!core.loading_session);
        // Nothing: the reconnect replay is the only catch-up. A stale half
        // reads as "conversation starts in the middle / reordered".
        assert!(core.messages.is_empty());
    }

    // ---- tests/cache-hit.test.ts ----

    #[test]
    fn a_prompt_response_with_cache_numbers_sets_cache_hit() {
        let mut core = fresh_core_with_replay(json!({ "sessionId": "sess_1" }));
        assert_eq!(core.cache_hit, None);

        assert_eq!(core.send_prompt(2000, "hi", vec![]), SendOutcome::Ran);
        let req = find_prompt(&mut core);
        core.on_prompt_result(
            2100,
            &req,
            &json!({ "stopReason": "end_turn", "usage": usage_cached() }),
        );
        // 8000 cached read / (1000 input + 8000 read + 1000 write) = 80%.
        assert_eq!(core.cache_hit, Some(80));

        // Reloading the session (switch away and back) keeps the readout:
        // the turn-end snapshot carries cacheHit, so the title bar does not
        // blank until the next locally-run turn.
        core.load_session("sess_1");
        assert_eq!(core.cache_hit, Some(80)); // painted from the cache
        let req0 = expect_load(&mut core);
        core.on_load_result(2200, &req0, &json!({ "sessionId": "sess_1" }));
        let req30 = expect_load(&mut core);
        core.on_load_result(2300, &req30, &json!({ "sessionId": "sess_1" }));
        assert_eq!(core.cache_hit, Some(80));
    }

    #[test]
    fn the_same_response_feeds_the_turn_speed_readout() {
        let mut core = fresh_core_with_replay(json!({ "sessionId": "sess_1" }));

        assert_eq!(core.send_prompt(2000, "again", vec![]), SendOutcome::Ran);
        let req = find_prompt(&mut core);
        // The fake bridge settles within one tick: the 0.1s floor applies —
        // tps is the floor-divided rate, seconds the floored wall time.
        core.on_prompt_result(
            2000,
            &req,
            &json!({ "stopReason": "end_turn", "usage": usage_cached() }),
        );
        let ts = core.turn_stats.expect("turnStats set");
        assert_eq!(ts.output_tokens, 500);
        assert!(ts.seconds >= 0.1);
        assert!(ts.tps > 0.0);

        // Switching sessions keeps the readout (snapshot restore).
        core.load_session("sess_1");
        assert_eq!(core.turn_stats.map(|t| t.output_tokens), Some(500));
    }

    #[test]
    fn a_foreign_turns_end_broadcast_feeds_the_readouts() {
        let mut core = fresh_core_with_replay(json!({ "sessionId": "sess_1" }));

        // A turn driven from ANOTHER client: the app only sees the
        // turn-state broadcast, never a prompt response of its own.
        core.on_turn_state(2000, "sess_1", true, None);
        assert!(core.is_running);
        core.on_turn_state(
            2100,
            "sess_1",
            false,
            Some(&json!({
                "usage": {
                    "totalTokens": 10500,
                    "inputTokens": 1000,
                    "outputTokens": 200,
                    "cachedReadTokens": 8000,
                    "cachedWriteTokens": 1000
                },
                "turnMs": 4000
            })),
        );
        // 8000 / (1000 + 8000 + 1000) = 80%; 200 tokens over 4s = 50 tok/s.
        assert_eq!(core.cache_hit, Some(80));
        let ts = core.turn_stats.expect("turnStats set");
        assert_eq!(ts.tps, 50.0);
        assert_eq!(ts.output_tokens, 200);
        assert_eq!(ts.seconds, 4.0);
        assert!(!core.is_running);
    }
}
