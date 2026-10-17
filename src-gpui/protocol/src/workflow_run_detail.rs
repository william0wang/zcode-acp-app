//! Pure halves of one run's detail view — port of
//! `src/components/config/WorkflowRunDetail.tsx` (the phase fold and slicing,
//! the workspace row text, the node-result shape) and
//! `src/components/config/WorkflowArtifactViews.tsx` (the four preset
//! artifact kinds' spec-driven models plus the shared fallbacks). The
//! desktop client renders from these models; nothing here touches UI.
//!
//! Every spec read is defensive (the bridge passes the declaration specs
//! through untouched — the protocol does not restate their shapes): a
//! malformed or absent spec degrades to the key-value fallback, never to a
//! panic. A journal that arrives clipped or from a newer engine must never
//! take the page down (TS's own contract, WorkflowRunDetail.tsx header).

use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::engine::DecodePaddingMode;
use base64::Engine as _;
use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::workflow::{
    WorkflowArtifact, WorkflowArtifactItem, WorkflowNodeSummary, WorkflowRunEvent,
    WorkflowRunEventPayload, WorkflowWorkspaceNode,
};
use crate::workflow_run_event_text::{
    actor_key_of, fmt_num, instance_key_of, newest_first, thousands, SliceFold,
};

/// Pretty JSON cap (TS `JSON_CAP`): a huge payload must not wedge the layout.
pub const JSON_CAP: usize = 20_000;
/// The artifact text chunk ask (TS `READ_CHUNK`): ≤512KiB is the upstream
/// per-read maximum anyway.
pub const READ_CHUNK: u64 = 256 * 1024;
/// The workspace row's WHAT cap (TS `argsText`'s 160-char slice).
const ARGS_TEXT_MAX: usize = 160;
/// One-line cell cap for object values (TS `cellText`'s 80-char slice).
const CELL_TEXT_MAX: usize = 80;
/// The binary verdict threshold (TS: >1% U+FFFD across the whole text once
/// the chunks stop coming).
const BINARY_BAD_RATIO: f64 = 0.01;

// ─── shared value helpers (defensive: nothing here may panic) ───────────────

/// A plain JSON record (TS `isRecord`: object, non-null, non-array).
pub fn is_record(v: &Value) -> bool {
    v.as_object().is_some()
}

/// JS `String(v)` parity for scalars; objects fall back to their JSON form.
fn js_string(v: &Value) -> String {
    match v {
        Value::Null => "null".to_owned(),
        Value::String(s) => s.clone(),
        Value::Number(n) => fmt_num(n.as_f64().unwrap_or(0.0)),
        Value::Bool(b) => b.to_string(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "?".to_owned()),
    }
}

/// Short cell text (TS `cellText`): scalars verbatim, null empty, objects as
/// one-line compact JSON capped at 80 chars.
pub fn cell_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Object(_) | Value::Array(_) => match serde_json::to_string(v) {
            Ok(s) => {
                if s.chars().count() > CELL_TEXT_MAX {
                    let head: String = s.chars().take(CELL_TEXT_MAX - 1).collect();
                    format!("{head}…")
                } else {
                    s
                }
            }
            Err(_) => js_string(v),
        },
        other => js_string(other),
    }
}

/// Pretty JSON for the viewers (TS `prettyJson`), capped at [`JSON_CAP`].
pub fn pretty_json(v: &Value) -> String {
    let text = match serde_json::to_string_pretty(v) {
        Ok(s) => s,
        Err(_) => js_string(v),
    };
    if text.chars().count() > JSON_CAP {
        let head: String = text.chars().take(JSON_CAP).collect();
        format!("{head}\n…")
    } else {
        text
    }
}

/// Dot-path getter for report items (TS `getPath`, "timing.after").
pub fn get_path<'a>(obj: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = obj;
    for seg in path.split('.') {
        cur = cur.as_object()?.get(seg)?;
    }
    Some(cur)
}

// ─── field specs (board/table/metrics/chart specs share this shape) ─────────

/// One declared field of a preset spec (TS `FieldSpec`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSpec {
    pub field: String,
    pub label: Option<String>,
    pub unit: Option<String>,
}

impl FieldSpec {
    /// The display label (TS `fieldLabel`: the declared label, else the path).
    pub fn label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.field)
    }
}

/// Parses one `{field, label?, unit?}` entry; `field` must be a non-empty
/// string or the entry is dropped (TS `parseField`).
pub fn parse_field(v: &Value) -> Option<FieldSpec> {
    let obj = v.as_object()?;
    let field = obj.get("field")?.as_str()?.to_owned();
    if field.is_empty() {
        return None;
    }
    Some(FieldSpec {
        field,
        label: obj.get("label").and_then(Value::as_str).map(str::to_owned),
        unit: obj.get("unit").and_then(Value::as_str).map(str::to_owned),
    })
}

/// Parses a fields array; empty after filtering = `None` (TS `parseFields`).
pub fn parse_fields(v: &Value) -> Option<Vec<FieldSpec>> {
    let arr = v.as_array()?;
    let fields: Vec<FieldSpec> = arr.iter().filter_map(parse_field).collect();
    (!fields.is_empty()).then_some(fields)
}

// ─── artifact list rows ──────────────────────────────────────────────────────

/// `Number(v)` for the values specs carry: numbers pass, numeric strings
/// parse (JS coerces), everything else is not a number.
fn js_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) if !s.trim().is_empty() => s.parse::<f64>().ok(),
        _ => None,
    }
}

/// The newest version number an artifact advertises (TS `latestVersion`):
/// the last integer `version` inside `versions[]`, else the top-level one.
pub fn latest_version(a: &WorkflowArtifact) -> u64 {
    for v in a.versions.as_deref().unwrap_or(&[]).iter().rev() {
        if let Some(n) = v.get("version").and_then(js_number) {
            if n >= 0.0 && n.fract() == 0.0 {
                return n as u64;
            }
        }
    }
    a.extra
        .get("version")
        .and_then(js_number)
        .filter(|n| *n >= 0.0 && n.fract() == 0.0)
        .map(|n| n as u64)
        .unwrap_or(0)
}

/// The artifact's declaration spec, passed through by the bridge untouched
/// (TS reads `artifact.spec` — the flattened `extra` map holds it).
pub fn artifact_spec(a: &WorkflowArtifact) -> Option<&Value> {
    a.extra.get("spec")
}

/// Whether the kind draws one of the four spec-driven views (TS `isPreset`).
pub fn artifact_is_preset(kind: &str) -> bool {
    matches!(kind, "board" | "table" | "metrics" | "chart")
}

/// Compact bytes (TS `formatBytes`): `512 B`, `1.5 KB`, `2.0 MB`.
pub fn format_bytes(n: u64) -> String {
    if n < 1024 {
        return format!("{n} B");
    }
    if n < 1024 * 1024 {
        return format!("{:.1} KB", n as f64 / 1024.0);
    }
    format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
}

// ─── workspace rows ──────────────────────────────────────────────────────────

/// The op-tile family (TS `OP_TILE`, first case-sensitive substring match
/// wins — the list order is behavior, not style: `file.write` reads as the
/// READ family because `file` is tested before `write`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpFamily {
    Read,
    Search,
    Git,
    Exec,
    Write,
    Other,
}

