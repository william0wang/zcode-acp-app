//! Approval card semantics — the port of the permission/elicitation slices
//! of `src/store/appStore.ts` plus the card shapes of
//! `src/components/PermissionDialog.tsx` / `ElicitationDialog.tsx`.
//!
//! CONTEXT.md "Approval Card": the surface rendering `session/request_permission`
//! requests — shown ONLY in the session that raised it (bridge 0.17.0
//! per-session semantics) — in three shapes: Plan Approval (ExitPlanMode),
//! Tool Permission, and Question (AskUserQuestion). Context shown on the card
//! comes from the tool_call matched by `toolCallId`, never guessed from the
//! options alone.
//!
//! Permission Race (CONTEXT.md): first-response-wins across clients. The
//! losing dismissal signals are `$/cancel_request` (the protocol-level
//! cancel, TS `onCancelRequest`) and — what the bridge actually broadcasts,
//! because the Node SDK never forwards the cancellation — `$/zcode/ask_settled`
//! (`zcode-acp-server` `emitAskSettled`, mirrored by martty's
//! `dismiss_ask_for_session`). Both drop the pending card without answering.

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::chat::{ChatMessage, ChatPart, ChatRole, ToolCallPart};

/// One `session/request_permission` option (wire field spellings frozen).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionOption {
    pub option_id: String,
    pub kind: Option<String>,
    pub name: Option<String>,
}

/// Context resolved for the approval card. The request params alone carry
/// almost nothing readable; the interesting text (plan, question) rides on
/// the tool_call the bridge emits right before the request (ADR 0003).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApprovalContext {
    pub tool_call_id: Option<String>,
    /// Raw backend tool name (`_meta.claudeCode.toolName`, e.g. "Bash"),
    /// distinct from `title` which holds the wire title "Bash: npm test".
    pub tool_name: Option<String>,
    /// ACP tool kind of the matched tool_call (execute/edit/switch_mode/…).
    pub kind: Option<String>,
    pub title: Option<String>,
    /// Plan text (ExitPlanMode) or question text (AskUserQuestion), from the
    /// matched tool_call's content.
    pub detail: Option<String>,
    pub plan: Option<String>,
    /// Pretty-printed `toolCall.rawInput` — the JSON fallback surface.
    pub raw_input_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingPermission {
    pub request_id: u64,
    pub session_id: String,
    pub options: Vec<PermissionOption>,
    pub context: Option<ApprovalContext>,
}

/// One option of an elicitation question field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElicitOption {
    pub value: String,
    pub label: String,
}

/// One AskUserQuestion field parsed out of an `elicitation/create` form
/// (the bridge's `q_<i>` enum/array + `q_<i>_other` free-text companion; the
/// plan-approval form is a single `approval` enum field).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElicitField {
    /// Field key in requestedSchema (q_0, q_1, …) and its free-text companion.
    pub key: String,
    pub other_key: Option<String>,
    pub question: String,
    /// Long-form markdown under the question — the plan-approval form (bridge
    /// 0.33+) carries the COMPLETE plan here.
    pub description: Option<String>,
    pub multi: bool,
    pub options: Vec<ElicitOption>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingElicitation {
    pub request_id: u64,
    pub session_id: String,
    pub message: String,
    pub fields: Vec<ElicitField>,
}

/// The pending ask of one session — permission or elicitation form.
#[derive(Debug, Clone, PartialEq)]
pub enum PendingAsk {
    Permission(PendingPermission),
    Elicitation(PendingElicitation),
}

impl PendingAsk {
    pub fn request_id(&self) -> u64 {
        match self {
            PendingAsk::Permission(p) => p.request_id,
            PendingAsk::Elicitation(e) => e.request_id,
        }
    }

    pub fn session_id(&self) -> &str {
        match self {
            PendingAsk::Permission(p) => &p.session_id,
            PendingAsk::Elicitation(e) => &e.session_id,
        }
    }
}

/// The three approval card shapes (CONTEXT.md "Approval Card"), classified
/// the way `PermissionDialog.tsx` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalShape {
    /// ExitPlanMode: fixed approve/reject pair, plan markdown body.
    Plan,
    /// A tool asking for permission: highlights + option list.
    Tool,
    /// AskUserQuestion: question text + option list.
    Question,
}

/// Classifies a pending permission request into its card shape
/// (PermissionDialog.tsx `isPlan` / `isQuestion`).
pub fn permission_shape(pending: &PendingPermission) -> ApprovalShape {
    let ctx = pending.context.as_ref();
    if ctx.map(|c| c.kind.as_deref() == Some("switch_mode")).unwrap_or(false) {
        return ApprovalShape::Plan;
    }
    let has = |id: &str| pending.options.iter().any(|o| o.option_id == id);
    if has("approve") && has("reject") {
        return ApprovalShape::Plan;
    }
    if ctx
        .map(|c| c.tool_name.as_deref() == Some("AskUserQuestion"))
        .unwrap_or(false)
    {
        return ApprovalShape::Question;
    }
    ApprovalShape::Tool
}

/// Common keys of tool inputs worth surfacing before the JSON fallback
/// (PermissionDialog.tsx `INPUT_KEYS`).
pub const INPUT_KEYS: [&str; 6] = ["command", "file_path", "path", "pattern", "url", "description"];

