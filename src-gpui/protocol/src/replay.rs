//! Replay projection helpers — port of `src/store/replay.ts`.
//!
//! Pure `session/update -> chat model` projection, replay pagination
//! metadata, and the per-session replay cache. No I/O lives here: everything
//! operates on the values it is handed. The companion state machine that
//! drives these helpers (load flows, earlier-page collection, pending queue,
//! snapshot racing) is [`crate::session::SessionCore`].
//!
//! Wire doctrine (inherited from the TS app): unknown update kinds and fields
//! are ignored, never rejected. Optional fields stay optional; `_meta` flags
//! (`zcode.collapsed`, `zcode.earlierPage`) ride either on the update itself
//! or on the notification envelope — both placements are accepted.

use serde_json::Value;

use crate::acp::{
    content_diff_blocks, content_text, ConfigOption, ContextUsage, SessionUpdate, SlashCommand,
};
use crate::chat::{ChatMessage, ChatPart, ChatRole, ToolCallPart, TurnStats};

/// Server-side tail replay (REMOTE-CLIENTS.md "Tail replay and history
/// pagination"): attach ships only the last [`REPLAY_TAIL_LIMIT`] messages;
/// older pages arrive via `session/load_earlier`.
pub const REPLAY_TAIL_LIMIT: u32 = 30;
pub const EARLIER_PAGE_LIMIT: u32 = 50;
/// Per-session replay cache bound (LRU): a wake-reconnect or a switch back
/// paints the snapshot instantly and reconciles with a metadata-only attach.
const SESSION_CACHE_LIMIT: usize = 6;

/// One todo/plan entry; `status` is "pending" | "active" | "completed".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    pub content: String,
    pub status: Option<String>,
}

/// Cached view of one session, painted instantly on re-entry (the snapshot a
/// metadata-only attach then reconciles against). Counts only refresh on
/// replay, so `total_messages` can trail `last_message_id` after a turn
/// watched live — the tail id is the primary reconcile key.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionSnapshot {
    pub messages: Vec<ChatMessage>,
    pub plan_entries: Option<Vec<PlanEntry>>,
    pub replay_cursor: Option<String>,
    pub has_more: bool,
    pub total_messages: Option<u64>,
    /// Tail message id (see [`snapshot_tail_id`]) — the primary reconcile key.
    pub last_message_id: Option<String>,
    pub config_options: Vec<ConfigOption>,
    pub current_mode_id: Option<String>,
    pub usage: Option<ContextUsage>,
    /// Last turn's readouts ride the snapshot too: without them a switch or
    /// wake-reconnect blanks the title-bar cache/tps segments until the next
    /// locally-run turn.
    pub cache_hit: Option<u32>,
    pub turn_stats: Option<TurnStats>,
}

/// LRU cache of session snapshots (back = most recently used). Reads refresh
/// recency so eviction order reflects use, exactly like the TS `Map`.
#[derive(Debug, Default)]
pub struct SessionCache {
    entries: Vec<(String, SessionSnapshot)>,
}

impl SessionCache {
    pub fn read(&mut self, session_id: &str) -> Option<SessionSnapshot> {
        let idx = self.entries.iter().position(|(k, _)| k == session_id)?;
        let (_, snap) = self.entries.remove(idx);
        self.entries.push((session_id.to_owned(), snap.clone()));
        Some(snap)
    }

    pub fn write(&mut self, session_id: &str, snap: SessionSnapshot) {
        self.entries.retain(|(k, _)| k != session_id);
        self.entries.push((session_id.to_owned(), snap));
        while self.entries.len() > SESSION_CACHE_LIMIT {
            self.entries.remove(0);
        }
    }

    pub fn remove(&mut self, session_id: &str) {
        self.entries.retain(|(k, _)| k != session_id);
    }

    pub fn contains(&self, session_id: &str) -> bool {
        self.entries.iter().any(|(k, _)| k == session_id)
    }
}

/// Id of the newest chat message — the snapshot-side reconcile key. Compared
/// VERBATIM against the bridge's `replayMeta.lastMessageId`: that field is
/// the newest journal entry's id as-is, and a thought IS its own journal
/// entry with a `thought_`-prefixed id — so a turn ending on a thought has
/// `thought_x` on both sides. Stripping the prefix here (the old behavior)
/// mismatched exactly that case and forced a redundant full replay on every
/// re-entry of the session.
pub fn snapshot_tail_id(messages: &[ChatMessage]) -> Option<String> {
    messages.last().map(|m| m.id.clone())
}

/// Local optimistic message ids ([`MessageFactory`]'s `m1`, `m2`, … counter).
/// They can never equal a backend/bridge id (bridge-generated ids are UUIDs),
/// so a local tail id carries no reconcile signal — the caller falls back to
/// the count check. Mirrors the TS regex `/^m\d+$/`.
pub fn is_local_message_id(id: Option<&str>) -> bool {
    match id {
        Some(s) => {
            let bytes = s.as_bytes();
            bytes.len() >= 2
                && bytes[0] == b'm'
                && bytes[1..].iter().all(|b| b.is_ascii_digit())
        }
        None => false,
    }
}