const OP_TILE: [(&str, OpFamily); 23] = [
    ("read", OpFamily::Read),
    // `file`, `cat`, `list` also land in the read family.
    ("file", OpFamily::Read),
    ("cat", OpFamily::Read),
    ("list", OpFamily::Read),
    ("search", OpFamily::Search),
    ("grep", OpFamily::Search),
    ("glob", OpFamily::Search),
    ("find", OpFamily::Search),
    ("scan", OpFamily::Search),
    ("git", OpFamily::Git),
    ("shell", OpFamily::Exec),
    ("bash", OpFamily::Exec),
    ("exec", OpFamily::Exec),
    ("spawn", OpFamily::Exec),
    ("terminal", OpFamily::Exec),
    ("run", OpFamily::Exec),
    ("write", OpFamily::Write),
    ("edit", OpFamily::Write),
    ("patch", OpFamily::Write),
    ("apply", OpFamily::Write),
    ("create", OpFamily::Write),
    ("delete", OpFamily::Write),
    ("move", OpFamily::Write),
];

/// The op tile's family: the FIRST matching substring in the TS regex list.
pub fn op_family(op: Option<&str>) -> OpFamily {
    let Some(op) = op else {
        return OpFamily::Other;
    };
    for (needle, family) in OP_TILE {
        if op.contains(needle) {
            return family;
        }
    }
    OpFamily::Other
}

/// Wall time between two epoch-ms stamps (TS `fmtDuration`): compact
/// "12s" / "3m41s"; `None` when absent or inverted.
pub fn fmt_duration(from: Option<u64>, to: Option<u64>) -> Option<String> {
    let (from, to) = (from?, to?);
    if to < from {
        return None;
    }
    let s = ((to - from) as f64 / 1000.0).round() as u64;
    if s < 60 {
        Some(format!("{s}s"))
    } else {
        Some(format!("{}m{}s", s / 60, s % 60))
    }
}

/// The node summary line (TS `nodeSummaryText`): resultCount · exit N ·
/// out N · err N · resultBytes — the set fields, joined with " · ".
pub fn node_summary_text(s: &WorkflowNodeSummary) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(n) = s.result_count {
        parts.push(n.to_string());
    }
    if let Some(code) = s.exit_code {
        parts.push(format!("exit {code}"));
    }
    if let Some(n) = s.stdout_bytes {
        parts.push(format!("out {}", format_bytes(n)));
    }
    if let Some(n) = s.stderr_bytes {
        parts.push(format!("err {}", format_bytes(n)));
    }
    parts.push(format_bytes(s.result_bytes));
    parts.join(" · ")
}

/// One arg value flattened to text (TS `argsText`'s `one`): strings trimmed,
/// scalars stringified, objects/arrays as compact JSON unless empty. `None`
/// carries no information.
fn one_arg(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_owned())
        }
        Value::Number(_) | Value::Bool(_) => Some(js_string(v)),
        Value::Object(_) | Value::Array(_) => {
            let s = serde_json::to_string(v).ok()?;
            match s.as_str() {
                "{}" | "[]" => None,
                _ => Some(s),
            }
        }
        Value::Null => None,
    }
}

fn cap_chars(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        s
    } else {
        s.chars().take(max).collect()
    }
}

/// The node's WHAT — its call arguments flattened to one line (TS
/// `argsText`): array args join with spaces, a single arg renders as-is;
/// capped at 160 chars.
pub fn args_text(n: &WorkflowWorkspaceNode) -> Option<String> {
    let args = n.args.as_ref()?;
    match args {
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().filter_map(one_arg).collect();
            if parts.is_empty() {
                None
            } else {
                Some(cap_chars(parts.join(" "), ARGS_TEXT_MAX))
            }
        }
        single => one_arg(single).map(|s| cap_chars(s, ARGS_TEXT_MAX)),
    }
}

/// The row's stable key (TS: `${siteId ?? "?"}:${ordinal ?? i}`).
pub fn node_key(n: &WorkflowWorkspaceNode, index: usize) -> String {
    format!(
        "{}:{}",
        n.site_id.as_deref().unwrap_or("?"),
        n.ordinal.unwrap_or(index as u64)
    )
}

/// The meta line's identity ref (TS `ref`): `site@ordinal · op`.
pub fn node_ref_line(n: &WorkflowWorkspaceNode) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(site) = &n.site_id {
        match n.ordinal {
            Some(ord) => parts.push(format!("{site}@{ord}")),
            None => parts.push(site.clone()),
        }
    }
    if let Some(op) = &n.op {
        parts.push(op.clone());
    }
    parts.join(" · ")
}

// ─── the phase fold (events tab) ─────────────────────────────────────────────

/// One entered phase, re-entries folded (TS `phases` memo): `{name, times}`
/// in first-seen order, `times` = the max ordinal seen for the name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseEntry {
    pub name: String,
    pub times: u64,
}

fn payload_str(p: &WorkflowRunEventPayload, key: &str) -> Option<String> {
    p.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Phases entered so far (`phase-entered` events), merged per name: a
/// re-entered phase counts up (`×N`), the fold is by max ordinal (resume
/// replays the prefix).
pub fn phases(events: &[WorkflowRunEvent]) -> Vec<PhaseEntry> {
    let mut order: Vec<String> = Vec::new();
    let mut seen: HashMap<String, u64> = HashMap::new();
    for e in events {
        if e.r#type != "phase-entered" {
            continue;
        }
        let Some(name) = payload_str(&e.payload, "name") else {
            continue;
        };
        let ordinal = e
            .payload
            .get("ordinal")
            .and_then(Value::as_f64)
            .filter(|o| o.fract() == 0.0 && *o >= 1.0)
            .map(|o| o as u64)
            .unwrap_or(1);
        if !seen.contains_key(&name) {
            order.push(name.clone());
        }
        seen.entry(name)
            .and_modify(|t| *t = (*t).max(ordinal))
            .or_insert(ordinal);
    }
    order
        .into_iter()
        .map(|name| {
            let times = seen[&name];
            PhaseEntry { name, times }
        })
        .collect()
}

/// One phase's slice of the journal, bounded by phase-entered events (TS
/// `PhaseSlice`); `phase: None` = the leading slice before the first one.
#[derive(Debug, Clone, PartialEq)]
pub struct PhaseSlice {
    pub phase: Option<String>,
    /// The marker's re-entry count (resume replays); ≥2 is worth showing.
    pub ordinal: Option<u64>,
    pub events: Vec<WorkflowRunEvent>,
}

/// The events tab's slicing (TS `phaseSlices` memo): each node renders only
/// in the slice holding its LAST event (`node_home`) and each actor only in
/// the slice of its FIRST creation (`actor_home`) — resume REPLAYS the
/// script prefix, so without those maps a resumed run shows every node twice.
#[derive(Debug, Clone, Default)]
pub struct PhaseSlices {
    pub slices: Vec<PhaseSlice>,
    pub node_home: HashMap<String, usize>,
    pub actor_home: HashMap<String, usize>,
}