/// A tool-input highlight row: the input key and its string value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputHighlight {
    pub label: String,
    pub value: String,
}

/// Pulls the readable scalar fields out of the card's raw tool input
/// (PermissionDialog.tsx `rawInputHighlights`). Unparseable input yields
/// nothing — the caller falls back to the raw JSON pane.
pub fn raw_input_highlights(ctx: &ApprovalContext) -> Vec<InputHighlight> {
    let Some(raw) = ctx.raw_input_text.as_deref() else {
        return Vec::new();
    };
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for key in INPUT_KEYS {
        if let Some(Value::String(v)) = map.get(key) {
            if !v.is_empty() {
                out.push(InputHighlight {
                    label: key.to_string(),
                    value: v.clone(),
                });
            }
        }
    }
    out
}

/// Locates the tool_call part the bridge emits right before an interaction
/// request; its content holds the plan/question text (ADR 0003). Newest
/// first (`replay.ts findToolCallPart`).
pub fn find_tool_call_part<'a>(
    messages: &'a [ChatMessage],
    tool_call_id: &str,
) -> Option<&'a ToolCallPart> {
    for msg in messages.iter().rev() {
        if msg.role != ChatRole::Assistant {
            continue;
        }
        for part in msg.parts.iter().rev() {
            if let ChatPart::ToolCall(tc) = part {
                if tc.tool_call_id == tool_call_id {
                    return Some(tc);
                }
            }
        }
    }
    None
}