/// Pagination metadata from a `session/load` / `session/load_earlier`
/// response. Field-level Optionals mirror the TS typeof guards at use sites;
/// a missing or wrongly-typed field reads as absent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReplayMeta {
    pub cursor: Option<String>,
    pub has_more: Option<bool>,
    pub total_messages: Option<u64>,
    /// Bridge additive: id of the newest message at the slice's end anchor.
    /// Preferred reconcile key over `totalMessages` (which drifts stale after
    /// turns watched live); absent on older bridges.
    pub last_message_id: Option<String>,
    /// Bridge flag: a turn is still in flight for this session (someone
    /// else's prompt, or one that survived our reconnect) — restore the
    /// running UI.
    pub turn_active: Option<bool>,
}

fn opt_str(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::to_owned)
}

fn opt_bool(v: Option<&Value>) -> Option<bool> {
    v.and_then(Value::as_bool)
}

fn opt_u64(v: Option<&Value>) -> Option<u64> {
    v.and_then(Value::as_u64)
}

/// Reads `result.replayMeta`; None when the result is not an object or has
/// no `replayMeta` object.
pub fn read_replay_meta(result: &Value) -> Option<ReplayMeta> {
    let meta = result.get("replayMeta")?;
    if !meta.is_object() {
        return None;
    }
    Some(ReplayMeta {
        cursor: opt_str(meta.get("cursor")),
        has_more: opt_bool(meta.get("hasMore")),
        total_messages: opt_u64(meta.get("totalMessages")),
        last_message_id: opt_str(meta.get("lastMessageId")),
        turn_active: opt_bool(meta.get("turnActive")),
    })
}

/// zcode-acp-server version that started marking load_earlier page updates
/// `_meta.zcode.earlierPage` — the floor for the marker-based page routing.
const PAGE_MARKER_BRIDGE_VERSION: (i64, i64, i64) = (0, 44, 1);

/// `Number.parseInt(s, 10)` semantics: skip leading whitespace, optional
/// sign, then digits; anything else (or nothing) is NaN -> None.
fn parse_int_prefix(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let bytes = t.as_bytes();
    let mut i = 0;
    let mut sign = 1i64;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        if bytes[i] == b'-' {
            sign = -1;
        }
        i += 1;
    }
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None;
    }
    t[start..i].parse::<i64>().ok().map(|n| sign * n)
}

/// Semver-ish floor check on the bridge's advertised agentInfo.version.
/// Unparseable versions read as NOT capable: the fallback (pre-marker
/// buffer-everything collection) is the safe behavior on any bridge.
pub fn bridge_supports_page_marker(version: Option<&str>) -> bool {
    let version = match version {
        Some(v) => v,
        None => return false,
    };
    let parts: Vec<&str> = version.split('.').collect();
    if parts.len() < 3 {
        return false;
    }
    let mut nums = [0i64; 3];
    for (i, p) in parts.iter().take(3).enumerate() {
        match parse_int_prefix(p) {
            Some(n) => nums[i] = n,
            None => return false,
        }
    }
    let floor = PAGE_MARKER_BRIDGE_VERSION;
    for i in 0..3 {
        let (n, f) = match i {
            0 => (nums[0], floor.0),
            1 => (nums[1], floor.1),
            _ => (nums[2], floor.2),
        };
        if n != f {
            return n > f;
        }
    }
    true
}

/// Optimistic message ids for locally inserted bubbles. The counter is shared
/// between replay and live insertion so the two id spaces never collide.
#[derive(Debug, Default)]
pub struct MessageFactory {
    counter: u64,
}

impl MessageFactory {
    pub fn new_message(&mut self, role: ChatRole, now_ms: u64) -> ChatMessage {
        self.counter += 1;
        ChatMessage {
            id: format!("m{}", self.counter),
            role,
            parts: Vec::new(),
            created_at: now_ms,
            collapsed: None,
        }
    }
}

/// Which trailing part kind [`append_text_part`] merges into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextPartKind {
    Text,
    Thought,
}

/// Fresh bottom message sharing the counter with replay, so live and replayed
/// ids never collide. Returns the id of the trailing same-role message when
/// one exists (the TS `ensureMessage` merge), inserting a new message
/// otherwise.
pub fn ensure_message(
    msgs: &mut Vec<ChatMessage>,
    role: ChatRole,
    ids: &mut MessageFactory,
    now_ms: u64,
) -> String {
    if let Some(last) = msgs.last() {
        if last.role == role {
            return last.id.clone();
        }
    }
    let message = ids.new_message(role, now_ms);
    let id = message.id.clone();
    msgs.push(message);
    id
}