pub fn phase_slices(events: &[WorkflowRunEvent]) -> PhaseSlices {
    let mut out = PhaseSlices::default();
    let mut cur = PhaseSlice {
        phase: None,
        ordinal: None,
        events: Vec::new(),
    };
    for e in events {
        if e.r#type == "phase-entered" {
            let p = &e.payload;
            let name = payload_str(p, "phaseName")
                .or_else(|| payload_str(p, "name"))
                .unwrap_or_else(|| "?".to_owned());
            let ordinal = p
                .get("ordinal")
                .and_then(Value::as_f64)
                .filter(|o| o.fract() == 0.0 && *o >= 1.0)
                .map(|o| o as u64);
            push_if_used(&mut out.slices, &mut cur);
            cur = PhaseSlice {
                phase: Some(name),
                ordinal,
                events: Vec::new(),
            };
            continue;
        }
        // `slices.len()` is cur's own index — cur is not pushed yet.
        let idx = out.slices.len();
        if e.r#type.starts_with("node-") {
            out.node_home.insert(instance_key_of(&e.payload), idx);
        } else if e.r#type == "actor-created" {
            let key = actor_key_of(&e.payload);
            out.actor_home.entry(key).or_insert(idx);
        }
        cur.events.push(e.clone());
    }
    push_if_used(&mut out.slices, &mut cur);
    out
}

fn push_if_used(slices: &mut Vec<PhaseSlice>, cur: &mut PhaseSlice) {
    if !cur.events.is_empty() || cur.phase.is_some() {
        slices.push(std::mem::replace(
            cur,
            PhaseSlice {
                phase: None,
                ordinal: None,
                events: Vec::new(),
            },
        ));
    }
}

/// The rows a phase card lists at its own level, newest first (TS
/// `phaseRows`): everything no step owns, plus the non-lifecycle rows
/// (reports, cache closes) of steps homed in ANOTHER slice — a resume
/// replay moves a step to its last slice, and the reports it filed earlier
/// must not vanish with it.
pub fn phase_rows(fold: &SliceFold, is_own: &dyn Fn(&str) -> bool) -> Vec<WorkflowRunEvent> {
    let mut rows = fold.others.clone();
    for n in &fold.nodes {
        if !is_own(&n.key) {
            rows.extend(
                n.events
                    .iter()
                    .filter(|e| !e.r#type.starts_with("node-"))
                    .cloned(),
            );
        }
    }
    rows.sort_by(newest_first);
    rows
}

/// Does a phase card paint at all: it owns a node or an actor, lists journal
/// rows, or carries the settle/usage. The newest PAINTING slice is the run's
/// visible head — an empty just-entered phase must not leave the content
/// beneath it folded shut behind a card that never renders.
pub fn phase_paints(fold: &SliceFold, sliced: &PhaseSlices, slice_index: usize) -> bool {
    if fold.settled.is_some() || fold.spent_tokens.is_some() {
        return true;
    }
    if fold.actors.iter().any(|a| sliced.actor_home.get(&a.key) == Some(&slice_index))
        || fold
            .nodes
            .iter()
            .any(|n| sliced.node_home.get(&n.key) == Some(&slice_index))
    {
        return true;
    }
    let is_own = |key: &str| sliced.node_home.get(key) == Some(&slice_index);
    !phase_rows(fold, &is_own).is_empty()
}

// ─── the artifact text channel ───────────────────────────────────────────────

/// Decodes a chunk's base64 (TS `atob`); padding is optional. `None` marks
/// an undecodable payload — the TS catch branch flips the viewer to binary.
pub fn decode_base64(data: &str) -> Option<Vec<u8>> {
    // atob tolerates missing padding; both spellings must decode.
    static ENGINE: GeneralPurpose = GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
    );
    ENGINE.decode(data).ok()
}

/// The binary verdict (TS: >1% U+FFFD across the WHOLE accumulated text once
/// the chunks stop coming — a split codepoint mid-stream must not read as
/// binary, which the whole-buffer lossy decode guarantees).
pub fn binary_verdict(text: &str) -> bool {
    let total = text.chars().count();
    if total == 0 {
        return false;
    }
    let bad = text.chars().filter(|&c| c == '\u{fffd}').count();
    (bad as f64 / total as f64) > BINARY_BAD_RATIO
}

/// One row of a preset artifact's paginated inventory (TS maps to
/// `{sequence: it.sequence ?? 0, item}` — the sequence drives the cursor
/// and the append dedupe).
#[derive(Debug, Clone, PartialEq)]
pub struct ArtifactDataItem {
    pub sequence: u64,
    pub item: Value,
}

pub fn map_artifact_items(items: Vec<WorkflowArtifactItem>) -> Vec<ArtifactDataItem> {
    items
        .into_iter()
        .map(|it| ArtifactDataItem {
            sequence: it.sequence.unwrap_or(0),
            item: it.item,
        })
        .collect()
}

/// The append-merge dedupe (TS `loadData`): an unadvancing cursor answers
/// with the same page — keep only items past the highest sequence held.
pub fn merge_artifact_items(
    held: Vec<ArtifactDataItem>,
    incoming: Vec<ArtifactDataItem>,
) -> Vec<ArtifactDataItem> {
    let floor = held.iter().map(|it| it.sequence).max().unwrap_or(0);
    let mut merged = held;
    merged.extend(incoming.into_iter().filter(|it| it.sequence > floor));
    merged
}

// ─── the shared fallback (KeyValueCards) ─────────────────────────────────────

/// One fallback card: a lone text line, or a record's key-value rows (TS
/// `KeyValueCards`); the u64 is the row's stable identity (its sequence).
#[derive(Debug, Clone, PartialEq)]
pub enum KeyValueCard {
    Text(String),
    Record(Vec<(String, String)>),
}

pub fn key_value_cards(items: &[ArtifactDataItem]) -> Vec<(u64, KeyValueCard)> {
    items
        .iter()
        .map(|it| {
            let card = match &it.item {
                Value::Object(rec) => KeyValueCard::Record(
                    rec.iter()
                        .map(|(k, v)| (k.clone(), cell_text(v)))
                        .collect::<Vec<_>>(),
                ),
                Value::String(s) => KeyValueCard::Text(s.clone()),
                other => KeyValueCard::Text(cell_text(other)),
            };
            (it.sequence, card)
        })
        .collect()
}

// ─── table artifacts ─────────────────────────────────────────────────────────