/// Builds the card context from a `session/request_permission` params object
/// (appStore.ts `buildApprovalContext`). Wire-provided popup fields (bridge
/// 0.33+ `toolCall.title` / `toolCall.content`) are preferred over the matched
/// tool_call part and the INPUT_KEYS heuristic; `plan` rides on
/// `toolCall.rawInput.plan`.
pub fn build_approval_context(params: &Value, messages: &[ChatMessage]) -> Option<ApprovalContext> {
    let tc = params.get("toolCall").filter(|v| v.is_object())?;
    let tool_call_id = tc
        .get("toolCallId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let part = tool_call_id
        .as_deref()
        .and_then(|id| find_tool_call_part(messages, id));
    let raw_input = tc.get("rawInput").filter(|v| !v.is_null());
    let plan = raw_input
        .and_then(|r| r.get("plan"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let wire_title = tc
        .get("title")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let content_text = tc
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.pointer("/content/text").and_then(Value::as_str))
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .filter(|s| !s.is_empty());
    let raw_input_text = raw_input.map(|r| match r {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    });
    if tool_call_id.is_none()
        && raw_input_text.is_none()
        && wire_title.is_none()
        && content_text.is_none()
    {
        return None;
    }
    Some(ApprovalContext {
        tool_call_id,
        tool_name: part.and_then(|p| {
            p.raw_name
                .clone()
                .or_else(|| Some(p.tool_name.clone()))
        }),
        kind: part.and_then(|p| p.kind.clone()),
        title: wire_title.or_else(|| part.map(|p| p.tool_name.clone())),
        detail: content_text.or_else(|| part.map(|p| p.detail.clone())),
        plan,
        raw_input_text,
    })
}

/// Parses a `session/request_permission` params object into the stored
/// pending entry (appStore.ts `onServerRequest`). Always stored — even an
/// option-less request stays answerable — with options filtered to those
/// carrying a string `optionId`.
pub fn parse_permission(
    request_id: u64,
    params: &Value,
    messages: &[ChatMessage],
) -> PendingPermission {
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let options = params
        .get("options")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let option_id = row.get("optionId").and_then(Value::as_str)?;
                    Some(PermissionOption {
                        option_id: option_id.to_string(),
                        kind: row
                            .get("kind")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        name: row
                            .get("name")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    PendingPermission {
        request_id,
        session_id,
        options,
        context: build_approval_context(params, messages),
    }
}

/// Parses an `elicitation/create` form into renderable fields (appStore.ts
/// `parseElicitationForm`): the bridge's `buildAskUserElicitationForm` shape
/// (`q_<i>` enum/array + `q_<i>_other` free-text companion) and the
/// plan-approval form (single `approval` enum field whose `description`
/// carries the full plan markdown). The `__skip__` sentinel option is
/// dropped — an unanswered field IS the skip. Returns `None` when the schema
/// has no question field at all.
pub fn parse_elicitation(request_id: u64, params: &Value) -> Option<PendingElicitation> {
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    let message = params
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let props = params
        .pointer("/requestedSchema/properties")
        .and_then(Value::as_object)?;
    let mut fields: Vec<ElicitField> = Vec::new();
    for (key, prop) in props {
        if key.ends_with("_other") {
            continue;
        }
        let Some(prop) = prop.as_object() else {
            continue;
        };
        let variants = prop
            .get("oneOf")
            .and_then(Value::as_array)
            .or_else(|| prop.get("items").and_then(|i| i.get("anyOf")).and_then(Value::as_array));
        let Some(variants) = variants else {
            continue; // not a question field (additive schema)
        };
        let options: Vec<ElicitOption> = variants
            .iter()
            .filter_map(|v| {
                let value = v.get("const").and_then(Value::as_str)?;
                if value == "__skip__" {
                    return None;
                }
                Some(ElicitOption {
                    value: value.to_string(),
                    label: v
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or(value)
                        .to_string(),
                })
            })
            .collect();
        let other_key_prop = format!("{key}_other");
        let other_key = if props
            .get(&other_key_prop)
            .map(Value::is_object)
            .unwrap_or(false)
        {
            Some(other_key_prop)
        } else {
            None
        };
        fields.push(ElicitField {
            key: key.clone(),
            other_key,
            question: prop
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(key)
                .to_string(),
            description: prop
                .get("description")
                .and_then(Value::as_str)
                .filter(|d| !d.trim().is_empty())
                .map(str::to_string),
            multi: prop.get("type").and_then(Value::as_str) == Some("array"),
            options,
        });
    }
    if fields.is_empty() {
        return None;
    }
    Some(PendingElicitation {
        request_id,
        session_id: session_id.to_string(),
        message,
        fields,
    })
}

/// The plan-approval elicitation form: a single non-multi field with no
/// free-text companion whose options are exactly approve/reject
/// (ElicitationDialog.tsx `isPlanField`; the values the bridge's
/// `buildPlanApprovalElicitationForm` hardcodes).
pub fn plan_field(form: &PendingElicitation) -> Option<&ElicitField> {
    let field = form.fields.first()?;
    if field.multi
        || field.other_key.is_some()
        || !field.options.iter().any(|o| o.value == "approve")
        || !field.options.iter().any(|o| o.value == "reject")
    {
        return None;
    }
    Some(field)
}

// ---------------------------------------------------------------------------
// Wire answers
// ---------------------------------------------------------------------------

/// The `session/request_permission` answer for a picked option
/// (appStore.ts `answerPermission`).
pub fn permission_selected_result(option_id: &str) -> Value {
    json!({ "outcome": { "outcome": "selected", "optionId": option_id } })
}

/// One field answer: a plain string or a multi-select array
/// (`Record<string, string | string[]>` on the wire).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElicitValue {
    One(String),
    Many(Vec<String>),
}

/// The submitted form content (field key -> value), in field order.
pub type ElicitAnswers = Vec<(String, ElicitValue)>;

/// The `elicitation/create` answer envelope: accept carries the form content,
/// anything else declines (appStore.ts `answerElicitation`).
pub fn elicitation_result(answers: Option<&ElicitAnswers>) -> Value {
    match answers {
        Some(fields) if !fields.is_empty() => {
            let mut content = serde_json::Map::new();
            for (key, value) in fields {
                let v = match value {
                    ElicitValue::One(s) => Value::String(s.clone()),
                    ElicitValue::Many(list) => Value::Array(
                        list.iter().map(|s| Value::String(s.clone())).collect(),
                    ),
                };
                content.insert(key.clone(), v);
            }
            json!({ "action": "accept", "content": Value::Object(content) })
        }
        _ => json!({ "action": "decline" }),
    }
}

/// Folds the card's local pick state into the submit content
/// (ElicitationDialog.tsx `submit`): multi-select sends the checked values as
/// an array, single-select sends the pick, and a non-empty free-text
/// companion lands under its own key (overriding the pick server-side).
/// Absent fields are skipped questions.
pub fn build_elicitation_answers(
    fields: &[ElicitField],
    picked: &HashMap<String, String>,
    checked: &HashMap<String, bool>,
    others: &HashMap<String, String>,
) -> ElicitAnswers {
    let mut answers: ElicitAnswers = Vec::new();
    for f in fields {
        let free = f
            .other_key
            .as_deref()
            .and_then(|k| others.get(k))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if f.multi {
            let selected: Vec<String> = f
                .options
                .iter()
                .filter(|o| checked.get(&o.value).copied().unwrap_or(false))
                .map(|o| o.value.clone())
                .collect();
            if !selected.is_empty() {
                answers.push((f.key.clone(), ElicitValue::Many(selected)));
            }
        } else if free.is_empty() {
            // No custom text: the plain pick decides; no pick = skipped question.
            if let Some(v) = picked.get(&f.key) {
                answers.push((f.key.clone(), ElicitValue::One(v.clone())));
            }
        }
        if !free.is_empty() {
            if let Some(key) = &f.other_key {
                answers.push((key.clone(), ElicitValue::One(free)));
            }
        }
    }
    answers
}

// ---------------------------------------------------------------------------
// Race state machine
// ---------------------------------------------------------------------------

/// What the driver should do with an incoming interaction request.
#[derive(Debug, Clone, PartialEq)]
pub enum IncomingAsk {
    /// Stored, waiting for a local answer. Carries the request it displaced
    /// (the bridge re-sends still-undecided asks to reconnected clients under
    /// a fresh id — `resendPendingInteractions`).
    Stored {
        session_id: String,
        displaced_request_id: Option<u64>,
    },
    /// Answer NOW with this wire result — the unparseable-elicitation case:
    /// decline rather than leave the request dangling (first-response-wins;
    /// another client may still answer).
    AnswerNow(Value),
    /// Unsupported interaction request — deliberately NOT answered: the
    /// primary editor client is always attached and may answer.
    Unsupported,
}

/// The result of [`ApprovalCore::on_request`].
#[derive(Debug, Clone, PartialEq)]
pub struct IncomingAskReport {
    pub ask: IncomingAsk,
    /// notify() text when the ask belongs to a session other than the active
    /// one, so a blocked approval never waits in silence (appStore.ts
    /// `onServerRequest`'s toast branches).
    pub toast: Option<String>,
}

/// A settled ask: the wire result plus the sessions whose awaiting badge
/// must clear.
#[derive(Debug, Clone, PartialEq)]
pub struct AskOutcome {
    pub result: Value,
    pub cleared_sessions: Vec<String>,
}

/// The pending-ask halves of the app store: per-session permission and
/// elicitation cards plus the Permission Race transitions. Keyed by session
/// id — a request is shown ONLY in the session that raised it (bridge 0.17.0
/// per-session semantics); another session's ask surfaces through the toast /
/// awaiting badge instead.
#[derive(Debug, Default)]
pub struct ApprovalCore {
    permissions: HashMap<String, PendingPermission>,
    elicitations: HashMap<String, PendingElicitation>,
}

impl ApprovalCore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Routes one server -> client interaction request.
    pub fn on_request(
        &mut self,
        request_id: u64,
        method: &str,
        params: &Value,
        messages: &[ChatMessage],
        active_session: Option<&str>,
    ) -> IncomingAskReport {
        match method {
            "session/request_permission" => {
                let pending = parse_permission(request_id, params, messages);
                let session_id = pending.session_id.clone();
                // The short toast title: the wire title, the raw toolName
                // param, or "tool" (appStore.ts).
                let title = params
                    .pointer("/toolCall/title")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        params
                            .get("toolName")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                    })
                    .unwrap_or("tool");
                let toast = toast_for_other_session(active_session, &session_id, || {
                    format!("approval requested: {}", truncate_chars(title, 60))
                });
                let displaced = self
                    .permissions
                    .insert(session_id.clone(), pending)
                    .filter(|old| old.request_id != request_id)
                    .map(|old| old.request_id);
                IncomingAskReport {
                    ask: IncomingAsk::Stored {
                        session_id,
                        displaced_request_id: displaced,
                    },
                    toast,
                }
            }
            "elicitation/create" => match parse_elicitation(request_id, params) {
                Some(form) => {
                    let session_id = form.session_id.clone();
                    let message = form.message.clone();
                    let toast = toast_for_other_session(active_session, &session_id, || {
                        let text = if message.is_empty() {
                            "waiting for your answer".to_string()
                        } else {
                            message
                        };
                        format!("agent question: {}", truncate_chars(&text, 60))
                    });
                    let displaced = self
                        .elicitations
                        .insert(session_id.clone(), form)
                        .filter(|old| old.request_id != request_id)
                        .map(|old| old.request_id);
                    IncomingAskReport {
                        ask: IncomingAsk::Stored {
                            session_id,
                            displaced_request_id: displaced,
                        },
                        toast,
                    }
                }
                // Unparseable schema: decline rather than leave the request
                // dangling (first-response-wins; Zed may still answer).
                None => IncomingAskReport {
                    ask: IncomingAsk::AnswerNow(elicitation_result(None)),
                    toast: None,
                },
            },
            _ => IncomingAskReport {
                ask: IncomingAsk::Unsupported,
                toast: None,
            },
        }
    }

    /// Answers a stored permission: clears every session holding this request
    /// id and returns the wire result. `None` = unknown request (the race was
    /// already lost or settled) — a no-op, exactly like a missing respond
    /// closure in the TS store.
    pub fn answer_permission(&mut self, request_id: u64, option_id: &str) -> Option<AskOutcome> {
        if !self
            .permissions
            .values()
            .any(|p| p.request_id == request_id)
        {
            return None;
        }
        let cleared = self.clear_permissions_by_request(request_id);
        Some(AskOutcome {
            result: permission_selected_result(option_id),
            cleared_sessions: cleared,
        })
    }

    /// Answers a stored elicitation form: `Some(answers)` accepts with
    /// content, `None` declines (Cancel). Absent fields are skipped questions.
    pub fn answer_elicitation(
        &mut self,
        request_id: u64,
        answers: Option<&ElicitAnswers>,
    ) -> Option<AskOutcome> {
        if !self
            .elicitations
            .values()
            .any(|e| e.request_id == request_id)
        {
            return None;
        }
        let cleared = self.clear_elicitations_by_request(request_id);
        Some(AskOutcome {
            result: elicitation_result(answers),
            cleared_sessions: cleared,
        })
    }

    /// `$/cancel_request`: we lost the Permission Race — drop the card(s) for
    /// this request without answering. Returns the sessions whose awaiting
    /// badge must clear.
    pub fn on_cancel_request(&mut self, request_id: u64) -> Vec<String> {
        let mut cleared = self.clear_permissions_by_request(request_id);
        cleared.extend(self.clear_elicitations_by_request(request_id));
        cleared
    }

    /// `$/zcode/ask_settled`: the ask was decided on another client (or the
    /// bridge's wait broke) — dismiss this session's copy of the dialog. The
    /// winning client already removed its own entry, so this is a no-op
    /// there. Returns the dropped request ids so the caller can drop their
    /// responders.
    pub fn on_ask_settled(
        &mut self,
        session_id: &str,
        kind: Option<&str>,
        tool_call_id: Option<&str>,
    ) -> Vec<u64> {
        let mut removed = Vec::new();
        if kind.map(|k| k == "permission").unwrap_or(true) {
            let matches = self.permissions.get(session_id).is_some_and(|p| {
                tool_call_id
                    .map(|id| p.context.as_ref().and_then(|c| c.tool_call_id.as_deref()) == Some(id))
                    .unwrap_or(true)
            });
            if matches {
                if let Some(p) = self.permissions.remove(session_id) {
                    removed.push(p.request_id);
                }
            }
        }
        if kind.map(|k| k == "elicitation").unwrap_or(true) {
            // Elicitation asks carry no toolCallId in the settle payload;
            // kind alone addresses them.
            if let Some(e) = self.elicitations.remove(session_id) {
                removed.push(e.request_id);
            }
        }
        removed
    }

    /// The pending ask of one session — the card renders ONLY there
    /// (PermissionDialog's `permissions[activeSessionId]`).
    pub fn pending_for_session(&self, session_id: &str) -> Option<PendingAsk> {
        if let Some(p) = self.permissions.get(session_id) {
            return Some(PendingAsk::Permission(p.clone()));
        }
        self.elicitations
            .get(session_id)
            .map(|e| PendingAsk::Elicitation(e.clone()))
    }

    /// Whether the session has any pending ask (drives the awaiting badge).
    pub fn has_pending(&self, session_id: &str) -> bool {
        self.permissions.contains_key(session_id) || self.elicitations.contains_key(session_id)
    }

    /// Full reset (driver lifecycle restart / instance switch).
    pub fn clear(&mut self) {
        self.permissions.clear();
        self.elicitations.clear();
    }

    fn clear_permissions_by_request(&mut self, request_id: u64) -> Vec<String> {
        let mut cleared = Vec::new();
        let ids: Vec<String> = self
            .permissions
            .iter()
            .filter(|(_, p)| p.request_id == request_id)
            .map(|(sid, _)| sid.clone())
            .collect();
        for sid in ids {
            self.permissions.remove(&sid);
            cleared.push(sid);
        }
        cleared
    }

    fn clear_elicitations_by_request(&mut self, request_id: u64) -> Vec<String> {
        let mut cleared = Vec::new();
        let ids: Vec<String> = self
            .elicitations
            .iter()
            .filter(|(_, e)| e.request_id == request_id)
            .map(|(sid, _)| sid.clone())
            .collect();
        for sid in ids {
            self.elicitations.remove(&sid);
            cleared.push(sid);
        }
        cleared
    }
}