/// Appends text to the message, merging into the trailing part when it is the
/// same kind, pushing a new part otherwise.
pub fn append_text_part(message: &mut ChatMessage, text: &str, part_kind: TextPartKind) {
    let merges = match (message.parts.last(), part_kind) {
        (Some(ChatPart::Text { .. }), TextPartKind::Text)
        | (Some(ChatPart::Thought { .. }), TextPartKind::Thought) => true,
        _ => false,
    };
    if merges {
        match message.parts.last_mut().expect("checked above") {
            ChatPart::Text { text: t } => t.push_str(text),
            ChatPart::Thought { text: t } => t.push_str(text),
            _ => unreachable!(),
        }
        return;
    }
    let part = match part_kind {
        TextPartKind::Text => ChatPart::Text {
            text: text.to_owned(),
        },
        TextPartKind::Thought => ChatPart::Thought {
            text: text.to_owned(),
        },
    };
    message.parts.push(part);
}

/// With a messageId, chunks group into that exact message (backend ids are
/// stable and dedupe across replay pages; thought ids carry a `thought_`
/// prefix and form their own stream). Without one, fall back to merging into
/// the trailing same-role message.
pub fn append_chunk_message(
    msgs: &mut Vec<ChatMessage>,
    u: &SessionUpdate,
    role: ChatRole,
    text: &str,
    part_kind: TextPartKind,
    ids: &mut MessageFactory,
    now_ms: u64,
) {
    let mid = u
        .rest
        .get("messageId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    if let Some(mid) = mid {
        if let Some(i) = msgs.iter().position(|m| m.id == mid) {
            append_text_part(&mut msgs[i], text, part_kind);
            return;
        }
        let part = match part_kind {
            TextPartKind::Text => ChatPart::Text {
                text: text.to_owned(),
            },
            TextPartKind::Thought => ChatPart::Thought {
                text: text.to_owned(),
            },
        };
        msgs.push(ChatMessage {
            id: mid.to_owned(),
            role,
            parts: vec![part],
            created_at: now_ms,
            collapsed: None,
        });
        return;
    }
    let target = ensure_message(msgs, role, ids, now_ms);
    if let Some(m) = msgs.iter_mut().find(|m| m.id == target) {
        append_text_part(m, text, part_kind);
    }
}

/// `_meta.zcode.collapsed` on a replayed user chunk (context handoff and
/// similar harness blocks) — the UI renders those behind an expand control.
/// Accepts the notification-level meta envelope.
pub fn is_collapsed_meta(meta: Option<&Value>) -> bool {
    meta.and_then(|m| m.get("zcode"))
        .map(|z| z.get("collapsed") == Some(&Value::Bool(true)))
        .unwrap_or(false)
}

/// `_meta.zcode.earlierPage` on session/load_earlier paged updates — the
/// bridge marks page replays so live-turn updates streaming while the page
/// request is in flight can be told apart (they must append, not ride the
/// prepend). Accepts either the update-level or the notification-level meta.
pub fn is_earlier_page_meta(meta: Option<&Value>) -> bool {
    meta.and_then(|m| m.get("zcode"))
        .map(|z| z.get("earlierPage") == Some(&Value::Bool(true)))
        .unwrap_or(false)
}