/// The table's render model: declared (or first-row) columns plus one ready
/// cell per column per row.
#[derive(Debug, Clone, PartialEq)]
pub struct TableModel {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// Computes the table view (TS `TableView`): columns come from the spec's
/// declared fields, else the first row's own keys (first 8). A spec `key`
/// field means a later item with the same key REPLACES the row (insertion
/// order preserved). Non-record items render as their own single-cell rows.
/// `None` = TS's everything-null branch (the caller falls back to
/// KeyValueCards over the non-record items).
pub fn table_model(items: &[ArtifactDataItem], spec: Option<&Value>) -> Option<TableModel> {
    let spec = spec.filter(|s| is_record(s));
    let declared = spec.and_then(|s| parse_fields(s.get("columns").unwrap_or(&Value::Null)));
    let key_field = spec
        .and_then(|s| s.get("key"))
        .and_then(Value::as_str)
        .filter(|k| !k.is_empty());

    // Newest-wins dedupe on the declared key, keeping first-seen order
    // (JS Map::set semantics). Keyed mode keeps only records — TS drops the
    // non-record items outright (WorkflowArtifactViews.tsx:163).
    let mut rows: Vec<(usize, Option<Map<String, Value>>)> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for (i, it) in items.iter().enumerate() {
        let record = it.item.as_object().cloned();
        match (&record, key_field) {
            (Some(rec), Some(key_field)) => {
                let key = get_path(&Value::Object(rec.clone()), key_field)
                    .filter(|v| !v.is_null())
                    .map(js_string)
                    .unwrap_or_else(|| format!("#{i}"));
                match index.get(&key) {
                    Some(&slot) => rows[slot] = (i, Some(rec.clone())),
                    None => {
                        index.insert(key, rows.len());
                        rows.push((i, Some(rec.clone())));
                    }
                }
            }
            // Keyed: a non-record item is not a row.
            (_, Some(_)) => continue,
            _ => rows.push((i, record)),
        }
    }

    let first_record = rows.iter().find_map(|(_, rec)| rec.clone());
    let columns: Vec<String> = match &declared {
        Some(fields) => fields.iter().map(|f| f.label().to_owned()).collect(),
        None => first_record?.keys().take(8).cloned().collect::<Vec<_>>(),
    };
    if rows.iter().all(|(_, rec)| rec.is_none()) {
        return None;
    }

    let cells: Vec<Vec<String>> = rows
        .into_iter()
        .map(|(i, rec)| {
            columns
                .iter()
                .map(|col| {
                    // TS resolves the cell through the declared label back to
                    // its field, else the column name, then reads the record
                    // DIRECTLY (not by dot path).
                    let field = declared
                        .as_ref()
                        .and_then(|fields| {
                            fields
                                .iter()
                                .find(|f| f.label() == col)
                                .map(|f| f.field.clone())
                        })
                        .unwrap_or_else(|| col.clone());
                    match rec.as_ref().and_then(|rec| rec.get(&field)) {
                        Some(v) => cell_text(v),
                        // A non-record row's cell carries the whole item.
                        None if rec.is_none() => cell_text(&items[i].item),
                        None => String::new(),
                    }
                })
                .collect()
        })
        .collect();
    Some(TableModel {
        columns,
        rows: cells,
    })
}

// ─── metrics artifacts ───────────────────────────────────────────────────────

/// One metrics tile: label + latest value (empty fields render "—") + unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricTile {
    pub label: String,
    pub value: String,
    pub unit: Option<String>,
}

/// Computes the metrics tiles (TS `MetricsTiles`): one tile per declared
/// metric, the value from the LATEST item that carries the field
/// (BoardSpec semantics). `None` = no usable spec (the caller falls back to
/// KeyValueCards).
pub fn metrics_tiles(items: &[ArtifactDataItem], spec: Option<&Value>) -> Option<Vec<MetricTile>> {
    let fields = spec
        .filter(|s| is_record(s))
        .and_then(|s| parse_fields(s.get("metrics").unwrap_or(&Value::Null)))?;
    let mut latest: HashMap<String, Value> = HashMap::new();
    for it in items {
        let Some(rec) = it.item.as_object() else {
            continue;
        };
        let obj = Value::Object(rec.clone());
        for f in &fields {
            if let Some(v) = get_path(&obj, &f.field) {
                if !v.is_null() {
                    latest.insert(f.field.clone(), v.clone());
                }
            }
        }
    }
    Some(
        fields
            .iter()
            .map(|f| MetricTile {
                label: f.label().to_owned(),
                value: latest
                    .get(&f.field)
                    .map(|v| cell_text(v))
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "—".to_owned()),
                unit: f.unit.clone(),
            })
            .collect(),
    )
}

// ─── board artifacts ─────────────────────────────────────────────────────────

/// One kanban card: title plus the spec-declared details that carried a
/// value (the card's identity rides `key` — a card moves columns by update).
#[derive(Debug, Clone, PartialEq)]
pub struct BoardCard {
    pub key: String,
    pub title: String,
    pub details: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoardColumn {
    pub name: String,
    pub cards: Vec<BoardCard>,
}

/// Computes the board's columns (TS `BoardView`): every record item is a
/// card keyed by the spec `key` field (a later item with the same key
/// UPDATES the card in place) placed in the column named by the `status`
/// field. Columns render in spec order, unknown statuses trail. `None` =
/// no usable spec (fallback).
pub fn board_columns(items: &[ArtifactDataItem], spec: Option<&Value>) -> Option<Vec<BoardColumn>> {
    let spec = spec.filter(|s| is_record(s))?;
    let key_field = spec.get("key").and_then(Value::as_str)?;
    let status_field = spec.get("status").and_then(Value::as_str)?;
    let declared_columns: Vec<String> = spec
        .get("columns")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let card_title = spec.get("cardTitle").and_then(Value::as_str);
    let details = parse_fields(spec.get("detail").unwrap_or(&Value::Null)).unwrap_or_default();

    // Insertion-ordered, replace-in-place (JS Map semantics): a card that
    // moves columns keeps its slot and its column follows the update.
    let mut order: Vec<String> = Vec::new();
    let mut cards: HashMap<String, BoardCard> = HashMap::new();
    let mut columns: HashMap<String, String> = HashMap::new();
    let mut extra_columns: Vec<String> = Vec::new();
    for it in items {
        let Some(rec) = it.item.as_object() else {
            continue;
        };
        let obj = Value::Object(rec.clone());
        let Some(key_val) = get_path(&obj, key_field) else {
            continue;
        };
        if key_val.is_null() {
            continue;
        }
        let key = js_string(key_val);
        if key.is_empty() {
            continue;
        }
        let column = match get_path(&obj, status_field) {
            None | Some(Value::Null) => "—".to_owned(),
            Some(v) => js_string(v),
        };
        if !declared_columns.contains(&column) && !extra_columns.contains(&column) {
            extra_columns.push(column.clone());
        }
        let title = match card_title {
            Some(path) => match get_path(&obj, path) {
                Some(v) if !v.is_null() && !js_string(v).is_empty() => js_string(v),
                _ => key.clone(),
            },
            None => key.clone(),
        };
        let detail_rows: Vec<(String, String)> = details
            .iter()
            .filter_map(|f| {
                let s = cell_text(get_path(&obj, &f.field)?);
                (!s.is_empty()).then(|| (f.label().to_owned(), s))
            })
            .collect();
        if !order.contains(&key) {
            order.push(key.clone());
        }
        cards.insert(
            key.clone(),
            BoardCard {
                key: key.clone(),
                title,
                details: detail_rows,
            },
        );
        columns.insert(key, column);
    }

    let mut all = declared_columns;
    all.extend(extra_columns);
    Some(
        all.into_iter()
            .map(|name| BoardColumn {
                cards: order
                    .iter()
                    .filter(|key| columns.get(*key).map(|c| c == &name).unwrap_or(false))
                    .filter_map(|key| cards.get(key).cloned())
                    .collect(),
                name,
            })
            .collect(),
    )
}

// ─── chart artifacts ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartKind {
    Line,
    Bar,
    Scatter,
}

/// One series: label + unit + points sorted by x, plus the latest y (the
/// legend's value column).
#[derive(Debug, Clone, PartialEq)]
pub struct ChartSeries {
    pub label: String,
    pub unit: Option<String>,
    pub points: Vec<(f64, f64)>,
    pub last: Option<f64>,
}