/// The toast fires only when the ask's session is not the active one — the
/// sheet renders inside its own session, so elsewhere in the app the ask is
/// announced once.
fn toast_for_other_session(
    active: Option<&str>,
    session_id: &str,
    build: impl FnOnce() -> String,
) -> Option<String> {
    if active == Some(session_id) {
        None
    } else {
        Some(build())
    }
}

/// UTF-8-safe char-capped slice (`String.prototype.slice`).
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        text.chars().take(max).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{ChatMessage, ChatPart, ToolCallPart};
    use serde_json::json;

    fn msg_with_tool_call(id: &str, name: &str, raw_name: Option<&str>, detail: &str) -> ChatMessage {
        ChatMessage {
            id: format!("m_{id}"),
            role: ChatRole::Assistant,
            parts: vec![ChatPart::ToolCall(ToolCallPart {
                tool_call_id: id.to_string(),
                tool_name: name.to_string(),
                detail: detail.to_string(),
                status: "pending".to_string(),
                kind: Some("execute".to_string()),
                raw_name: raw_name.map(str::to_string),
                fold_kind: None,
                diffs: None,
            })],
            created_at: 1,
            collapsed: None,
        }
    }

    // ---- context matching by toolCallId ----

    #[test]
    fn context_prefers_wire_popup_fields_and_matches_the_tool_call_by_id() {
        let messages = vec![
            msg_with_tool_call("tc_1", "Bash: old", Some("Bash"), "stale detail"),
            msg_with_tool_call("tc_2", "Edit: file.rs", Some("Edit"), "part detail"),
        ];
        let params = json!({
            "sessionId": "s1",
            "options": [{ "optionId": "allow_once", "kind": "allow_once", "name": "Allow" }],
            "toolCall": {
                "toolCallId": "tc_2",
                "title": "Edit: file.rs",
                "rawInput": { "file_path": "/x.rs", "command": "none" },
                "content": [
                    { "type": "content", "content": { "type": "text", "text": "wire body" } }
                ]
            }
        });
        let ctx = build_approval_context(&params, &messages).expect("context");
        // Part fields enrich: raw backend name and kind come from the matched
        // tool_call (tc_2, not tc_1).
        assert_eq!(ctx.tool_call_id.as_deref(), Some("tc_2"));
        assert_eq!(ctx.tool_name.as_deref(), Some("Edit"));
        assert_eq!(ctx.kind.as_deref(), Some("execute"));
        // Wire-provided title/content win over the matched part.
        assert_eq!(ctx.title.as_deref(), Some("Edit: file.rs"));
        assert_eq!(ctx.detail.as_deref(), Some("wire body"));
        assert!(ctx.plan.is_none());
        assert!(ctx.raw_input_text.as_deref().unwrap().contains("file_path"));
    }

    #[test]
    fn context_falls_back_to_the_matched_part_when_the_wire_carries_nothing() {
        let messages = vec![msg_with_tool_call("tc_9", "Bash: npm test", None, "part-only detail")];
        let params = json!({
            "sessionId": "s1",
            "toolCall": { "toolCallId": "tc_9", "rawInput": null }
        });
        let ctx = build_approval_context(&params, &messages).expect("context");
        assert_eq!(ctx.title.as_deref(), Some("Bash: npm test"));
        assert_eq!(ctx.detail.as_deref(), Some("part-only detail"));
        assert_eq!(ctx.raw_input_text, None);
    }

    #[test]
    fn context_without_any_signal_is_none() {
        let params = json!({ "sessionId": "s1", "toolCall": {} });
        assert_eq!(build_approval_context(&params, &[]), None);
        // No toolCall object at all.
        assert_eq!(build_approval_context(&json!({ "sessionId": "s1" }), &[]), None);
    }

    #[test]
    fn plan_rides_on_raw_input_and_skips_user_messages() {
        // A user message carrying a same-id part must not match.
        let mut user_msg = msg_with_tool_call("tc_p", "x", None, "wrong role");
        user_msg.role = ChatRole::User;
        let messages = vec![user_msg];
        let params = json!({
            "toolCall": {
                "toolCallId": "tc_p",
                "rawInput": { "plan": "# The plan" },
                "title": "Ready to code?"
            }
        });
        let ctx = build_approval_context(&params, &messages).expect("context");
        assert_eq!(ctx.plan.as_deref(), Some("# The plan"));
        assert_eq!(ctx.tool_name, None, "user-role parts never match");
    }

    // ---- shape classification ----

    #[test]
    fn plan_shape_comes_from_kind_or_the_approve_reject_pair() {
        let mk = |kind: Option<&str>, ids: &[&str]| PendingPermission {
            request_id: 1,
            session_id: "s".into(),
            options: ids
                .iter()
                .map(|i| PermissionOption {
                    option_id: i.to_string(),
                    kind: None,
                    name: None,
                })
                .collect(),
            context: kind.map(|k| ApprovalContext {
                kind: Some(k.to_string()),
                ..Default::default()
            }),
        };
        assert_eq!(permission_shape(&mk(Some("switch_mode"), &[])), ApprovalShape::Plan);
        assert_eq!(
            permission_shape(&mk(None, &["approve", "reject"])),
            ApprovalShape::Plan
        );
        assert_eq!(permission_shape(&mk(None, &["allow_once"])), ApprovalShape::Tool);
        let question = PendingPermission {
            context: Some(ApprovalContext {
                tool_name: Some("AskUserQuestion".into()),
                ..Default::default()
            }),
            ..mk(None, &["optA", "optB"])
        };
        assert_eq!(permission_shape(&question), ApprovalShape::Question);
    }

    #[test]
    fn input_highlights_extract_the_common_keys_and_ignore_bad_json() {
        let mut ctx = ApprovalContext {
            raw_input_text: Some(
                json!({ "command": "rm -rf /tmp/x", "file_path": "/a/b.rs", "nested": { "a": 1 } })
                    .to_string(),
            ),
            ..Default::default()
        };
        let hs = raw_input_highlights(&ctx);
        assert_eq!(
            hs.iter().map(|h| h.label.as_str()).collect::<Vec<_>>(),
            vec!["command", "file_path"]
        );
        assert_eq!(hs[0].value, "rm -rf /tmp/x");

        ctx.raw_input_text = Some("not json {".into());
        assert!(raw_input_highlights(&ctx).is_empty());
        ctx.raw_input_text = None;
        assert!(raw_input_highlights(&ctx).is_empty());
    }

    // ---- elicitation form parsing ----

    #[test]
    fn elicitation_form_parses_questions_companions_and_drops_the_skip_sentinel() {
        let params = json!({
            "sessionId": "s1",
            "message": "Please answer 2 questions.",
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "q_0": {
                        "type": "string",
                        "title": "Which library?",
                        "oneOf": [
                            { "const": "serde", "title": "serde" },
                            { "const": "__skip__", "title": "Skip this question" }
                        ]
                    },
                    "q_0_other": { "type": "string", "title": "custom" },
                    "q_1": {
                        "type": "array",
                        "title": "Pick many",
                        "items": { "anyOf": [
                            { "const": "a", "title": "A" },
                            { "const": "b", "title": "B" }
                        ] }
                    },
                    "not_a_question": { "type": "string" }
                },
                "required": []
            }
        });
        let form = parse_elicitation(7, &params).expect("form");
        assert_eq!(form.session_id, "s1");
        assert_eq!(form.message, "Please answer 2 questions.");
        assert_eq!(form.fields.len(), 2);
        let q0 = &form.fields[0];
        assert_eq!(q0.other_key.as_deref(), Some("q_0_other"));
        assert!(!q0.multi);
        assert_eq!(
            q0.options.iter().map(|o| o.value.as_str()).collect::<Vec<_>>(),
            vec!["serde"]
        );
        let q1 = &form.fields[1];
        assert!(q1.multi);
        assert_eq!(q1.other_key, None, "no q_1_other property exists");
        assert_eq!(q1.options.len(), 2);
    }

    #[test]
    fn elicitation_form_without_question_fields_is_none() {
        let params = json!({
            "sessionId": "s1",
            "message": "x",
            "requestedSchema": { "type": "object", "properties": { "a": { "type": "string" } } }
        });
        assert!(parse_elicitation(1, &params).is_none());
        // Missing session / schema likewise.
        assert!(parse_elicitation(1, &json!({ "message": "x" })).is_none());
    }

    #[test]
    fn plan_approval_form_is_detected_as_the_single_approve_reject_field() {
        let params = json!({
            "sessionId": "s1",
            "message": "Ready to code?",
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "approval": {
                        "type": "string",
                        "title": "Decision",
                        "description": "# The full plan",
                        "oneOf": [
                            { "const": "approve", "title": "Approve" },
                            { "const": "reject", "title": "Reject" }
                        ]
                    }
                },
                "required": ["approval"]
            }
        });
        let form = parse_elicitation(3, &params).expect("form");
        let field = plan_field(&form).expect("plan field");
        assert_eq!(field.key, "approval");
        assert_eq!(field.description.as_deref(), Some("# The full plan"));
    }

    // ---- submit folding ----

    #[test]
    fn elicitation_submit_folds_picks_checks_and_free_text() {
        let fields = vec![
            ElicitField {
                key: "q_0".into(),
                other_key: Some("q_0_other".into()),
                question: "Q0".into(),
                description: None,
                multi: false,
                options: vec![ElicitOption {
                    value: "a".into(),
                    label: "A".into(),
                }],
            },
            ElicitField {
                key: "q_1".into(),
                other_key: None,
                question: "Q1".into(),
                description: None,
                multi: true,
                options: vec![
                    ElicitOption { value: "x".into(), label: "X".into() },
                    ElicitOption { value: "y".into(), label: "Y".into() },
                ],
            },
        ];
        let mut picked = HashMap::new();
        picked.insert("q_0".to_string(), "a".to_string());
        let mut checked = HashMap::new();
        checked.insert("x".to_string(), true);
        let mut others = HashMap::new();
        others.insert("q_0_other".to_string(), "  custom  ".to_string());

        let answers = build_elicitation_answers(&fields, &picked, &checked, &others);
        // Free text overrides the single-select pick and rides under its own
        // key; the multi field sends the checked values as an array.
        assert_eq!(
            answers,
            vec![
                ("q_0_other".to_string(), ElicitValue::One("custom".to_string())),
                ("q_1".to_string(), ElicitValue::Many(vec!["x".to_string()])),
            ]
        );

        let result = elicitation_result(Some(&answers));
        assert_eq!(result.pointer("/action"), Some(&json!("accept")));
        assert_eq!(result.pointer("/content/q_0_other"), Some(&json!("custom")));
        assert_eq!(result.pointer("/content/q_1"), Some(&json!(["x"])));
        // Decline (cancel / empty).
        assert_eq!(elicitation_result(None), json!({ "action": "decline" }));
        assert_eq!(
            elicitation_result(Some(&vec![])),
            json!({ "action": "decline" })
        );
    }

    // ---- race state machine ----

    fn permission_params(tool_call_id: &str, option_ids: &[&str]) -> Value {
        json!({
            "sessionId": "sess_1",
            "options": option_ids
                .iter()
                .map(|id| json!({ "optionId": id, "kind": "allow_once", "name": id }))
                .collect::<Vec<_>>(),
            "toolCall": {
                "toolCallId": tool_call_id,
                "title": "Bash: npm test",
                "rawInput": { "command": "npm test" }
            }
        })
    }

    #[test]
    fn permission_stores_per_session_and_answers_once() {
        let mut core = ApprovalCore::new();
        let report = core.on_request(
            10,
            "session/request_permission",
            &permission_params("tc_1", &["allow_once", "deny"]),
            &[],
            Some("sess_1"),
        );
        // Active session: no toast (the card itself is the surface).
        assert_eq!(report.toast, None);
        match report.ask {
            IncomingAsk::Stored {
                session_id,
                displaced_request_id,
            } => {
                assert_eq!(session_id, "sess_1");
                assert_eq!(displaced_request_id, None);
            }
            other => panic!("expected Stored, got {other:?}"),
        }
        assert!(core.has_pending("sess_1"));
        assert_eq!(
            core.pending_for_session("sess_1").map(|a| a.request_id()),
            Some(10)
        );

        // First answer wins locally: the result carries the selected option,
        // and a second answer for the same id is a no-op.
        let out = core.answer_permission(10, "allow_once").expect("answered");
        assert_eq!(out.result.pointer("/outcome/optionId"), Some(&json!("allow_once")));
        assert_eq!(out.cleared_sessions, vec!["sess_1"]);
        assert!(core.answer_permission(10, "deny").is_none());
        assert!(!core.has_pending("sess_1"));
    }

    #[test]
    fn another_sessions_ask_toasts_and_never_leaks_into_the_active_card() {
        let mut core = ApprovalCore::new();
        let report = core.on_request(
            11,
            "session/request_permission",
            &permission_params("tc_2", &["allow_once"]),
            &[],
            Some("sess_active"),
        );
        assert_eq!(
            report.toast.as_deref(),
            Some("approval requested: Bash: npm test")
        );
        assert!(core.pending_for_session("sess_active").is_none());
        assert!(core.pending_for_session("sess_1").is_some());
    }

    #[test]
    fn cancel_request_drops_the_card_without_answering() {
        let mut core = ApprovalCore::new();
        core.on_request(
            12,
            "session/request_permission",
            &permission_params("tc_3", &["allow_once"]),
            &[],
            Some("sess_1"),
        );
        let cleared = core.on_cancel_request(12);
        assert_eq!(cleared, vec!["sess_1"]);
        assert!(!core.has_pending("sess_1"));
        // Losing the race makes the late local answer a no-op.
        assert!(core.answer_permission(12, "allow_once").is_none());
    }

    #[test]
    fn ask_settled_dismisses_by_session_kind_and_tool_call_id() {
        let mut core = ApprovalCore::new();
        core.on_request(
            20,
            "session/request_permission",
            &permission_params("tc_4", &["approve", "reject"]),
            &[],
            Some("sess_1"),
        );
        // toolCallId mismatch: not ours, stays.
        assert!(core
            .on_ask_settled("sess_1", Some("permission"), Some("other_tc"))
            .is_empty());
        assert!(core.has_pending("sess_1"));
        // Matching id (or no id) drops it.
        let removed = core.on_ask_settled("sess_1", Some("permission"), Some("tc_4"));
        assert_eq!(removed, vec![20]);
        assert!(!core.has_pending("sess_1"));

        // Elicitation settles by kind (no toolCallId on that payload shape).
        let form = json!({
            "sessionId": "sess_1",
            "message": "Please answer the question.",
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "q_0": {
                        "type": "string",
                        "title": "Q",
                        "oneOf": [{ "const": "a", "title": "A" }]
                    }
                },
                "required": []
            }
        });
        core.on_request(21, "elicitation/create", &form, &[], Some("sess_1"));
        let removed = core.on_ask_settled("sess_1", Some("elicitation"), None);
        assert_eq!(removed, vec![21]);
    }

    #[test]
    fn resend_displaces_the_stale_entry_under_its_fresh_request_id() {
        let mut core = ApprovalCore::new();
        core.on_request(
            30,
            "session/request_permission",
            &permission_params("tc_5", &["allow_once"]),
            &[],
            Some("sess_1"),
        );
        // The bridge re-sends the still-undecided ask after our reconnect,
        // under a new id.
        let report = core.on_request(
            31,
            "session/request_permission",
            &permission_params("tc_5", &["allow_once"]),
            &[],
            Some("sess_1"),
        );
        match report.ask {
            IncomingAsk::Stored {
                displaced_request_id,
                ..
            } => assert_eq!(displaced_request_id, Some(30)),
            other => panic!("expected Stored, got {other:?}"),
        }
        // The old id no longer answers anything.
        assert!(core.answer_permission(30, "allow_once").is_none());
    }

    #[test]
    fn unparseable_elicitation_is_declined_and_unsupported_methods_stay_unanswered() {
        let mut core = ApprovalCore::new();
        let bad_form = json!({
            "sessionId": "s1",
            "message": "x",
            "requestedSchema": { "type": "object", "properties": {} }
        });
        let report = core.on_request(40, "elicitation/create", &bad_form, &[], Some("s1"));
        assert_eq!(
            report.ask,
            IncomingAsk::AnswerNow(json!({ "action": "decline" }))
        );

        let report = core.on_request(41, "session/other_interaction", &json!({}), &[], None);
        assert_eq!(report.ask, IncomingAsk::Unsupported);
    }

    #[test]
    fn plan_approval_form_answers_directly_with_the_approval_field() {
        let mut core = ApprovalCore::new();
        let params = json!({
            "sessionId": "s1",
            "message": "Ready to code?",
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "approval": {
                        "type": "string",
                        "description": "# Plan",
                        "oneOf": [
                            { "const": "approve", "title": "Approve" },
                            { "const": "reject", "title": "Reject" }
                        ]
                    }
                },
                "required": ["approval"]
            }
        });
        core.on_request(50, "elicitation/create", &params, &[], Some("s1"));
        let out = core
            .answer_elicitation(
                50,
                Some(&vec![("approval".to_string(), ElicitValue::One("approve".into()))]),
            )
            .expect("answered");
        assert_eq!(
            out.result,
            json!({ "action": "accept", "content": { "approval": "approve" } })
        );
        // Cancel declines the whole form.
        core.on_request(51, "elicitation/create", &params, &[], Some("s1"));
        let out = core.answer_elicitation(51, None).expect("answered");
        assert_eq!(out.result, json!({ "action": "decline" }));
    }

    #[test]
    fn toast_title_truncates_at_sixty_chars_and_falls_back_to_tool() {
        let long = "x".repeat(80);
        let params = json!({
            "sessionId": "other",
            "toolCall": { "toolCallId": "tc", "title": long }
        });
        let mut core = ApprovalCore::new();
        let report = core.on_request(60, "session/request_permission", &params, &[], Some("active"));
        assert_eq!(report.toast.unwrap().chars().count(), "approval requested: ".len() + 60);

        let bare = json!({ "sessionId": "other", "toolCall": { "toolCallId": "tc" } });
        let report = core.on_request(61, "session/request_permission", &bare, &[], Some("active"));
        assert_eq!(report.toast.as_deref(), Some("approval requested: tool"));
    }
}