/// JS `String(v)` for the primitives these wire fields carry; missing/null
/// maps to the caller's default.
fn js_string_or(v: Option<&Value>, default: &str) -> String {
    match v {
        None | Some(Value::Null) => default.to_owned(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => default.to_owned(),
    }
}

/// Locates the tool_call part the bridge emits right before an interaction
/// request; its content holds the plan/question text (ADR 0003). Newest
/// assistant message wins.
pub fn find_tool_call_part(msgs: &[ChatMessage], tool_call_id: &str) -> Option<ToolCallPart> {
    let (mi, pi) = find_tool_call_part_index(msgs, tool_call_id)?;
    match &msgs[mi].parts[pi] {
        ChatPart::ToolCall(t) => Some(t.clone()),
        _ => None,
    }
}

/// Same lookup, but returns message/part indices so the caller can replace
/// the part in place (tool_call dedupe on insert). Empty ids never match.
pub fn find_tool_call_part_index(msgs: &[ChatMessage], tool_call_id: &str) -> Option<(usize, usize)> {
    if tool_call_id.is_empty() {
        return None;
    }
    for (i, m) in msgs.iter().enumerate().rev() {
        if m.role != ChatRole::Assistant {
            continue;
        }
        for (j, p) in m.parts.iter().enumerate().rev() {
            if let ChatPart::ToolCall(t) = p {
                if t.tool_call_id == tool_call_id {
                    return Some((i, j));
                }
            }
        }
    }
    None
}

/// What one applied update changed. Each [`apply_one`] branch produces at
/// most one of these (exactly the TS single-key patch objects); `Nothing` is
/// the TS `null` return.
#[derive(Debug, Clone, PartialEq)]
pub enum ApplyPatch {
    /// `msgs` was mutated in place (message kinds).
    Messages,
    /// Plan is a full snapshot; `None` payload clears it (empty entry list).
    Plan(Option<Vec<PlanEntry>>),
    ConfigOptions(Vec<ConfigOption>),
    CurrentMode(String),
    Usage(ContextUsage),
    /// Full snapshot, overwrite semantics (the bridge re-sends after each
    /// session/load).
    AvailableCommands(Vec<SlashCommand>),
    Nothing,
}

/// Applies one update to the message list in place; returns what else
/// changed. Message kinds operate on `msgs`; session-level kinds carry their
/// own values. Unknown kinds (additive-only contract) are ignored.
pub fn apply_one(
    msgs: &mut Vec<ChatMessage>,
    u: &SessionUpdate,
    meta: Option<&Value>,
    ids: &mut MessageFactory,
    now_ms: u64,
) -> ApplyPatch {
    let kind = u.session_update.as_str();
    let rest = &u.rest;

    if kind == "user_message_chunk" || kind == "agent_message_chunk" {
        let role = if kind == "user_message_chunk" {
            ChatRole::User
        } else {
            ChatRole::Assistant
        };
        let content = rest.get("content").cloned().unwrap_or(Value::Null);
        let text = content_text(&content);
        if text.is_empty() {
            return ApplyPatch::Nothing;
        }
        append_chunk_message(msgs, u, role, &text, TextPartKind::Text, ids, now_ms);
        if role == ChatRole::User && is_collapsed_meta(meta) {
            let mid = rest.get("messageId").and_then(Value::as_str).unwrap_or("");
            if let Some(m) = msgs
                .iter_mut()
                .find(|m| m.id == mid && m.collapsed != Some(true))
            {
                m.collapsed = Some(true);
            }
        }
        return ApplyPatch::Messages;
    }
    if kind == "agent_thought_chunk" {
        let content = rest.get("content").cloned().unwrap_or(Value::Null);
        let text = content_text(&content);
        if text.is_empty() {
            return ApplyPatch::Nothing;
        }
        append_chunk_message(
            msgs,
            u,
            ChatRole::Assistant,
            &text,
            TextPartKind::Thought,
            ids,
            now_ms,
        );
        return ApplyPatch::Messages;
    }
    if kind == "tool_call" {
        let update_meta = rest.get("_meta");
        let claude_meta = update_meta.and_then(|m| m.get("claudeCode"));
        let zcode_meta = update_meta.and_then(|m| m.get("zcode"));
        // Replay harness folds (server 0.6.0): handoff summaries and
        // rewritten tool transcripts arrive as completed tool_calls flagged
        // in _meta.zcode.
        let fold = match zcode_meta {
            Some(z) if z.get("collapsed") == Some(&Value::Bool(true)) => {
                opt_str(z.get("kind"))
            }
            _ => None,
        };
        let content = rest.get("content").cloned().unwrap_or(Value::Null);
        let diffs = content_diff_blocks(&content);
        // The bridge ships the plan/question text as the tool_call's initial
        // content — keep it in detail so approval cards can show it.
        let part = ToolCallPart {
            tool_call_id: js_string_or(rest.get("toolCallId"), ""),
            tool_name: js_string_or(rest.get("title"), "tool"),
            detail: content_text(&content),
            status: js_string_or(rest.get("status"), "pending"),
            kind: opt_str(rest.get("kind")),
            raw_name: opt_str(claude_meta.and_then(|c| c.get("toolName"))),
            fold_kind: fold,
            diffs: if diffs.is_empty() { None } else { Some(diffs) },
        };
        // Dedupe on every insert (REPLAY-GUIDE contract): a reannounced
        // interaction (e.g. a still-pending plan approval) reuses its
        // toolCallId, and replay can deliver the same historical call again.
        // Replace the existing part in place instead of appending a second.
        if let Some((mi, pi)) = find_tool_call_part_index(msgs, &part.tool_call_id) {
            msgs[mi].parts[pi] = ChatPart::ToolCall(part);
            return ApplyPatch::Messages;
        }
        let mid = rest
            .get("messageId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        if let Some(mid) = mid {
            if let Some(i) = msgs.iter().position(|m| m.id == mid) {
                msgs[i].parts.push(ChatPart::ToolCall(part));
            } else {
                msgs.push(ChatMessage {
                    id: mid.to_owned(),
                    role: ChatRole::Assistant,
                    parts: vec![ChatPart::ToolCall(part)],
                    created_at: now_ms,
                    collapsed: None,
                });
            }
            return ApplyPatch::Messages;
        }
        let target = ensure_message(msgs, ChatRole::Assistant, ids, now_ms);
        if let Some(m) = msgs.iter_mut().find(|m| m.id == target) {
            m.parts.push(ChatPart::ToolCall(part));
        }
        return ApplyPatch::Messages;
    }
    if kind == "tool_call_update" {
        let tool_call_id = js_string_or(rest.get("toolCallId"), "");
        let content = rest.get("content");
        let owned = content.cloned().unwrap_or(Value::Null);
        let chunk = content_text(&owned);
        let diff_blocks = content_diff_blocks(&owned);
        // Terminal-channel Bash (enabled while an editor client is attached):
        // output streams as _meta.terminal_output.data deltas — append them
        // so the card keeps its output; the terminal_exit update then only
        // closes.
        let term_text = rest
            .get("_meta")
            .and_then(|m| m.get("terminal_output"))
            .and_then(|t| t.get("data"))
            .and_then(Value::as_str);
        // Non-terminal tools may carry a rawOutput string without content
        // blocks.
        let raw_out = rest.get("rawOutput").and_then(Value::as_str);
        // An update carrying content blocks REPLACES the whole collection
        // (text and diffs); a status-only update leaves them untouched.
        let has_content = content
            .map(|c| {
                !c.is_null()
                    && (!c.is_array() || c.as_array().map(|a| !a.is_empty()).unwrap_or(false))
            })
            .unwrap_or(false);
        let status = opt_str(rest.get("status"));
        // Search backwards: the tool call may live in an earlier assistant
        // message when other clients' turns interleaved.
        for i in (0..msgs.len()).rev() {
            if msgs[i].role != ChatRole::Assistant {
                continue;
            }
            let has_part = msgs[i].parts.iter().any(|p| match p {
                ChatPart::ToolCall(t) => t.tool_call_id == tool_call_id,
                _ => false,
            });
            if !has_part {
                continue;
            }
            for p in msgs[i].parts.iter_mut() {
                if let ChatPart::ToolCall(t) = p {
                    // JS `chunk || rawOut || p.detail`: empty strings fall
                    // through, so each fallback only applies when non-empty.
                    t.detail = if let Some(term) = term_text {
                        format!("{}{}", t.detail, term)
                    } else if !chunk.is_empty() {
                        chunk.clone()
                    } else if raw_out.map(|r| !r.is_empty()).unwrap_or(false) {
                        raw_out.unwrap_or("").to_owned()
                    } else {
                        t.detail.clone()
                    };
                    if let Some(s) = &status {
                        t.status = s.clone();
                    }
                    if has_content {
                        t.diffs = if diff_blocks.is_empty() {
                            None
                        } else {
                            Some(diff_blocks.clone())
                        };
                    }
                }
            }
            return ApplyPatch::Messages;
        }
        return ApplyPatch::Nothing;
    }
    if kind == "plan" {
        // Plan updates are full snapshots; render the latest one as a live
        // status area outside the message stream.
        let raw = rest.get("entries").and_then(Value::as_array);
        let entries: Vec<PlanEntry> = raw
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| {
                        let content = e.get("content").and_then(Value::as_str).unwrap_or("");
                        if content.is_empty() {
                            return None;
                        }
                        Some(PlanEntry {
                            content: content.to_owned(),
                            status: opt_str(e.get("status")),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        return ApplyPatch::Plan(if entries.is_empty() {
            None
        } else {
            Some(entries)
        });
    }
    if kind == "config_option_update" {
        let opts = rest
            .get("configOptions")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| serde_json::from_value::<ConfigOption>(v.clone()).ok())
                    .collect::<Vec<_>>()
            });
        return match opts {
            Some(opts) => ApplyPatch::ConfigOptions(opts),
            None => ApplyPatch::Nothing,
        };
    }
    if kind == "current_mode_update" {
        return match opt_str(rest.get("currentModeId")) {
            Some(mode) => ApplyPatch::CurrentMode(mode),
            None => ApplyPatch::Nothing,
        };
    }
    if kind == "usage_update" {
        let used = opt_u64(rest.get("used"));
        let size = opt_u64(rest.get("size"));
        return match (used, size) {
            (Some(used), Some(size)) => ApplyPatch::Usage(ContextUsage { used, size }),
            _ => ApplyPatch::Nothing,
        };
    }
    if kind == "available_commands_update" {
        let list = rest.get("availableCommands").and_then(Value::as_array);
        return match list {
            None => ApplyPatch::Nothing,
            Some(arr) => {
                let commands: Vec<SlashCommand> = arr
                    .iter()
                    .filter(|c| {
                        c.get("name")
                            .map(|n| n.is_string())
                            .unwrap_or(false)
                    })
                    .filter_map(|c| serde_json::from_value::<SlashCommand>(c.clone()).ok())
                    .collect();
                ApplyPatch::AvailableCommands(commands)
            }
        };
    }
    // Unknown kinds (additive-only contract): ignore.
    ApplyPatch::Nothing
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn factory() -> MessageFactory {
        MessageFactory::default()
    }

    fn chunk(kind: &str, mid: Option<&str>, text: &str) -> SessionUpdate {
        let mut rest = serde_json::Map::new();
        rest.insert(
            "content".to_owned(),
            json!({ "type": "text", "text": text }),
        );
        if let Some(mid) = mid {
            rest.insert("messageId".to_owned(), json!(mid));
        }
        SessionUpdate::with_rest(kind, rest)
    }

    #[test]
    fn bridge_page_marker_floor() {
        // Port anchor: the 0.44.1 floor from tests/earlier-*.test.ts drivers.
        assert!(bridge_supports_page_marker(Some("0.44.1")));
        assert!(bridge_supports_page_marker(Some("0.44.2")));
        assert!(bridge_supports_page_marker(Some("0.45.0")));
        assert!(bridge_supports_page_marker(Some("1.0.0")));
        assert!(!bridge_supports_page_marker(Some("0.44.0")));
        assert!(!bridge_supports_page_marker(Some("0.43.9")));
        // parseInt prefix semantics: pre-release suffixes parse their digits.
        assert!(bridge_supports_page_marker(Some("0.44.1-beta.2")));
        // Unparseable versions read as NOT capable (safe fallback).
        assert!(!bridge_supports_page_marker(Some("0.44")));
        assert!(!bridge_supports_page_marker(Some("abc")));
        assert!(!bridge_supports_page_marker(None));
    }

    #[test]
    fn local_message_id_shape() {
        assert!(is_local_message_id(Some("m1")));
        assert!(is_local_message_id(Some("m42")));
        assert!(!is_local_message_id(Some("m")));
        assert!(!is_local_message_id(Some("m1a")));
        assert!(!is_local_message_id(Some("1")));
        assert!(!is_local_message_id(Some("thought_m1")));
        assert!(!is_local_message_id(None));
    }

    #[test]
    fn snapshot_tail_id_is_verbatim() {
        assert_eq!(snapshot_tail_id(&[]), None);
        let mut ids = factory();
        let a = ids.new_message(ChatRole::User, 0);
        let b = ids.new_message(ChatRole::Assistant, 0);
        assert_eq!(snapshot_tail_id(&[a, b.clone()]).as_deref(), Some("m2"));
        // A thought id passes through verbatim (no prefix stripping).
        let thought = ChatMessage {
            id: "thought_m9".to_owned(),
            role: ChatRole::Assistant,
            parts: vec![ChatPart::Thought { text: "t".into() }],
            created_at: 0,
            collapsed: None,
        };
        assert_eq!(
            snapshot_tail_id(&[thought]).as_deref(),
            Some("thought_m9")
        );
    }

    #[test]
    fn session_cache_is_lru_bounded() {
        let mut cache = SessionCache::default();
        for i in 0..8 {
            let mut snap = SessionSnapshot::default();
            snap.total_messages = Some(i);
            cache.write(&format!("s{i}"), snap);
        }
        // Only the 6 most recent survive.
        assert!(!cache.contains("s0"));
        assert!(!cache.contains("s1"));
        assert!(cache.contains("s2"));
        assert!(cache.contains("s7"));
        // A read refreshes recency: s2 becomes most recent, s3 is now oldest.
        assert_eq!(cache.read("s2").and_then(|s| s.total_messages), Some(2));
        cache.write("s8", SessionSnapshot::default());
        assert!(cache.contains("s2"));
        assert!(!cache.contains("s3"));
        cache.remove("s8");
        assert!(!cache.contains("s8"));
    }

    #[test]
    fn chunks_group_by_message_id_and_part_kind() {
        let mut msgs = Vec::new();
        let mut ids = factory();
        // Same messageId: one message, consecutive same-kind parts merge.
        apply_one(
            &mut msgs,
            &chunk("agent_message_chunk", Some("m_a"), "hello "),
            None,
            &mut ids,
            1,
        );
        apply_one(
            &mut msgs,
            &chunk("agent_message_chunk", Some("m_a"), "world"),
            None,
            &mut ids,
            2,
        );
        apply_one(
            &mut msgs,
            &chunk("agent_thought_chunk", Some("m_a"), "thinking"),
            None,
            &mut ids,
            3,
        );
        apply_one(
            &mut msgs,
            &chunk("agent_message_chunk", Some("m_a"), "more"),
            None,
            &mut ids,
            4,
        );
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].id, "m_a");
        assert_eq!(msgs[0].parts.len(), 3);
        assert_eq!(
            msgs[0].parts[0],
            ChatPart::Text {
                text: "hello world".into()
            }
        );
        assert_eq!(
            msgs[0].parts[1],
            ChatPart::Thought {
                text: "thinking".into()
            }
        );
        assert_eq!(
            msgs[0].parts[2],
            ChatPart::Text { text: "more".into() }
        );
        // No messageId: merges into the trailing same-role message.
        apply_one(
            &mut msgs,
            &chunk("agent_message_chunk", None, " tail"),
            None,
            &mut ids,
            5,
        );
        assert_eq!(msgs.len(), 1);
        assert_eq!(
            msgs[0].parts[2],
            ChatPart::Text { text: "more tail".into() }
        );
    }

    #[test]
    fn collapsed_user_meta_marks_the_message() {
        let mut msgs = Vec::new();
        let mut ids = factory();
        let u = chunk("user_message_chunk", Some("m_u"), "context handoff text");
        let meta = json!({ "zcode": { "collapsed": true } });
        let patch = apply_one(&mut msgs, &u, Some(&meta), &mut ids, 1);
        assert_eq!(patch, ApplyPatch::Messages);
        assert_eq!(msgs[0].collapsed, Some(true));
        // Without the flag the message stays expanded.
        let mut msgs2 = Vec::new();
        apply_one(&mut msgs2, &u, None, &mut ids, 2);
        assert_eq!(msgs2[0].collapsed, None);
    }

    #[test]
    fn tool_call_parts_dedupe_on_tool_call_id() {
        let mut msgs = Vec::new();
        let mut ids = factory();
        let mut rest = serde_json::Map::new();
        rest.insert("toolCallId".to_owned(), json!("tc_1"));
        rest.insert("title".to_owned(), json!("Bash: npm test"));
        rest.insert(
            "content".to_owned(),
            json!({ "type": "text", "text": "output v1" }),
        );
        rest.insert("kind".to_owned(), json!("execute"));
        rest.insert(
            "_meta".to_owned(),
            json!({ "claudeCode": { "toolName": "Bash" } }),
        );
        apply_one(
            &mut msgs,
            &SessionUpdate::with_rest("tool_call", rest.clone()),
            None,
            &mut ids,
            1,
        );
        // Same toolCallId reannounced (replay re-delivery): replaced in
        // place, never a second part.
        rest.insert(
            "content".to_owned(),
            json!({ "type": "text", "text": "output v2" }),
        );
        rest.insert("status".to_owned(), json!("completed"));
        apply_one(
            &mut msgs,
            &SessionUpdate::with_rest("tool_call", rest),
            None,
            &mut ids,
            2,
        );
        let tool_calls: Vec<&ToolCallPart> = msgs
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| match p {
                ChatPart::ToolCall(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].tool_name, "Bash: npm test");
        assert_eq!(tool_calls[0].detail, "output v2");
        assert_eq!(tool_calls[0].status, "completed");
        assert_eq!(tool_calls[0].raw_name.as_deref(), Some("Bash"));
        assert_eq!(tool_calls[0].kind.as_deref(), Some("execute"));
        // ADR 0003 lookup: the part is findable by toolCallId.
        let found = find_tool_call_part(&msgs, "tc_1").expect("part findable");
        assert_eq!(found.detail, "output v2");
        assert!(find_tool_call_part(&msgs, "tc_missing").is_none());
    }

    #[test]
    fn tool_call_update_replaces_content_and_appends_terminal_output() {
        let mut msgs = Vec::new();
        let mut ids = factory();
        let mut call = serde_json::Map::new();
        call.insert("toolCallId".to_owned(), json!("tc_1"));
        call.insert("title".to_owned(), json!("Bash"));
        call.insert("content".to_owned(), json!({ "type": "text", "text": "v1" }));
        apply_one(
            &mut msgs,
            &SessionUpdate::with_rest("tool_call", call),
            None,
            &mut ids,
            1,
        );

        // Content blocks REPLACE the whole collection (text and diffs).
        let mut upd = serde_json::Map::new();
        upd.insert("toolCallId".to_owned(), json!("tc_1"));
        upd.insert(
            "content".to_owned(),
            json!([{ "type": "text", "text": "replaced" }]),
        );
        upd.insert("status".to_owned(), json!("in_progress"));
        apply_one(
            &mut msgs,
            &SessionUpdate::with_rest("tool_call_update", upd),
            None,
            &mut ids,
            2,
        );
        let part = find_tool_call_part(&msgs, "tc_1").unwrap();
        assert_eq!(part.detail, "replaced");
        assert_eq!(part.status, "in_progress");

        // Status-only update leaves content untouched.
        let mut status_only = serde_json::Map::new();
        status_only.insert("toolCallId".to_owned(), json!("tc_1"));
        status_only.insert("status".to_owned(), json!("completed"));
        apply_one(
            &mut msgs,
            &SessionUpdate::with_rest("tool_call_update", status_only),
            None,
            &mut ids,
            3,
        );
        let part = find_tool_call_part(&msgs, "tc_1").unwrap();
        assert_eq!(part.detail, "replaced");
        assert_eq!(part.status, "completed");

        // Terminal-output deltas append; rawOutput applies when content is
        // empty; a content update without diff blocks clears diffs.
        let mut term = serde_json::Map::new();
        term.insert("toolCallId".to_owned(), json!("tc_1"));
        term.insert(
            "_meta".to_owned(),
            json!({ "terminal_output": { "data": "line1\n" } }),
        );
        apply_one(
            &mut msgs,
            &SessionUpdate::with_rest("tool_call_update", term),
            None,
            &mut ids,
            4,
        );
        let part = find_tool_call_part(&msgs, "tc_1").unwrap();
        assert_eq!(part.detail, "replacedline1\n");

        // An update with NO content blocks and a rawOutput string: the empty
        // chunk falls through to the raw output (JS `chunk || rawOut`).
        let mut raw = serde_json::Map::new();
        raw.insert("toolCallId".to_owned(), json!("tc_1"));
        raw.insert("rawOutput".to_owned(), json!("raw fallback"));
        apply_one(
            &mut msgs,
            &SessionUpdate::with_rest("tool_call_update", raw),
            None,
            &mut ids,
            5,
        );
        let part = find_tool_call_part(&msgs, "tc_1").unwrap();
        assert_eq!(part.detail, "raw fallback");

        let mut with_diff = serde_json::Map::new();
        with_diff.insert("toolCallId".to_owned(), json!("tc_1"));
        with_diff.insert(
            "content".to_owned(),
            json!([{ "type": "diff", "path": "/a", "oldText": null, "newText": "n" }]),
        );
        apply_one(
            &mut msgs,
            &SessionUpdate::with_rest("tool_call_update", with_diff),
            None,
            &mut ids,
            6,
        );
        let part = find_tool_call_part(&msgs, "tc_1").unwrap();
        assert_eq!(part.diffs.as_ref().map(|d| d.len()), Some(1));

        let mut text_only = serde_json::Map::new();
        text_only.insert("toolCallId".to_owned(), json!("tc_1"));
        text_only.insert(
            "content".to_owned(),
            json!([{ "type": "text", "text": "final" }]),
        );
        apply_one(
            &mut msgs,
            &SessionUpdate::with_rest("tool_call_update", text_only),
            None,
            &mut ids,
            7,
        );
        let part = find_tool_call_part(&msgs, "tc_1").unwrap();
        assert_eq!(part.diffs, None);
        assert_eq!(part.detail, "final");
    }

    #[test]
    fn session_level_kinds_carry_their_own_values() {
        let mut msgs = Vec::new();
        let mut ids = factory();

        let plan = SessionUpdate::with_rest(
            "plan",
            serde_json::Map::from_iter([
                ("entries".to_owned(), json!([
                    { "content": "step one", "status": "completed" },
                    { "content": "step two", "status": "pending" },
                    { "status": "no content dropped" },
                ])),
            ]),
        );
        assert_eq!(
            apply_one(&mut msgs, &plan, None, &mut ids, 1),
            ApplyPatch::Plan(Some(vec![
                PlanEntry {
                    content: "step one".into(),
                    status: Some("completed".into()),
                },
                PlanEntry {
                    content: "step two".into(),
                    status: Some("pending".into()),
                },
            ]))
        );
        // An emptied plan clears the snapshot.
        let empty_plan = SessionUpdate::with_rest(
            "plan",
            serde_json::Map::from_iter([("entries".to_owned(), json!([]))]),
        );
        assert_eq!(
            apply_one(&mut msgs, &empty_plan, None, &mut ids, 2),
            ApplyPatch::Plan(None)
        );

        let usage = SessionUpdate::with_rest(
            "usage_update",
            serde_json::Map::from_iter([
                ("used".to_owned(), json!(120)),
                ("size".to_owned(), json!(200)),
            ]),
        );
        assert_eq!(
            apply_one(&mut msgs, &usage, None, &mut ids, 3),
            ApplyPatch::Usage(ContextUsage { used: 120, size: 200 })
        );
        // Missing size -> no patch (both numbers required).
        let half = SessionUpdate::with_rest(
            "usage_update",
            serde_json::Map::from_iter([("used".to_owned(), json!(1))]),
        );
        assert_eq!(
            apply_one(&mut msgs, &half, None, &mut ids, 4),
            ApplyPatch::Nothing
        );

        let mode = SessionUpdate::with_rest(
            "current_mode_update",
            serde_json::Map::from_iter([("currentModeId".to_owned(), json!("code"))]),
        );
        assert_eq!(
            apply_one(&mut msgs, &mode, None, &mut ids, 5),
            ApplyPatch::CurrentMode("code".into())
        );

        let commands = SessionUpdate::with_rest(
            "available_commands_update",
            serde_json::Map::from_iter([(
                "availableCommands".to_owned(),
                json!([{ "name": "init" }, { "description": "no name dropped" }]),
            )]),
        );
        assert_eq!(
            apply_one(&mut msgs, &commands, None, &mut ids, 6),
            ApplyPatch::AvailableCommands(vec![SlashCommand {
                name: "init".into(),
                description: None,
                input: None,
            }])
        );

        // Unknown kinds are ignored (additive-only contract).
        let future = SessionUpdate::new("future_update_kind");
        assert_eq!(
            apply_one(&mut msgs, &future, None, &mut ids, 7),
            ApplyPatch::Nothing
        );

        // Empty chunk text is a no-op.
        let empty_chunk = chunk("agent_message_chunk", Some("m_x"), "");
        assert_eq!(
            apply_one(&mut msgs, &empty_chunk, None, &mut ids, 8),
            ApplyPatch::Nothing
        );
    }
}
