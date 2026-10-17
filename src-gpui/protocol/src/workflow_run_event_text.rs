//! Human-readable rendering for one dynamic-workflow journal event — port
//! of `src/components/config/workflowRunEventText.ts` (the mobile/web
//! implementation; the desktop client shares the semantics).
//!
//! The v4 journal contract deliberately does not restate the engine's payload
//! shapes ("readers interpret per event kind", upstream transport.ts), so the
//! client owes every event kind a line of its own. The vocabulary mirrors the
//! desktop run panel: a localized label plus a detail that carries identity
//! and data (site refs, actor names, message previews) and never needs
//! localization. Unknown kinds degrade to the raw type name with a short
//! payload preview — readable first, raw JSON only in the row expander.
//! Every payload read is defensive: a journal that arrives clipped or from a
//! newer engine must never take the page down.
//!
//! Localization is injected through the [`Translate`] trait (flat i18next
//! keys, `{{var}}` interpolation, `defaultValue` passthrough) so this crate
//! stays free of any i18n backend.

use serde_json::{Map, Value};

use crate::workflow::{WorkflowRunEvent, WorkflowRunEventPayload};

/// One-line detail cap — the row expander carries the full payload.
pub const DETAIL_MAX: usize = 200;

/// The i18n surface the renderer needs: `t(key, values)`. The catalog keys
/// are FLAT (`zconfig.wfStatusRunning`), so a dot in the key would make
/// i18next resolve a nested object that does not exist and fall back forever.
pub trait Translate {
    fn t(&self, key: &str, values: &[(&'static str, String)]) -> String;
}

/// Failure/cancel tint; everything else stays neutral.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunEventTone {
    Default,
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunEventDesc {
    /// Localized main label ("任务完成 · 成功").
    pub label: String,
    /// Identity + data (site@ordinal, actor name, message preview).
    pub detail: Option<String>,
    pub tone: RunEventTone,
    /// Set for unknown kinds: the UI renders the label in monospace.
    pub mono_label: Option<bool>,
}

// ─── payload accessors (defensive: nothing here may panic) ──────────────────

fn pstr(p: &WorkflowRunEventPayload, k: &str) -> Option<String> {
    p.get(k)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn pnum(p: &WorkflowRunEventPayload, k: &str) -> Option<f64> {
    p.get(k).and_then(Value::as_f64)
}

fn as_record(v: Option<&Value>) -> Option<&Map<String, Value>> {
    v.and_then(Value::as_object)
}

/// JS `String(n)` parity for the numbers these payloads carry (shared with
/// the run-detail models in `workflow_run_detail`).
pub(crate) fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

fn fmt_num_opt(n: Option<f64>) -> String {
    n.map(fmt_num).unwrap_or_else(|| "?".to_owned())
}

/// `n.toLocaleString()` (en-US grouping) — the usage line's token counts
/// (shared with the chart axis formatter in `workflow_run_detail`).
pub(crate) fn thousands(n: f64) -> String {
    let negative = n < 0.0;
    let int = (n.abs().trunc()) as u64;
    let frac = n.abs().fract();
    let digits = int.to_string();
    let grouped = digits
        .as_bytes()
        .rchunks(3)
        .rev()
        .map(std::str::from_utf8)
        .map(Result::unwrap)
        .collect::<Vec<_>>()
        .join(",");
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(&grouped);
    if frac > 0.0 {
        out.push_str(&format!("{}", frac).trim_start_matches("0").to_owned());
    }
    out
}

fn value_to_plain_string(v: &Value) -> String {
    match v {
        Value::Null => "?".to_owned(),
        Value::String(s) => s.clone(),
        Value::Number(n) => fmt_num(n.as_f64().unwrap_or(0.0)),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

// ─── ref helpers ─────────────────────────────────────────────────────────────

/// `{siteId, ordinal}` → `site@ordinal` — the engine's own refToString form,
/// so a row can be cross-read against backend logs.
pub fn ref_text(value: &Value) -> Option<String> {
    let obj = value.as_object()?;
    let site = obj.get("siteId").and_then(Value::as_str)?;
    if site.is_empty() {
        return None;
    }
    match obj.get("ordinal").and_then(Value::as_f64) {
        Some(ord) => Some(format!("{site}@{}", fmt_num(ord))),
        None => Some(site.to_owned()),
    }
}

/// Stable fold key for a node-* event's instance ref.
pub fn instance_key_of(p: &WorkflowRunEventPayload) -> String {
    p.get("instance")
        .and_then(ref_text_opt)
        .unwrap_or_else(|| "?".to_owned())
}

fn ref_text_opt(v: &Value) -> Option<String> {
    ref_text(v)
}

/// Stable fold key for an actor across replayed prefixes (resume re-hires).
pub fn actor_key_of(p: &WorkflowRunEventPayload) -> String {
    match p.get("actor").and_then(ref_text) {
        Some(r) => r,
        None => format!("name:{}", pstr(p, "name").unwrap_or_else(|| "?".to_owned())),
    }
}

// ─── detail assembly ─────────────────────────────────────────────────────────

fn join_detail(parts: Vec<Option<String>>) -> Option<String> {
    let kept: Vec<String> = parts
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    if kept.is_empty() {
        None
    } else {
        Some(kept.join(" · "))
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        let head: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

fn message_of(value: Option<&Value>) -> Option<String> {
    as_record(value).and_then(|o| {
        o.get("message")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    })
}

/// Compact one-line preview of a report item / unknown payload.
pub fn item_summary(item: &Value) -> Option<String> {
    match item {
        Value::Null => None,
        Value::String(s) => {
            if s.is_empty() {
                None
            } else {
                Some(truncate(s, DETAIL_MAX))
            }
        }
        other => serde_json::to_string(other)
            .ok()
            .map(|s| truncate(&s, DETAIL_MAX)),
    }
}

pub fn outcome_word(t: &dyn Translate, outcome: &Value) -> String {
    match outcome.as_str() {
        Some("ok") => t.t("zconfig.wfOutcomeOk", &[]),
        Some("failed") => t.t("zconfig.wfOutcomeFailed", &[]),
        Some("cancelled") => t.t("zconfig.wfOutcomeCancelled", &[]),
        _ => value_to_plain_string(outcome),
    }
}

/// Status token → localized word; unknown tokens stay verbatim (via the
/// catalog's `defaultValue` passthrough).
pub fn status_word(t: &dyn Translate, token: Option<&str>) -> String {
    let Some(token) = token.filter(|s| !s.is_empty()) else {
        return "?".to_owned();
    };
    let mut chars = token.chars();
    let key = match chars.next() {
        Some(first) => format!("zconfig.wfStatus{}{}", first.to_uppercase(), chars.as_str()),
        None => "zconfig.wfStatus".to_owned(),
    };
    t.t(&key, &[("defaultValue", token.to_owned())])
}

/// Short qid tag — the tail is the only part that tells two asks apart.
fn qid_tag(p: &WorkflowRunEventPayload) -> Option<String> {
    let qid = pstr(p, "qid")?;
    let tail: String = qid
        .chars()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Some(format!("…{tail}"))
}

/// Options for [`describe_run_event_with`].
#[derive(Debug, Clone, Copy, Default)]
pub struct DescribeOptions {
    /// Drop the `site@ordinal` ref from the detail — rows nested under their
    /// own step already say which instance they belong to.
    pub omit_site: bool,
}

/// One journal event → one readable line. The field names per kind come from
/// the engine's RunEvent union (dynamic-workflow/src/engine/types.ts).
pub fn describe_run_event(t: &dyn Translate, ev: &WorkflowRunEvent) -> RunEventDesc {
    describe_run_event_with(t, ev, DescribeOptions::default())
}

pub fn describe_run_event_with(
    t: &dyn Translate,
    ev: &WorkflowRunEvent,
    opts: DescribeOptions,
) -> RunEventDesc {
    let p = &ev.payload;
    let site = if opts.omit_site {
        None
    } else {
        p.get("instance").and_then(ref_text)
    };
    match ev.r#type.as_str() {
        "run-started" => RunEventDesc {
            label: t.t("zconfig.wfEvRunStarted", &[]),
            detail: None,
            tone: RunEventTone::Default,
            mono_label: None,
        },

        "actor-created" => RunEventDesc {
            label: t.t("zconfig.wfEvActorCreated", &[]),
            detail: join_detail(vec![pstr(p, "name"), p.get("actor").and_then(ref_text)]),
            tone: RunEventTone::Default,
            mono_label: None,
        },

        "node-queued" => RunEventDesc {
            label: t.t("zconfig.wfEvNodeQueued", &[]),
            detail: join_detail(vec![pstr(p, "actorName"), site.clone(), pstr(p, "kind")]),
            tone: RunEventTone::Default,
            mono_label: None,
        },

        "node-dispatched" => RunEventDesc {
            label: t.t("zconfig.wfEvNodeDispatched", &[]),
            detail: join_detail(vec![pstr(p, "actorName"), site.clone()]),
            tone: RunEventTone::Default,
            mono_label: None,
        },

        "node-executing" => RunEventDesc {
            label: t.t("zconfig.wfEvNodeExecuting", &[]),
            detail: site.clone(),
            tone: RunEventTone::Default,
            mono_label: None,
        },

        "node-waiting" => {
            let label = if pstr(p, "cause").as_deref() == Some("backoff") {
                t.t(
                    "zconfig.wfEvNodeWaitingBackoff",
                    &[(
                        "reason",
                        pstr(p, "reason").unwrap_or_else(|| "?".to_owned()),
                    )],
                )
            } else {
                t.t("zconfig.wfEvNodeWaiting", &[])
            };
            RunEventDesc {
                label,
                detail: site.clone(),
                tone: RunEventTone::Default,
                mono_label: None,
            }
        }

        "node-nudged" => RunEventDesc {
            label: t.t("zconfig.wfEvNodeNudged", &[]),
            detail: site.clone(),
            tone: RunEventTone::Default,
            mono_label: None,
        },

        "node-repairing" => RunEventDesc {
            label: t.t(
                "zconfig.wfEvNodeRepairing",
                &[("n", fmt_num_opt(pnum(p, "attempt")))],
            ),
            detail: site.clone(),
            tone: RunEventTone::Default,
            mono_label: None,
        },

        "node-progress" => {
            let tool = as_record(p.get("lastTool")).and_then(|tool| {
                join_detail(vec![
                    pstr(tool, "name"),
                    pstr(tool, "target").map(|target| truncate(&target, 80)),
                ])
            });
            RunEventDesc {
                label: t.t(
                    "zconfig.wfEvNodeProgress",
                    &[("turn", fmt_num_opt(pnum(p, "turn")))],
                ),
                detail: join_detail(vec![site.clone(), tool]),
                tone: RunEventTone::Default,
                mono_label: None,
            }
        }

        "node-settled" => {
            let outcome = p
                .get("outcome")
                .filter(|v| matches!(v.as_str(), Some("ok") | Some("failed") | Some("cancelled")));
            let label = t.t(
                if p.get("cached") == Some(&Value::Bool(true)) {
                    "zconfig.wfEvNodeSettledCached"
                } else {
                    "zconfig.wfEvNodeSettled"
                },
                &[(
                    "outcome",
                    outcome
                        .map(|o| outcome_word(t, o))
                        .unwrap_or_else(|| "?".to_owned()),
                )],
            );
            let failed = matches!(
                outcome.and_then(Value::as_str),
                Some("failed") | Some("cancelled")
            );
            RunEventDesc {
                label,
                detail: join_detail(vec![site.clone(), message_of(p.get("error"))]),
                tone: if failed {
                    RunEventTone::Failed
                } else {
                    RunEventTone::Default
                },
                mono_label: None,
            }
        }

        "usage-updated" => {
            let spent = pnum(p, "spentTokens");
            RunEventDesc {
                label: t.t("zconfig.wfEvUsageUpdated", &[]),
                detail: spent.map(|n| format!("{} tokens", thousands(n))),
                tone: RunEventTone::Default,
                mono_label: None,
            }
        }

        "log" => {
            let message = pstr(p, "message");
            RunEventDesc {
                label: t.t("zconfig.wfEvLog", &[]),
                detail: message.map(|m| truncate(&m, DETAIL_MAX)),
                tone: RunEventTone::Default,
                mono_label: None,
            }
        }

        "phase-entered" => {
            let name = pstr(p, "name");
            let ordinal = pnum(p, "ordinal");
            RunEventDesc {
                label: t.t("zconfig.wfEvPhaseEntered", &[]),
                detail: join_detail(vec![name, ordinal.filter(|o| *o >= 2.0).map(fmt_num)]),
                tone: RunEventTone::Default,
                mono_label: None,
            }
        }

        "report" => RunEventDesc {
            label: t.t("zconfig.wfEvReport", &[]),
            detail: join_detail(vec![site.clone(), p.get("item").and_then(item_summary)]),
            tone: RunEventTone::Default,
            mono_label: None,
        },

        "artifact-published" => {
            let art = as_record(p.get("artifact"));
            let version = art
                .and_then(|a| a.get("version"))
                .and_then(Value::as_f64)
                .map(|v| format!("v{}", fmt_num(v)));
            RunEventDesc {
                label: t.t("zconfig.wfEvArtifactPublished", &[]),
                detail: match art {
                    Some(a) => join_detail(vec![pstr(a, "id"), version]),
                    None => pstr(p, "id"),
                },
                tone: RunEventTone::Default,
                mono_label: None,
            }
        }

        "artifact-failed" => RunEventDesc {
            label: t.t("zconfig.wfEvArtifactFailed", &[]),
            detail: join_detail(vec![pstr(p, "id"), message_of(p.get("error"))]),
            tone: RunEventTone::Failed,
            mono_label: None,
        },

        "compaction" => RunEventDesc {
            label: t.t("zconfig.wfEvCompaction", &[]),
            detail: p.get("actor").and_then(ref_text),
            tone: RunEventTone::Default,
            mono_label: None,
        },

        "import-cache-closed" => RunEventDesc {
            label: t.t("zconfig.wfEvImportCacheClosed", &[]),
            detail: join_detail(vec![pstr(p, "actorName"), site.clone()]),
            tone: RunEventTone::Default,
            mono_label: None,
        },

        "escalation-raised" => {
            let question = pstr(p, "question");
            RunEventDesc {
                label: t.t("zconfig.wfEvEscalationRaised", &[]),
                detail: join_detail(vec![
                    qid_tag(p),
                    pstr(p, "actorName").or_else(|| p.get("actor").and_then(ref_text)),
                    question.map(|q| truncate(&q, DETAIL_MAX)),
                ]),
                tone: RunEventTone::Default,
                mono_label: None,
            }
        }

        "escalation-resolved" => {
            let answer = pstr(p, "answer");
            RunEventDesc {
                label: t.t("zconfig.wfEvEscalationResolved", &[]),
                detail: join_detail(vec![qid_tag(p), answer.map(|a| truncate(&a, DETAIL_MAX))]),
                tone: RunEventTone::Default,
                mono_label: None,
            }
        }

        "concurrency-changed" => RunEventDesc {
            label: t.t("zconfig.wfEvConcurrencyChanged", &[]),
            detail: join_detail(vec![
                pstr(p, "key"),
                Some(format!(
                    "{} → {}",
                    fmt_num_opt(pnum(p, "previous")),
                    fmt_num_opt(pnum(p, "next"))
                )),
                pstr(p, "reason"),
            ]),
            tone: RunEventTone::Default,
            mono_label: None,
        },

        "run-caps-changed" => {
            let cap = |v: Option<&Value>| -> Option<String> {
                as_record(v)
                    .and_then(|o| o.get("maxConcurrency"))
                    .and_then(Value::as_f64)
                    .map(fmt_num)
            };
            let prev = cap(p.get("previous"));
            let next = cap(p.get("caps"));
            RunEventDesc {
                label: t.t("zconfig.wfEvRunCapsChanged", &[]),
                detail: match (prev, next) {
                    (Some(p), Some(n)) => Some(format!("{p} → {n}")),
                    (None, Some(n)) => Some(n),
                    _ => None,
                },
                tone: RunEventTone::Default,
                mono_label: None,
            }
        }

        "run-stalled" => RunEventDesc {
            label: t.t("zconfig.wfEvRunStalled", &[]),
            detail: item_summary(&Value::Object(p.clone())),
            tone: RunEventTone::Default,
            mono_label: None,
        },

        "run-settled" => {
            let status = pstr(p, "status");
            let superseded_by = pstr(p, "supersededBy");
            let failed = matches!(status.as_deref(), Some("errored") | Some("stopped"));
            RunEventDesc {
                label: t.t(
                    "zconfig.wfEvRunSettled",
                    &[("status", status_word(t, status.as_deref()))],
                ),
                detail: join_detail(vec![
                    pstr(p, "stopReason"),
                    message_of(p.get("error")),
                    superseded_by.map(|s| {
                        let tail: String = s
                            .chars()
                            .rev()
                            .take(8)
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                            .collect();
                        format!("→ …{tail}")
                    }),
                ]),
                tone: if failed {
                    RunEventTone::Failed
                } else {
                    RunEventTone::Default
                },
                mono_label: None,
            }
        }

        _ => RunEventDesc {
            label: ev.r#type.clone(),
            detail: item_summary(&Value::Object(p.clone())),
            tone: RunEventTone::Default,
            mono_label: Some(true),
        },
    }
}

// ─── Slice folding (the progress tab's reducer) ──────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeState {
    Queued,
    Executing,
    Waiting,
    Repairing,
    Ok,
    Failed,
    Cancelled,
}

impl NodeState {
    /// Settled outcomes are final — replayed lifecycle noise must not reopen
    /// one.
    pub fn is_settled(self) -> bool {
        matches!(
            self,
            NodeState::Ok | NodeState::Failed | NodeState::Cancelled
        )
    }
}

/// A node's folded lifecycle: one line per instance, LATEST state wins.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeAgg {
    pub key: String,
    pub site: String,
    pub ordinal: Option<f64>,
    pub state: NodeState,
    /// Subagent display name (node-dispatched carries actorName).
    pub label: Option<String>,
    /// Author instruction head (node-queued / node-dispatched carry it).
    pub head: Option<String>,
    /// Latest observed activity (progress tool, wait cause, repair attempt).
    pub activity: Option<String>,
    pub error: Option<String>,
    /// The step's own journal rows (its lifecycle plus the reports it
    /// filed), oldest first — the detail a collapsed step row expands into.
    pub events: Vec<WorkflowRunEvent>,
}

pub fn node_state_word(t: &dyn Translate, state: NodeState) -> Option<String> {
    match state {
        NodeState::Ok => Some(t.t("zconfig.wfOutcomeOk", &[])),
        NodeState::Failed => Some(t.t("zconfig.wfOutcomeFailed", &[])),
        NodeState::Cancelled => Some(t.t("zconfig.wfOutcomeCancelled", &[])),
        NodeState::Queued => Some(t.t("zconfig.wfNodeStateQueued", &[])),
        NodeState::Waiting => Some(t.t("zconfig.wfNodeStateWaiting", &[])),
        NodeState::Repairing => Some(t.t("zconfig.wfNodeStateRepairing", &[])),
        // executing: the pulse dot already says "live"
        NodeState::Executing => None,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActorLine {
    pub key: String,
    pub label: String,
}

/// The folded view of one slice (the span between two phase-entered
/// markers).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SliceFold {
    pub actors: Vec<ActorLine>,
    pub nodes: Vec<NodeAgg>,
    pub others: Vec<WorkflowRunEvent>,
    pub settled: Option<WorkflowRunEvent>,
    pub spent_tokens: Option<f64>,
}

/// Fold one slice's events. A node's identity is its `instance` ref
/// (`{siteId, ordinal}` — the engine's own InstanceRef), NOT a top-level
/// siteId field: reading the wrong spelling collapses every node onto one
/// empty key. node-* lifecycles fold per instance into a single line (latest
/// state; settled is final), actor-created becomes a hiring line, usage
/// updates collapse to the newest total, run-settled closes the section, and
/// the rest stay as journal rows for [`describe_run_event`].
pub fn reduce_slice(t: &dyn Translate, events: &[WorkflowRunEvent]) -> SliceFold {
    let mut fold = SliceFold::default();
    // Insertion-ordered like the TS Map: replaces keep their slot.
    let mut nodes: Vec<NodeAgg> = Vec::new();
    for ev in events {
        let p = &ev.payload;
        if ev.r#type == "actor-created" {
            fold.actors.push(ActorLine {
                key: actor_key_of(p),
                label: pstr(p, "name")
                    .or_else(|| p.get("actor").and_then(ref_text))
                    .unwrap_or_else(|| "?".to_owned()),
            });
            continue;
        }
        if ev.r#type == "run-settled" {
            fold.settled = Some(ev.clone());
            continue;
        }
        if ev.r#type == "usage-updated" {
            if let Some(spent) = pnum(p, "spentTokens") {
                fold.spent_tokens = Some(spent);
            }
            continue;
        }
        if ev.r#type.starts_with("node-") {
            let key = instance_key_of(p);
            let reference = as_record(p.get("instance")).cloned().unwrap_or_default();
            let prev = nodes.iter().find(|n| n.key == key);
            if prev.is_some_and(|n| n.state.is_settled()) {
                continue;
            }
            let mut state = prev.map(|n| n.state).unwrap_or(NodeState::Queued);
            let mut label = prev.and_then(|n| n.label.clone());
            let mut head = prev.and_then(|n| n.head.clone());
            let mut activity = prev.and_then(|n| n.activity.clone());
            let mut error = prev.and_then(|n| n.error.clone());
            let mut own = prev.map(|n| n.events.clone()).unwrap_or_default();
            own.push(ev.clone());
            match ev.r#type.as_str() {
                "node-queued" => {
                    state = NodeState::Queued;
                    if head.is_none() {
                        head = pstr(p, "instructionsHead");
                    }
                }
                "node-dispatched" => {
                    state = NodeState::Executing;
                    if label.is_none() {
                        label = pstr(p, "actorName");
                    }
                    if head.is_none() {
                        head = pstr(p, "instructionsHead");
                    }
                    activity = None;
                }
                "node-executing" => {
                    // waiting ⇄ executing is a self-loop; leaving the wait
                    // must clear the wait activity or the rest of the
                    // generation reads "waiting".
                    if matches!(state, NodeState::Queued | NodeState::Waiting) {
                        state = NodeState::Executing;
                        activity = None;
                    }
                }
                "node-waiting" => {
                    state = NodeState::Waiting;
                    activity = if pstr(p, "cause").as_deref() == Some("backoff") {
                        Some(t.t(
                            "zconfig.wfWaitBackoff",
                            &[(
                                "reason",
                                pstr(p, "reason").unwrap_or_else(|| "?".to_owned()),
                            )],
                        ))
                    } else {
                        Some(t.t("zconfig.wfWaitSlot", &[]))
                    };
                }
                "node-progress" => {
                    state = NodeState::Executing;
                    let tool = as_record(p.get("lastTool"));
                    let tool_name = tool.and_then(|tool| pstr(tool, "name"));
                    let tool_target = tool.and_then(|tool| pstr(tool, "target"));
                    let turn = pnum(p, "turn");
                    activity = if let Some(name) = tool_name {
                        let target = tool_target.map(|target| truncate(&target, 60));
                        Some(match target {
                            Some(target) => format!("{name} {target}"),
                            None => name,
                        })
                    } else if let Some(turn) = turn {
                        Some(t.t("zconfig.wfNodeTurn", &[("n", fmt_num(turn))]))
                    } else {
                        activity
                    };
                }
                "node-repairing" => {
                    state = NodeState::Repairing;
                    activity = Some(t.t(
                        "zconfig.wfNodeRepairAttempt",
                        &[("n", fmt_num_opt(pnum(p, "attempt")))],
                    ));
                }
                "node-nudged" => {
                    activity = Some(t.t("zconfig.wfNodeNudgedShort", &[]));
                }
                "node-settled" => {
                    state = match p.get("outcome").and_then(Value::as_str) {
                        Some("ok") => NodeState::Ok,
                        Some("failed") => NodeState::Failed,
                        Some("cancelled") => NodeState::Cancelled,
                        _ => NodeState::Ok,
                    };
                    activity = if p.get("cached") == Some(&Value::Bool(true)) {
                        Some(t.t("zconfig.wfNodeCached", &[]))
                    } else {
                        None
                    };
                    error = message_of(p.get("error"));
                }
                _ => {}
            }
            let site = reference
                .get("siteId")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| key.clone());
            let ordinal = reference.get("ordinal").and_then(Value::as_f64);
            let next = NodeAgg {
                key: key.clone(),
                site,
                ordinal,
                state,
                label,
                head,
                activity,
                error,
                events: own,
            };
            match nodes.iter_mut().find(|n| n.key == key) {
                Some(slot) => *slot = next,
                None => nodes.push(next),
            }
            continue;
        }
        if ev.r#type == "report" || ev.r#type == "import-cache-closed" {
            // Filed by a step this slice knows → the step's own detail. A
            // script-level row (no instance ref) stays a phase row.
            if let Some(site) = p.get("instance").and_then(ref_text) {
                if let Some(slot) = nodes.iter_mut().find(|n| n.key == site) {
                    slot.events.push(ev.clone());
                    continue;
                }
            }
        }
        fold.others.push(ev.clone());
    }
    fold.nodes = nodes;
    fold
}