/// The chart's render model in normalized coordinates: the UI maps the
/// fractions onto its canvas, so all scaling decisions live here with the
/// data (TS ChartView's pixel math, minus the pixels).
#[derive(Debug, Clone, PartialEq)]
pub struct ChartModel {
    pub kind: ChartKind,
    pub series: Vec<ChartSeries>,
    /// String x values became category slots by first-seen order.
    pub categories: Vec<String>,
    pub baseline: Option<f64>,
    pub use_log: bool,
    pub y_min: f64,
    pub y_max: f64,
    pub x_min: f64,
    pub x_max: f64,
}

impl ChartModel {
    /// The three gridline values (TS `gridValues`): min, mid, max.
    pub fn grid_values(&self) -> [f64; 3] {
        [self.y_min, (self.y_min + self.y_max) / 2.0, self.y_max]
    }

    fn tf(&self, v: f64) -> f64 {
        if self.use_log {
            v.log10()
        } else {
            v
        }
    }

    /// A y value's height fraction, 0 = axis floor … 1 = axis top.
    pub fn y_frac(&self, v: f64) -> f64 {
        let span = self.tf(self.y_max) - self.tf(self.y_min);
        if span == 0.0 {
            return 0.5;
        }
        ((self.tf(v) - self.tf(self.y_min)) / span).clamp(0.0, 1.0)
    }

    /// An x value's width fraction, 0 = left … 1 = right.
    pub fn x_frac(&self, v: f64) -> f64 {
        let span = self.x_max - self.x_min;
        if span == 0.0 {
            return 0.5;
        }
        ((v - self.x_min) / span).clamp(0.0, 1.0)
    }
}

fn to_num(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    match v {
        Value::Number(n) => n.as_f64().filter(|n| n.is_finite()),
        Value::String(s) if !s.trim().is_empty() => s.parse::<f64>().ok().filter(|n| n.is_finite()),
        _ => None,
    }
}

/// The x coordinate of one item (TS `toX`): a number charts as-is, any other
/// non-empty value becomes a category slot by first-seen order.
fn chart_x(obj: &Value, field: &str, categories: &mut Vec<String>) -> Option<f64> {
    let v = get_path(obj, field);
    if let Some(n) = to_num(v) {
        return Some(n);
    }
    let s = match v {
        None | Some(Value::Null) => return None,
        Some(Value::String(s)) if s.is_empty() => return None,
        Some(v) => js_string(v),
    };
    match categories.iter().position(|c| c == &s) {
        Some(idx) => Some(idx as f64),
        None => {
            categories.push(s);
            Some((categories.len() - 1) as f64)
        }
    }
}

/// Computes the chart model (TS `ChartView`): each tagged report item is one
/// point, x/y read through the spec's dot paths; string x values become
/// category slots by first-seen order; points order by x. `None` = no
/// usable spec (fallback).
pub fn chart_model(items: &[ArtifactDataItem], spec: Option<&Value>) -> Option<ChartModel> {
    let spec = spec.filter(|s| is_record(s))?;
    let x = parse_field(spec.get("x").unwrap_or(&Value::Null))?;
    let y_field = parse_field(spec.get("y").unwrap_or(&Value::Null));
    let y_fields = parse_fields(spec.get("y").unwrap_or(&Value::Null));
    let y = y_fields.or_else(|| y_field.map(|f| vec![f]));
    let y = y?;
    let kind = match spec.get("type").and_then(Value::as_str) {
        Some("bar") => ChartKind::Bar,
        Some("scatter") => ChartKind::Scatter,
        _ => ChartKind::Line,
    };
    let use_log = spec.get("scale").and_then(Value::as_str) == Some("log");
    let baseline_field = parse_field(spec.get("baseline").unwrap_or(&Value::Null));

    let mut categories: Vec<String> = Vec::new();

    let mut series: Vec<ChartSeries> = Vec::new();
    for f in &y {
        let mut points: Vec<(f64, f64)> = Vec::new();
        for it in items {
            let Some(rec) = it.item.as_object() else {
                continue;
            };
            let obj = Value::Object(rec.clone());
            let Some(y) = to_num(get_path(&obj, &f.field)) else {
                continue;
            };
            let Some(x) = chart_x(&obj, &x.field, &mut categories) else {
                continue;
            };
            points.push((x, y));
        }
        points.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        series.push(ChartSeries {
            label: f.label().to_owned(),
            unit: f.unit.clone(),
            last: points.last().map(|p| p.1),
            points,
        });
    }

    let mut baseline: Option<f64> = None;
    if let Some(bf) = &baseline_field {
        for it in items {
            let Some(rec) = it.item.as_object() else {
                continue;
            };
            let obj = Value::Object(rec.clone());
            match get_path(&obj, &bf.field) {
                Some(Value::Number(n)) if n.as_f64().is_some_and(f64::is_finite) => {
                    baseline = n.as_f64();
                    break;
                }
                _ => {}
            }
        }
    }

    let mut all: Vec<f64> = series
        .iter()
        .flat_map(|s| s.points.iter().map(|p| p.1))
        .collect();
    if let Some(b) = baseline {
        all.push(b);
    }
    let mut y_min = all.iter().copied().fold(f64::INFINITY, f64::min);
    let mut y_max = all.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if !all.is_empty() && y_min == y_max {
        y_min -= 1.0;
        y_max += 1.0;
    } else {
        let pad = (y_max - y_min) * 0.08;
        y_min -= pad;
        y_max += pad;
    }
    if all.is_empty() {
        y_min = 0.0;
        y_max = 1.0;
    }
    let use_log = use_log && y_min > 0.0;
    let xs: Vec<f64> = series
        .iter()
        .flat_map(|s| s.points.iter().map(|p| p.0))
        .collect();
    let x_min = xs.iter().copied().fold(0.0, f64::min);
    let x_max = xs
        .iter()
        .copied()
        .fold(0.0, f64::max)
        .max(if categories.is_empty() {
            0.0
        } else {
            (categories.len() - 1) as f64
        });

    Some(ChartModel {
        kind,
        series,
        categories,
        baseline,
        use_log,
        y_min,
        y_max,
        x_min,
        x_max,
    })
}

/// The chart's axis/legend number format (TS `fmt`): thousands grouping over
/// 1000, else two-decimal rounding.
pub fn chart_fmt(v: f64) -> String {
    if v.abs() >= 1000.0 {
        thousands(v)
    } else {
        fmt_num((v * 100.0).round() / 100.0)
    }
}

// ─── the node result shape ───────────────────────────────────────────────────

/// A workspace node's expanded body, structured before raw (TS
/// `NodeResultView`): strings render as text, world.run-shaped results
/// split into exit code + output streams, plain objects become key-value
/// rows, anything else stays scalar text.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeResultBody {
    Text(String),
    Run {
        exit_code: Option<f64>,
        stdout: Option<String>,
        stderr: Option<String>,
    },
    Record(Vec<(String, String)>),
    Scalar(String),
}

pub fn node_result_body(body: &Value) -> NodeResultBody {
    match body {
        Value::String(s) => NodeResultBody::Text(s.clone()),
        Value::Object(rec) => {
            let run_shaped = ["stdout", "stderr", "exitCode"]
                .iter()
                .any(|k| rec.contains_key(*k));
            if run_shaped {
                NodeResultBody::Run {
                    exit_code: rec.get("exitCode").and_then(Value::as_f64),
                    stdout: rec.get("stdout").and_then(Value::as_str).map(str::to_owned),
                    stderr: rec.get("stderr").and_then(Value::as_str).map(str::to_owned),
                }
            } else {
                NodeResultBody::Record(rec.iter().map(|(k, v)| (k.clone(), cell_text(v))).collect())
            }
        }
        other => NodeResultBody::Scalar(cell_text(other)),
    }
}