/// Sorts highest sequence first (TS `newestFirst`).
pub fn newest_first(a: &WorkflowRunEvent, b: &WorkflowRunEvent) -> std::cmp::Ordering {
    b.sequence.cmp(&a.sequence)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Minimal i18next stand-in: a two-entry catalog so `defaultValue`
    /// fallbacks (statusWord's unknown-token passthrough) behave like the
    /// real thing, and every other key interpolates its own id so assertions
    /// read like the calls. (Port of the vitest helper.)
    struct Catalog;

    impl Translate for Catalog {
        fn t(&self, key: &str, values: &[(&'static str, String)]) -> String {
            const CATALOG: [(&str, &str); 2] = [
                ("zconfig.wfStatusRunning", "运行中"),
                ("zconfig.wfStatusCompleted", "已完成"),
            ];
            match CATALOG.iter().find(|(k, _)| *k == key) {
                Some((_, template)) => interpolate(template, values),
                None => values
                    .iter()
                    .find(|(k, _)| *k == "defaultValue")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_else(|| key.to_string()),
            }
        }
    }

    /// `template.replace(/\{\{(\w+)\}\}/g, …)` — missing vars render "?".
    fn interpolate(template: &str, values: &[(&'static str, String)]) -> String {
        let mut out = String::new();
        let mut rest = template;
        while let Some(start) = rest.find("{{") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            match after.find("}}") {
                Some(end) => {
                    let name = &after[..end];
                    if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                        let value = values
                            .iter()
                            .find(|(k, _)| *k == name)
                            .map(|(_, v)| v.clone())
                            .unwrap_or_else(|| "?".to_owned());
                        out.push_str(&value);
                    } else {
                        out.push_str("{{");
                        out.push_str(name);
                        out.push_str("}}");
                    }
                    rest = &after[end + 2..];
                }
                None => {
                    out.push_str(&rest[start..]);
                    rest = "";
                }
            }
        }
        out.push_str(rest);
        out
    }

    fn ev(sequence: u64, typ: &str, payload: Value) -> WorkflowRunEvent {
        WorkflowRunEvent {
            sequence,
            r#type: typ.to_owned(),
            payload: payload.as_object().cloned().unwrap_or_default(),
            truncated: None,
        }
    }

    fn instance(site_id: &str, ordinal: i64) -> Value {
        json!({ "siteId": site_id, "ordinal": ordinal })
    }

    // ── refText ────────────────────────────────────────────────────────────

    #[test]
    fn ref_text_renders_the_engine_site_ordinal_form() {
        assert_eq!(
            ref_text(&instance("review#3", 2)).as_deref(),
            Some("review#3@2")
        );
    }

    #[test]
    fn ref_text_degrades_on_anything_that_is_not_a_ref() {
        assert_eq!(ref_text(&Value::Null), None);
        assert_eq!(ref_text(&json!("review#3")), None);
        assert_eq!(ref_text(&json!({ "ordinal": 1 })), None);
    }

    // ── describeRunEvent ───────────────────────────────────────────────────

    #[test]
    fn reads_node_identity_from_the_instance_ref_not_a_top_level_site_id() {
        let desc = describe_run_event(
            &Catalog,
            &ev(
                1,
                "node-dispatched",
                json!({ "instance": instance("scan", 4), "actorName": "scout" }),
            ),
        );
        let detail = desc.detail.expect("detail");
        assert!(detail.contains("scout"), "detail: {detail}");
        assert!(detail.contains("scan@4"), "detail: {detail}");
        assert_eq!(desc.mono_label, None);
    }

    #[test]
    fn tints_failed_settlements_and_carries_the_error_message() {
        let desc = describe_run_event(
            &Catalog,
            &ev(
                2,
                "node-settled",
                json!({
                    "instance": instance("scan", 4),
                    "outcome": "failed",
                    "error": { "code": "EIO", "message": "disk went away" }
                }),
            ),
        );
        assert_eq!(desc.tone, RunEventTone::Failed);
        let detail = desc.detail.expect("detail");
        assert!(detail.contains("disk went away"), "detail: {detail}");
    }

    #[test]
    fn localizes_run_settlement_status_stopped_errored_read_as_failed() {
        let ok = describe_run_event(
            &Catalog,
            &ev(3, "run-settled", json!({ "status": "completed" })),
        );
        assert_eq!(ok.tone, RunEventTone::Default);
        assert_eq!(ok.label, "zconfig.wfEvRunSettled"); // unknown key passes through
        let stopped = describe_run_event(
            &Catalog,
            &ev(
                4,
                "run-settled",
                json!({ "status": "stopped", "stopReason": "user" }),
            ),
        );
        assert_eq!(stopped.tone, RunEventTone::Failed);
        let detail = stopped.detail.expect("detail");
        assert!(detail.contains("user"), "detail: {detail}");
    }

    #[test]
    fn unknown_kinds_keep_the_raw_type_name_in_monospace_with_a_short_preview() {
        let blob: String = "x".repeat(400);
        let unknown = describe_run_event(&Catalog, &ev(6, "future-event", json!({ "blob": blob })));
        assert_eq!(unknown.label, "future-event");
        assert_eq!(unknown.mono_label, Some(true));
        assert!(
            unknown
                .detail
                .as_ref()
                .is_some_and(|d| d.chars().count() <= 200),
            "detail: {:?}",
            unknown.detail
        );
    }

    // ── statusWord ─────────────────────────────────────────────────────────

    #[test]
    fn status_word_maps_known_tokens_and_passes_unknown_ones_through() {
        assert_eq!(status_word(&Catalog, Some("running")), "运行中");
        assert_eq!(status_word(&Catalog, Some("completed")), "已完成");
        assert_eq!(
            status_word(&Catalog, Some("brand-new-state")),
            "brand-new-state"
        );
        assert_eq!(status_word(&Catalog, None), "?");
    }

    // ── reduceSlice ────────────────────────────────────────────────────────

    #[test]
    fn folds_one_node_lifecycle_into_a_single_latest_state_line() {
        let out = reduce_slice(
            &Catalog,
            &[
                ev(
                    1,
                    "node-queued",
                    json!({ "instance": instance("scan", 1), "instructionsHead": "scan the tree" }),
                ),
                ev(
                    2,
                    "node-dispatched",
                    json!({
                        "instance": instance("scan", 1),
                        "actorName": "scout",
                        "instructionsHead": "scan the tree"
                    }),
                ),
                ev(
                    3,
                    "node-progress",
                    json!({
                        "instance": instance("scan", 1),
                        "turn": 2,
                        "lastTool": { "name": "Grep", "target": "src/**" }
                    }),
                ),
                ev(
                    4,
                    "node-settled",
                    json!({ "instance": instance("scan", 1), "outcome": "ok" }),
                ),
            ],
        );
        assert_eq!(out.nodes.len(), 1);
        let node = &out.nodes[0];
        assert_eq!(node.state, NodeState::Ok);
        assert_eq!(node.label.as_deref(), Some("scout"));
        assert_eq!(node.head.as_deref(), Some("scan the tree"));
    }

    #[test]
    fn keys_nodes_by_instance_ref_two_instances_stay_two_lines() {
        let out = reduce_slice(
            &Catalog,
            &[
                ev(1, "node-queued", json!({ "instance": instance("ask", 1) })),
                ev(2, "node-queued", json!({ "instance": instance("ask", 2) })),
                ev(
                    3,
                    "node-settled",
                    json!({ "instance": instance("ask", 1), "outcome": "ok" }),
                ),
            ],
        );
        assert_eq!(out.nodes.len(), 2);
        let mut states: Vec<_> = out.nodes.iter().map(|n| n.state).collect();
        states.sort_by_key(|s| format!("{s:?}"));
        assert_eq!(states, vec![NodeState::Ok, NodeState::Queued]);
    }

    #[test]
    fn settled_is_final_replayed_lifecycle_noise_must_not_reopen_it() {
        let out = reduce_slice(
            &Catalog,
            &[
                ev(
                    1,
                    "node-settled",
                    json!({ "instance": instance("ask", 1), "outcome": "failed" }),
                ),
                ev(
                    2,
                    "node-progress",
                    json!({ "instance": instance("ask", 1), "turn": 9 }),
                ),
            ],
        );
        assert_eq!(out.nodes[0].state, NodeState::Failed);
    }

    #[test]
    fn usage_updates_collapse_to_the_newest_total_actors_keep_names() {
        let out = reduce_slice(
            &Catalog,
            &[
                ev(
                    1,
                    "actor-created",
                    json!({ "actor": instance("a", 1), "name": "scout" }),
                ),
                ev(2, "usage-updated", json!({ "spentTokens": 1000 })),
                ev(3, "usage-updated", json!({ "spentTokens": 2500 })),
                ev(4, "log", json!({ "message": "hello" })),
            ],
        );
        assert_eq!(
            out.actors,
            vec![ActorLine {
                key: "a@1".to_owned(),
                label: "scout".to_owned()
            }]
        );
        assert_eq!(out.spent_tokens, Some(2500.0));
        assert_eq!(out.others.len(), 1); // the log stays a readable row
    }

    #[test]
    fn leaving_the_wait_state_clears_the_wait_activity() {
        let out = reduce_slice(
            &Catalog,
            &[
                ev(
                    1,
                    "node-dispatched",
                    json!({ "instance": instance("ask", 1) }),
                ),
                ev(
                    2,
                    "node-waiting",
                    json!({ "instance": instance("ask", 1), "cause": "slot" }),
                ),
                ev(
                    3,
                    "node-executing",
                    json!({ "instance": instance("ask", 1) }),
                ),
            ],
        );
        assert_eq!(out.nodes[0].state, NodeState::Executing);
        assert_eq!(out.nodes[0].activity, None);
    }

    #[test]
    fn keeps_run_settled_as_the_sections_closing_event() {
        let out = reduce_slice(
            &Catalog,
            &[ev(1, "run-settled", json!({ "status": "completed" }))],
        );
        assert_eq!(
            out.settled.as_ref().map(|e| e.r#type.clone()),
            Some("run-settled".to_owned())
        );
    }

    #[test]
    fn keeps_each_steps_own_rows_and_nests_its_reports() {
        let out = reduce_slice(
            &Catalog,
            &[
                ev(
                    1,
                    "node-queued",
                    json!({ "instance": instance("scan", 1), "instructionsHead": "scan the repo" }),
                ),
                ev(
                    2,
                    "node-dispatched",
                    json!({ "instance": instance("scan", 1), "actorName": "scout" }),
                ),
                ev(
                    3,
                    "node-progress",
                    json!({ "instance": instance("scan", 1), "turn": 2 }),
                ),
                ev(
                    4,
                    "report",
                    json!({ "instance": instance("scan", 1), "item": "filed" }),
                ),
                ev(5, "report", json!({ "item": "script-level" })),
                ev(6, "log", json!({ "message": "narration" })),
                ev(
                    7,
                    "import-cache-closed",
                    json!({ "instance": instance("scan", 1), "actorName": "scout" }),
                ),
                ev(
                    8,
                    "node-settled",
                    json!({ "instance": instance("scan", 1), "outcome": "ok" }),
                ),
            ],
        );
        assert_eq!(out.nodes.len(), 1);
        let seqs: Vec<u64> = out.nodes[0].events.iter().map(|e| e.sequence).collect();
        assert_eq!(seqs, vec![1, 2, 3, 4, 7, 8]);
        // A report without an instance ref belongs to the script, not a step.
        let other_seqs: Vec<u64> = out.others.iter().map(|e| e.sequence).collect();
        assert_eq!(other_seqs, vec![5, 6]);
    }

    #[test]
    fn a_settled_step_keeps_its_history_and_ignores_replayed_rows() {
        let out = reduce_slice(
            &Catalog,
            &[
                ev(
                    1,
                    "node-dispatched",
                    json!({ "instance": instance("scan", 1) }),
                ),
                ev(
                    2,
                    "node-settled",
                    json!({ "instance": instance("scan", 1), "outcome": "ok" }),
                ),
                ev(
                    3,
                    "node-dispatched",
                    json!({ "instance": instance("scan", 1) }),
                ),
                ev(
                    4,
                    "report",
                    json!({ "instance": instance("scan", 1), "item": "late" }),
                ),
            ],
        );
        assert_eq!(out.nodes[0].state, NodeState::Ok);
        let seqs: Vec<u64> = out.nodes[0].events.iter().map(|e| e.sequence).collect();
        assert_eq!(seqs, vec![1, 2, 4]);
    }

    #[test]
    fn describe_run_event_with_omits_the_site_ref_for_nested_rows() {
        let e = ev(
            3,
            "node-progress",
            json!({
                "instance": instance("scan", 4),
                "turn": 2,
                "lastTool": { "name": "Read", "target": "a.ts" },
            }),
        );
        let plain = describe_run_event(&Catalog, &e);
        assert!(plain.detail.as_deref().unwrap().contains("scan@4"));
        let nested = describe_run_event_with(&Catalog, &e, DescribeOptions { omit_site: true });
        let detail = nested.detail.unwrap();
        assert!(!detail.contains("scan@4"));
        assert!(detail.contains("Read"));
    }

    #[test]
    fn newest_first_sorts_descending() {
        let mut rows = vec![
            ev(2, "log", json!({})),
            ev(5, "log", json!({})),
            ev(1, "log", json!({})),
        ];
        rows.sort_by(newest_first);
        let seqs: Vec<u64> = rows.iter().map(|e| e.sequence).collect();
        assert_eq!(seqs, vec![5, 2, 1]);
    }
}