// ─── the unknown-kind metadata card ──────────────────────────────────────────

/// The metadata card of an unknown-kind artifact (TS `ArtifactMetaCard`):
/// the known identity fields plus every unrecognized top-level key — the
/// shape is open, so new kinds still read.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ArtifactMeta {
    pub kind: String,
    pub description: Option<String>,
    pub id: String,
    pub content_type: Option<String>,
    pub source_path: Option<String>,
    pub item_count: Option<u64>,
    /// The top-level keys the card does not know (rendered as extra rows).
    pub extras: Vec<(String, String)>,
}

const ARTIFACT_KNOWN_KEYS: [&str; 11] = [
    "id",
    "kind",
    "title",
    "description",
    "contentType",
    "sourcePath",
    "primary",
    "itemCount",
    "version",
    "versions",
    "spec",
];

pub fn artifact_meta(a: &WorkflowArtifact) -> ArtifactMeta {
    ArtifactMeta {
        kind: a.kind.clone(),
        description: a
            .extra
            .get("description")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        id: a.id.clone(),
        content_type: a
            .extra
            .get("contentType")
            .filter(|v| !v.is_null())
            .map(js_string),
        source_path: a
            .extra
            .get("sourcePath")
            .and_then(Value::as_str)
            .map(str::to_owned),
        item_count: a.item_count,
        extras: a
            .extra
            .iter()
            .filter(|(k, _)| !ARTIFACT_KNOWN_KEYS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), cell_text(v)))
            .collect(),
    }
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow_run_event_text::reduce_slice;
    use serde_json::json;

    fn event(sequence: u64, kind: &str, payload: Value) -> WorkflowRunEvent {
        WorkflowRunEvent {
            sequence,
            r#type: kind.to_owned(),
            payload: payload.as_object().cloned().unwrap_or_default(),
            truncated: None,
        }
    }

    fn artifact(id: &str, kind: &str, extra: Value) -> WorkflowArtifact {
        let mut full = json!({ "id": id, "kind": kind });
        if let (Some(full_obj), Some(extra_obj)) = (full.as_object_mut(), extra.as_object()) {
            for (k, v) in extra_obj {
                full_obj.insert(k.clone(), v.clone());
            }
        }
        serde_json::from_value(full).expect("artifact parses")
    }

    fn items(v: Value) -> Vec<ArtifactDataItem> {
        map_artifact_items(
            serde_json::from_value::<Vec<crate::workflow::WorkflowArtifactItem>>(v)
                .expect("items parse"),
        )
    }

    // ── phases + slices (L2)

    #[test]
    fn phases_fold_reentry_by_max_ordinal() {
        let events = vec![
            event(1, "run-started", json!({})),
            event(2, "phase-entered", json!({ "name": "map", "ordinal": 1 })),
            event(
                3,
                "phase-entered",
                json!({ "name": "reduce", "ordinal": 1 }),
            ),
            event(4, "phase-entered", json!({ "name": "map", "ordinal": 2 })),
        ];
        let folded = phases(&events);
        assert_eq!(
            folded,
            vec![
                PhaseEntry {
                    name: "map".into(),
                    times: 2
                },
                PhaseEntry {
                    name: "reduce".into(),
                    times: 1
                },
            ]
        );
    }

    #[test]
    fn slices_bound_at_phase_boundaries() {
        let events = vec![
            event(1, "run-started", json!({})),
            event(2, "phase-entered", json!({ "name": "a" })),
            event(
                3,
                "node-queued",
                json!({ "instance": { "siteId": "s", "ordinal": 1 } }),
            ),
            event(4, "phase-entered", json!({ "phaseName": "b" })),
            event(
                5,
                "node-settled",
                json!({ "instance": { "siteId": "s", "ordinal": 1 }, "outcome": "ok" }),
            ),
        ];
        let slices = phase_slices(&events);
        assert_eq!(slices.slices.len(), 3);
        assert_eq!(slices.slices[0].phase, None);
        assert_eq!(slices.slices[0].events.len(), 1);
        assert_eq!(slices.slices[1].phase.as_deref(), Some("a"));
        assert_eq!(slices.slices[2].phase.as_deref(), Some("b"));
        // The node's LAST event decides its slice (resume replay guard).
        assert_eq!(slices.node_home.get("s@1"), Some(&2));
    }

    #[test]
    fn actor_home_keeps_first_creation() {
        let events = vec![
            event(1, "actor-created", json!({ "name": "impl" })),
            event(2, "phase-entered", json!({ "name": "a" })),
            event(3, "actor-created", json!({ "name": "impl" })),
        ];
        let slices = phase_slices(&events);
        assert_eq!(slices.actor_home.get("name:impl"), Some(&0));
    }

    // ── workspace rows

    #[test]
    fn args_text_flattens_and_caps() {
        let node = serde_json::from_value::<WorkflowWorkspaceNode>(json!({
            "siteId": "port", "ordinal": 2, "op": "world.run",
            "args": ["cargo", "test", "-p", "protocol"]
        }))
        .unwrap();
        assert_eq!(args_text(&node).as_deref(), Some("cargo test -p protocol"));
        let empty = serde_json::from_value::<WorkflowWorkspaceNode>(json!({
            "op": "noop", "args": {}
        }))
        .unwrap();
        assert_eq!(args_text(&empty), None);
    }

    #[test]
    fn op_tile_first_match_wins() {
        // `file` is tested before `write`: the TS regex list order is behavior.
        assert_eq!(op_family(Some("file.write")), OpFamily::Read);
        // The regexes are case-sensitive: `writeFile` carries no `file`.
        assert_eq!(op_family(Some("world.writeFile")), OpFamily::Write);
        assert_eq!(op_family(Some("world.read")), OpFamily::Read);
        assert_eq!(op_family(Some("world.run")), OpFamily::Exec);
        assert_eq!(op_family(Some("fs.gitStatus")), OpFamily::Git);
        assert_eq!(op_family(Some("search.code")), OpFamily::Search);
        assert_eq!(op_family(Some("edit.replace")), OpFamily::Write);
        assert_eq!(op_family(None), OpFamily::Other);
    }

    #[test]
    fn node_summary_line_joined_with_dots() {
        let s: WorkflowNodeSummary = serde_json::from_value(json!({
            "resultBytes": 1844, "resultCount": 38, "exitCode": 0,
            "stdoutBytes": 1502, "stderrBytes": 342
        }))
        .unwrap();
        assert_eq!(
            node_summary_text(&s),
            "38 · exit 0 · out 1.5 KB · err 342 B · 1.8 KB"
        );
    }

    #[test]
    fn duration_compact_words() {
        assert_eq!(fmt_duration(Some(1000), Some(13_000)), Some("12s".into()));
        assert_eq!(fmt_duration(Some(0), Some(221_000)), Some("3m41s".into()));
        assert_eq!(fmt_duration(Some(10), Some(1)), None);
        assert_eq!(fmt_duration(None, Some(1)), None);
    }

    // ── the text channel

    #[test]
    fn base64_decodes_with_or_without_padding() {
        assert_eq!(decode_base64("aGk=").unwrap(), b"hi".to_vec());
        assert_eq!(decode_base64("aGk").unwrap(), b"hi".to_vec());
        assert!(decode_base64("!!!").is_none());
    }

    #[test]
    fn binary_verdict_needs_one_percent() {
        // 1 of 100 replacement chars is exactly 1% — NOT over the bar.
        let text = "a".repeat(99) + "\u{fffd}";
        assert!(!binary_verdict(&text));
        let text = "a".repeat(98) + "\u{fffd}\u{fffd}";
        assert!(binary_verdict(&text));
        assert!(!binary_verdict(""));
    }

    #[test]
    fn data_items_merge_keeps_only_new_sequences() {
        let held = vec![
            ArtifactDataItem {
                sequence: 1,
                item: json!(1),
            },
            ArtifactDataItem {
                sequence: 3,
                item: json!(3),
            },
        ];
        // A degenerate cursor answers with the FULL page again.
        let incoming = vec![
            ArtifactDataItem {
                sequence: 1,
                item: json!(1),
            },
            ArtifactDataItem {
                sequence: 4,
                item: json!(4),
            },
        ];
        let merged = merge_artifact_items(held, incoming);
        assert_eq!(
            merged.iter().map(|it| it.sequence).collect::<Vec<_>>(),
            vec![1, 3, 4]
        );
    }

    // ── spec views

    #[test]
    fn table_declared_columns_and_key_replace() {
        let spec = json!({
            "key": "title",
            "columns": [
                { "field": "title", "label": "File" },
                { "field": "state" }
            ]
        });
        let items = items(json!([
            { "sequence": 1, "item": { "title": "hub.rs", "state": "done" } },
            { "sequence": 2, "item": { "title": "workflow.rs", "state": "todo" } },
            // Same key: REPLACES the row, keeping its slot.
            { "sequence": 3, "item": { "title": "hub.rs", "state": "ported" } }
        ]));
        let model = table_model(&items, Some(&spec)).expect("spec is usable");
        assert_eq!(model.columns, vec!["File", "state"]);
        assert_eq!(model.rows.len(), 2);
        assert_eq!(model.rows[0], vec!["hub.rs", "ported"]);
        assert_eq!(model.rows[1], vec!["workflow.rs", "todo"]);
    }

    #[test]
    fn table_without_spec_uses_first_row_keys() {
        let items = items(json!([
            { "sequence": 1, "item": { "a": 1, "b": 2, "c": 3, "d": 4, "e": 5, "f": 6, "g": 7, "h": 8, "i": 9 } }
        ]));
        let model = table_model(&items, None).expect("first row names columns");
        // First 8 keys only.
        assert_eq!(model.columns.len(), 8);
        assert!(!model.columns.contains(&"i".to_owned()));
        assert_eq!(model.rows[0][0], "1");
    }

    #[test]
    fn table_all_non_records_falls_back() {
        let items = items(json!([
            { "sequence": 1, "item": "just text" },
            { "sequence": 2, "item": 42 }
        ]));
        assert!(table_model(&items, None).is_none());
    }

    #[test]
    fn table_keyed_drops_non_records() {
        // TS keeps only keyed records in keyed mode; the strays do not get
        // their own single-cell rows (WorkflowArtifactViews.tsx:163).
        let spec = json!({ "key": "title" });
        let mixed = items(json!([
            { "sequence": 1, "item": { "title": "hub.rs", "state": "done" } },
            { "sequence": 2, "item": "stray text" },
            { "sequence": 3, "item": 42 }
        ]));
        let model = table_model(&mixed, Some(&spec)).expect("spec is usable");
        assert_eq!(model.rows.len(), 1);
        assert_eq!(model.rows[0][0], "hub.rs");
        // All non-records under a key: nothing rows — the KV fallback.
        let stray = items(json!([
            { "sequence": 1, "item": "stray" },
            { "sequence": 2, "item": 7 }
        ]));
        assert!(table_model(&stray, Some(&spec)).is_none());
    }

    #[test]
    fn metrics_latest_value_per_field() {
        let spec = json!({ "metrics": [
            { "field": "tests", "label": "Tests" },
            { "field": "timing.after", "unit": "ms" }
        ]});
        let items = items(json!([
            { "sequence": 1, "item": { "tests": 10, "timing": { "after": 100 } } },
            { "sequence": 2, "item": { "tests": 38 } }
        ]));
        let tiles = metrics_tiles(&items, Some(&spec)).expect("tiles");
        assert_eq!(tiles[0].label, "Tests");
        assert_eq!(tiles[0].value, "38");
        assert_eq!(tiles[1].value, "100");
        assert_eq!(tiles[1].unit.as_deref(), Some("ms"));
        // A spec without usable fields falls back.
        assert!(metrics_tiles(&items, Some(&json!({ "metrics": [] }))).is_none());
    }

    #[test]
    fn board_cards_move_columns_on_update() {
        let spec = json!({
            "key": "title", "status": "state",
            "columns": ["todo", "done"],
            "cardTitle": "title",
            "detail": [{ "field": "notes", "label": "Notes" }]
        });
        let items = items(json!([
            { "sequence": 1, "item": { "title": "hub.rs", "state": "todo", "notes": "n1" } },
            { "sequence": 2, "item": { "title": "workflow.rs", "state": "todo" } },
            { "sequence": 3, "item": { "title": "hub.rs", "state": "done" } },
            // Unknown status trails in an extra column.
            { "sequence": 4, "item": { "title": "misc", "state": "blocked" } }
        ]));
        let cols = board_columns(&items, Some(&spec)).expect("board");
        assert_eq!(cols.len(), 3);
        assert_eq!(cols[0].name, "todo");
        assert_eq!(cols[0].cards.len(), 1);
        assert_eq!(cols[0].cards[0].title, "workflow.rs");
        // hub.rs moved to done by its update; its detail row vanished with it.
        assert_eq!(cols[1].name, "done");
        assert_eq!(cols[1].cards[0].title, "hub.rs");
        assert!(cols[1].cards[0].details.is_empty());
        assert_eq!(cols[2].name, "blocked");
    }

    #[test]
    fn chart_points_categories_and_scaling() {
        let spec = json!({
            "x": { "field": "day" },
            "y": [{ "field": "ms", "label": "Latency" }],
            "baseline": { "field": "budget" }
        });
        let items = items(json!([
            { "sequence": 1, "item": { "day": "mon", "ms": 100, "budget": 200 } },
            { "sequence": 2, "item": { "day": "tue", "ms": 300 } }
        ]));
        let model = chart_model(&items, Some(&spec)).expect("chart");
        assert_eq!(model.kind, ChartKind::Line);
        assert_eq!(model.categories, vec!["mon", "tue"]);
        assert_eq!(model.series[0].points, vec![(0.0, 100.0), (1.0, 300.0)]);
        assert_eq!(model.series[0].last, Some(300.0));
        assert_eq!(model.baseline, Some(200.0));
        assert_eq!(model.x_min, 0.0);
        assert_eq!(model.x_max, 1.0);
        // y covers 100..300 padded by 8%, so 300 sits below the top.
        assert!(model.y_frac(100.0) < model.y_frac(300.0));
        assert!(model.y_frac(300.0) < 1.0);
        assert!(model.y_frac(200.0) > 0.4 && model.y_frac(200.0) < 0.6);
        // The mid gridline bisects.
        let grid = model.grid_values();
        assert!((grid[1] - (model.y_min + model.y_max) / 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn chart_numeric_strings_chart_and_log_needs_positive_floor() {
        let spec = json!({
            "x": { "field": "i" },
            "y": { "field": "v" },
            "scale": "log"
        });
        let items = items(json!([
            { "sequence": 1, "item": { "i": "1", "v": "10" } },
            { "sequence": 2, "item": { "i": 2, "v": 1000 } },
            // A zero floor must switch log off rather than log(0).
            { "sequence": 3, "item": { "i": 3, "v": 0 } }
        ]));
        let model = chart_model(&items, Some(&spec)).expect("chart");
        assert!(
            !model.use_log,
            "y floor reached 0 — log scale must back off"
        );
        assert_eq!(model.series[0].points.len(), 3);
    }

    #[test]
    fn chart_bad_spec_falls_back() {
        assert!(chart_model(&[], Some(&json!({ "y": { "field": "v" } }))).is_none());
        assert!(chart_model(&[], None).is_none());
    }

    // ── node result shape

    #[test]
    fn node_result_splits_run_shape() {
        let run = node_result_body(&json!({
            "exitCode": 1, "stdout": "ok", "stderr": "boom", "extra": 7
        }));
        match run {
            NodeResultBody::Run {
                exit_code,
                stdout,
                stderr,
            } => {
                assert_eq!(exit_code, Some(1.0));
                assert_eq!(stdout.as_deref(), Some("ok"));
                assert_eq!(stderr.as_deref(), Some("boom"));
            }
            other => panic!("run shape expected, got {other:?}"),
        }
        assert!(matches!(
            node_result_body(&json!({ "a": 1 })),
            NodeResultBody::Record(_)
        ));
        assert!(matches!(
            node_result_body(&json!("plain")),
            NodeResultBody::Text(_)
        ));
        assert!(matches!(node_result_body(&json!(5)), NodeResultBody::Scalar(s) if s == "5"));
    }

    // ── misc

    #[test]
    fn latest_version_reads_versions_then_top_level() {
        let a = artifact(
            "a",
            "markdown",
            json!({ "versions": [ { "version": 1 }, { "version": 2 } ] }),
        );
        assert_eq!(latest_version(&a), 2);
        let b = artifact("b", "file", json!({ "version": 7 }));
        assert_eq!(latest_version(&b), 7);
        let c = artifact("c", "file", json!({}));
        assert_eq!(latest_version(&c), 0);
    }

    #[test]
    fn cell_text_caps_objects_and_empties_null() {
        assert_eq!(cell_text(&Value::Null), "");
        assert_eq!(cell_text(&json!(true)), "true");
        assert_eq!(cell_text(&json!(1.0)), "1");
        let long = Value::String("x".repeat(200));
        let wrapped = json!({ "k": long });
        let out = cell_text(&wrapped);
        assert!(out.chars().count() == 80 && out.ends_with('…'));
    }

    #[test]
    fn pretty_json_caps_at_json_cap() {
        let big = json!(Value::String("y".repeat(JSON_CAP + 50)));
        let out = pretty_json(&big);
        assert!(out.chars().count() <= JSON_CAP + 2);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn artifact_meta_collects_known_and_extras() {
        let a = artifact(
            "art-9",
            "weird",
            json!({
                "description": "Some new kind",
                "contentType": "application/x-weird",
                "sourcePath": "/tmp/x",
                "itemCount": 3,
                "frobnicate": true
            }),
        );
        let meta = artifact_meta(&a);
        assert_eq!(meta.kind, "weird");
        assert_eq!(meta.description.as_deref(), Some("Some new kind"));
        assert_eq!(meta.content_type.as_deref(), Some("application/x-weird"));
        assert_eq!(meta.source_path.as_deref(), Some("/tmp/x"));
        assert_eq!(meta.item_count, Some(3));
        assert_eq!(
            meta.extras,
            vec![("frobnicate".to_owned(), "true".to_owned())]
        );
    }

    // ── the newest-first phase cards (mobile parity)

    /// reduce_slice needs a Translate; the identity catalog is enough — the
    /// folds under test read no localized words.
    struct TextCatalog;

    impl crate::workflow_run_event_text::Translate for TextCatalog {
        fn t(&self, key: &str, _values: &[(&'static str, String)]) -> String {
            key.to_owned()
        }
    }

    #[test]
    fn slices_capture_the_reentry_ordinal() {
        let sliced = phase_slices(&[
            event(1, "phase-entered", json!({ "name": "map", "ordinal": 1 })),
            event(2, "log", json!({ "message": "x" })),
            event(3, "phase-entered", json!({ "name": "map", "ordinal": 2 })),
        ]);
        assert_eq!(sliced.slices[0].ordinal, Some(1));
        assert_eq!(sliced.slices[1].ordinal, Some(2));
        // A marker without an ordinal carries none.
        let bare = phase_slices(&[event(1, "phase-entered", json!({ "name": "a" }))]);
        assert_eq!(bare.slices[0].ordinal, None);
    }

    #[test]
    fn phase_rows_free_orphan_reports_and_sort_newest_first() {
        let events = vec![
            event(1, "log", json!({ "message": "first" })),
            event(
                2,
                "node-dispatched",
                json!({ "instance": { "siteId": "s", "ordinal": 1 }, "actorName": "impl" }),
            ),
            event(
                3,
                "report",
                json!({ "instance": { "siteId": "s", "ordinal": 1 }, "item": "filed" }),
            ),
            event(
                4,
                "node-settled",
                json!({ "instance": { "siteId": "s", "ordinal": 1 }, "outcome": "ok" }),
            ),
            event(5, "log", json!({ "message": "last" })),
        ];
        let fold = reduce_slice(&TextCatalog, &events);
        let seqs = |rows: &[WorkflowRunEvent]| {
            rows.iter().map(|e| e.sequence).collect::<Vec<_>>()
        };
        // The step is owned here: its report stays inside the step.
        assert_eq!(seqs(&phase_rows(&fold, &|_| true)), vec![5, 1]);
        // Homed in a later slice (resume replay): the report rejoins the
        // phase, the lifecycle rows do not.
        assert_eq!(seqs(&phase_rows(&fold, &|_| false)), vec![5, 3, 1]);
    }

    #[test]
    fn phase_paints_covers_each_channel() {
        let events = vec![
            event(1, "phase-entered", json!({ "name": "a" })),
            event(2, "log", json!({ "message": "x" })),
            event(3, "phase-entered", json!({ "name": "b" })),
        ];
        let sliced = phase_slices(&events);
        let folds: Vec<SliceFold> = sliced
            .slices
            .iter()
            .map(|s| reduce_slice(&TextCatalog, &s.events))
            .collect();
        // A journal row paints.
        assert!(phase_paints(&folds[0], &sliced, 0));
        // An empty just-entered phase paints nothing — it must not become
        // the visible head over the content beneath it.
        assert!(!phase_paints(&folds[1], &sliced, 1));
    }
}
