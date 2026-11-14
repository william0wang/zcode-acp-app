//! Remote Terminal carrier state (ADR-0011, `.zcode/docs/remote-terminal-
//! requirements.md`): the workbench owns the Remote Terminal objects, the
//! append-only seq-numbered event logs, and the Command Turn state machine;
//! the hub is a dumb authenticated pipe. This module is the pure half —
//! frame shapes and registry transitions, no gpui and no sockets — so every
//! wire behavior is unit-testable without a hub. `runtime.rs` glues it to
//! the control connection and the panes.
//!
//! Frame summary (all JSON, `v: 1`; the hub relays verbatim):
//! - phone → here: `open {sessionId}` / `replay {afterSeq}` / `turn
//!   {command}` / `interrupt` / `close`
//! - here → phone: `state {terminal}` / `events {events}` / `error
//!   {code, message}`
//! - here → hub (consumed, never relayed): `push {title, body}` / `audit
//!   {line}` — composed by the driver, not this module
//! - hub → here (generated, never relayed): `subs {count}` — driver-only

use std::collections::{HashMap, VecDeque};

use serde_json::{json, Value};

/// Events kept per terminal (the replay window; `.zcode/docs/` "event log").
pub(crate) const MAX_EVENTS: usize = 200;

/// One terminal's lifecycle. `Gone` keeps the record (and its log): the next
/// `open` re-opens it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TermState {
    Opening,
    Ready,
    Gone,
}

impl TermState {
    fn wire(self) -> &'static str {
        match self {
            TermState::Opening => "opening",
            TermState::Ready => "ready",
            TermState::Gone => "gone",
        }
    }
}

/// A Command Turn in flight. Serial per terminal: a second `turn` while one
/// runs is `error busy`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RunningTurn {
    turn_id: String,
    command: String,
    started_at: u64,
}

/// What a settled turn reports back to the driver (audit + push payloads).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SettledTurn {
    pub turn_id: String,
    pub command: String,
    pub exit_code: i64,
    pub duration_ms: u64,
}

/// What `open` decided, so the driver knows whether to ask for a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenOutcome {
    /// No pane: the driver sends [`crate::runtime::RuntimeEvent`]'s spawn
    /// request and the workbench answers via `UiCommand::TerminalPaneResult`.
    SpawnNeeded,
    /// A spawn request is already out — do NOT ask again.
    SpawnPending,
    /// A pane the driver still holds a tap for re-bound (a control-link
    /// blip): `opening`→`ready` without a spawn.
    Rebound,
    /// The terminal was already `ready`: an idempotent re-open.
    AlreadyReady,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EventBody {
    Open {
        cwd: String,
    },
    Input {
        turn_id: String,
        text: String,
    },
    Output {
        turn_id: String,
        text: String,
    },
    Exit {
        turn_id: String,
        exit_code: i64,
        duration_ms: u64,
    },
    Note {
        text: String,
    },
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TermEvent {
    seq: u64,
    body: EventBody,
}

impl TermEvent {
    fn to_json(&self) -> Value {
        let base = json!({ "seq": self.seq });
        match &self.body {
            EventBody::Open { cwd } => {
                let mut v = base;
                v["kind"] = json!("open");
                v["cwd"] = json!(cwd);
                v
            }
            EventBody::Input { turn_id, text } => {
                let mut v = base;
                v["kind"] = json!("input");
                v["turnId"] = json!(turn_id);
                v["text"] = json!(text);
                v
            }
            EventBody::Output { turn_id, text } => {
                let mut v = base;
                v["kind"] = json!("output");
                v["turnId"] = json!(turn_id);
                v["text"] = json!(text);
                v
            }
            EventBody::Exit {
                turn_id,
                exit_code,
                duration_ms,
            } => {
                let mut v = base;
                v["kind"] = json!("exit");
                v["turnId"] = json!(turn_id);
                v["exitCode"] = json!(exit_code);
                v["durationMs"] = json!(duration_ms);
                v
            }
            EventBody::Note { text } => {
                let mut v = base;
                v["kind"] = json!("note");
                v["text"] = json!(text);
                v
            }
            EventBody::Close => {
                let mut v = base;
                v["kind"] = json!("close");
                v
            }
        }
    }
}

/// One Remote Terminal: the pairing between a session and ONE pane. Keyed by
/// session id in the registry (one per session, reused across turns).
#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteTerminal {
    terminal_id: String,
    session_id: String,
    cwd: String,
    state: TermState,
    turn: Option<RunningTurn>,
    /// A spawn request is out (the workbench has not answered yet) — a
    /// second `open` must not ask again.
    spawn_pending: bool,
    last_seq: u64,
    events: VecDeque<TermEvent>,
}

impl RemoteTerminal {
    fn terminal_json(&self) -> Value {
        json!({
            "terminalId": self.terminal_id,
            "sessionId": self.session_id,
            "cwd": self.cwd,
            "state": self.state.wire(),
            "turn": self.turn.as_ref().map(|t| json!({
                "turnId": t.turn_id,
                "command": t.command,
                "state": "running",
            })),
            "lastSeq": self.last_seq,
        })
    }

    fn state_frame(&self) -> String {
        frame(json!({ "type": "state", "terminal": self.terminal_json() }))
    }

    fn append(&mut self, body: EventBody) -> Value {
        self.last_seq += 1;
        let ev = TermEvent {
            seq: self.last_seq,
            body,
        };
        let out = ev.to_json();
        self.events.push_back(ev);
        if self.events.len() > MAX_EVENTS {
            self.events.pop_front();
        }
        out
    }

    /// The events past `after_seq`, oldest first.
    fn events_past(&self, after_seq: u64) -> Vec<Value> {
        self.events
            .iter()
            .filter(|e| e.seq > after_seq)
            .map(|e| e.to_json())
            .collect()
    }
}

/// A workbench-frame error: `{v, type:"error", code, message}`.
pub(crate) fn error_frame(code: &str, message: &str) -> String {
    frame(json!({ "type": "error", "code": code, "message": message }))
}

fn frame(mut body: Value) -> String {
    body["v"] = json!(1);
    body.to_string()
}

/// The hub relay caps one received frame at 256 KiB (its ws `maxPayload`)
/// and answers an oversized frame by killing the whole control socket — a
/// full-ring replay would be up to ~840 KiB. Burst events in budget-sized
/// frames instead: events are append-only, so a client folds chunked frames
/// exactly like live appends.
pub(crate) const MAX_EVENTS_FRAME_BYTES: usize = 96 * 1024;

/// One replay slice as one or more `events` frames under the byte budget
/// (an empty slice still answers — the phone's cursor sync needs a frame).
fn events_frames(events: &[Value]) -> Vec<String> {
    let mut frames = Vec::new();
    let mut batch: Vec<Value> = Vec::new();
    let mut bytes = 0usize;
    for event in events {
        let len = serde_json::to_string(event).map_or(0, |s| s.len());
        if !batch.is_empty() && bytes + len > MAX_EVENTS_FRAME_BYTES {
            frames.push(frame(json!({ "type": "events", "events": batch })));
            batch = Vec::new();
            bytes = 0;
        }
        batch.push(event.clone());
        bytes += len;
    }
    if !batch.is_empty() || frames.is_empty() {
        frames.push(frame(json!({ "type": "events", "events": batch })));
    }
    frames
}

/// The identity header the control connection carries (the hub's
/// DESKTOP_CLIENT_NAME): browsers cannot set custom WS headers, which is
/// exactly what keeps a phone from taking the carrier seat.
pub(crate) const DESKTOP_CLIENT_HEADER: (&str, &str) =
    ("x-zcode-client", "zcode-acp-remote-desktop");

/// The control connection's URL: hub scheme swap + `/api/desktop-control`.
/// Companion of `protocol::ws::ws_url` — no `instance` param; the identity
/// rides the header above instead.
pub(crate) fn control_ws_url(hub_url: &str) -> Result<String, String> {
    let (scheme, rest) = hub_url
        .split_once("://")
        .ok_or_else(|| format!("bad hub url (no scheme): {hub_url}"))?;
    let scheme = match scheme {
        "https" => "wss",
        "http" => "ws",
        other => other,
    };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() {
        return Err(format!("bad hub url (no host): {hub_url}"));
    }
    Ok(format!("{scheme}://{authority}/api/desktop-control"))
}

/// The standalone marker printf: the OSC 641 sequence carrying the exit
/// status (`%s` reads `$?` at run time). The emulator swallows the sequence;
/// the pane's tee reads it.
pub(crate) const MARKER_PRINTF: &str = "printf '\\e]641;%s\\a' \"$?\"";

/// The line written into the pane for one turn: the command plus an
/// invisible OSC 641 marker carrying its exit status (the emulator swallows
/// the sequence; the pane's tee reads it). Trailing `;`/whitespace is
/// trimmed first — `cmd;; printf` is a shell syntax error and would hang
/// the turn with no marker ever firing.
pub(crate) fn injection_line(command: &str) -> String {
    let trimmed = command.trim().trim_end_matches(';').trim_end();
    format!("{trimmed}; {MARKER_PRINTF}")
}

const MAX_TAIL_LINES: usize = 40;
const MAX_TAIL_BYTES: usize = 4 * 1024;

/// The literal scaffolding as the ECHO of a typed marker line shows it:
/// backslash-e is two plain characters here (the OSC itself only
/// materializes when printf runs, and ANSI strip leaves the text intact).
const MARKER_ECHO: &str = r"printf '\e]641;%s\a'";

/// Shape a turn's captured bytes into the wire tail: ANSI sequences
/// stripped (the pane keeps its full-fidelity copy — this is transcript
/// display only), CR/CRLF normalized to LF, the ECHOED injection line(s)
/// dropped, cut to the last 40 lines / 4 KiB.
///
/// The pty echoes every typed line into the tee — including the marker
/// scaffolding — so a raw capture opens with `cmd; printf '\e]641;…' "$?"`
/// before the command's own output. Dropping lines that carry the literal
/// scaffolding keeps the transcript to what the command printed
/// (acceptance 2: `echo hi` → `hi`). Both echo variants (the injected
/// `cmd; printf …` and the interrupt's standalone `printf …`) contain the
/// fragment; real output matching it byte-for-byte is pathological and
/// accepted.
pub(crate) fn turn_tail(bytes: &[u8]) -> String {
    let normalized = normalized_text(bytes);
    let filtered: Vec<&str> = normalized
        .lines()
        .filter(|line| !line.contains(MARKER_ECHO))
        .collect();
    cut_tail_lines(&filtered)
}

/// ANSI-stripped, CR/CRLF-normalized capture text (pre-tail).
fn normalized_text(bytes: &[u8]) -> String {
    let stripped = strip_ansi(bytes);
    let text = String::from_utf8_lossy(&stripped);
    let mut normalized = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            normalized.push('\n');
        } else {
            normalized.push(c);
        }
    }
    normalized
}

/// The last MAX_TAIL_LINES lines / MAX_TAIL_BYTES of the capture's lines
/// (one pass over pre-split lines — a re-join/re-split round trip drops
/// the final implied blank line `lines()` keeps).
fn cut_tail_lines(lines: &[&str]) -> String {
    let tail = if lines.len() > MAX_TAIL_LINES {
        &lines[lines.len() - MAX_TAIL_LINES..]
    } else {
        lines
    };
    let mut out = tail.join("\n");
    if out.len() > MAX_TAIL_BYTES {
        // Cut from the front (the END is the tail); land on a char boundary.
        let start = out.len() - MAX_TAIL_BYTES;
        let start = out
            .char_indices()
            .map(|(i, _)| i)
            .find(|i| *i >= start)
            .unwrap_or(start);
        out = out[start..].to_string();
    }
    out
}

/// Drop ANSI control sequences: CSI (`ESC [ … final`), OSC (`ESC ] …
/// BEL`/`ST`), and two-byte ESC escapes.
fn strip_ansi(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        match bytes.get(i + 1) {
            Some(b'[') => {
                i += 2;
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                i += 1; // the final byte
            }
            Some(b']') => {
                i += 2;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            Some(_) => i += 2, // two-byte ESC escape
            None => i += 1,    // trailing lone ESC
        }
    }
    out
}

/// All Remote Terminals this workbench process owns. Pure state; the driver
/// applies the returned frame strings to the control socket verbatim.
#[derive(Debug, Default)]
pub(crate) struct TerminalRegistry {
    next_terminal: u64,
    next_turn: u64,
    terminals: HashMap<String, RemoteTerminal>,
}

impl TerminalRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn reset(&mut self) {
        self.terminals.clear();
    }

    fn get(&self, session_id: &str) -> Option<&RemoteTerminal> {
        self.terminals.get(session_id)
    }

    /// A turn is in flight on the terminal (the driver's settle-poll guard).
    pub(crate) fn turn_running(&self, session_id: &str) -> bool {
        self.get(session_id).is_some_and(|t| t.turn.is_some())
    }

    /// Phone `open {sessionId}`: create/return the session's terminal. Always
    /// answers with a `state` frame plus a full replay from seq 0.
    pub(crate) fn open(
        &mut self,
        session_id: &str,
        cwd: &str,
        pane_alive: bool,
    ) -> (OpenOutcome, Vec<String>) {
        // Create-or-refresh the record first so the state frame names it.
        let outcome;
        if let Some(t) = self.terminals.get_mut(session_id) {
            if t.cwd.is_empty() {
                t.cwd = cwd.to_string();
            }
            match (t.state, pane_alive, t.spawn_pending) {
                (TermState::Ready, _, _) => outcome = OpenOutcome::AlreadyReady,
                (_, true, _) => {
                    // The pane outlived the control link (panes die with the
                    // app, not the link): re-bind it.
                    t.state = TermState::Ready;
                    t.append(EventBody::Open { cwd: t.cwd.clone() });
                    outcome = OpenOutcome::Rebound;
                }
                (TermState::Opening, false, true) => {
                    // Spawn already asked; the answer is on its way.
                    outcome = OpenOutcome::SpawnPending;
                }
                (TermState::Opening, false, false) => {
                    t.spawn_pending = true;
                    outcome = OpenOutcome::SpawnNeeded;
                }
                (TermState::Gone, false, _) => {
                    t.state = TermState::Opening;
                    t.spawn_pending = true;
                    outcome = OpenOutcome::SpawnNeeded;
                }
            }
        } else {
            self.next_terminal += 1;
            self.terminals.insert(
                session_id.to_string(),
                RemoteTerminal {
                    terminal_id: format!("t_{}", self.next_terminal),
                    session_id: session_id.to_string(),
                    cwd: cwd.to_string(),
                    state: TermState::Opening,
                    turn: None,
                    spawn_pending: true,
                    last_seq: 0,
                    events: VecDeque::new(),
                },
            );
            outcome = OpenOutcome::SpawnNeeded;
        }
        let t = &self.terminals[session_id];
        let mut frames = vec![t.state_frame()];
        frames.extend(events_frames(&t.events_past(0)));
        (outcome, frames)
    }

    /// Phone `replay {afterSeq}`: only seq > after_seq, never duplicates.
    pub(crate) fn replay(&self, session_id: &str, after_seq: u64) -> Vec<String> {
        match self.get(session_id) {
            Some(t) => events_frames(&t.events_past(after_seq)),
            None => vec![error_frame(
                "unknown_session",
                "no terminal for that session",
            )],
        }
    }

    /// Phone `turn {command}`: append `input`, enter `running`. Errors:
    /// `unknown_session` / `not_ready` / `busy` / `bad_request`.
    pub(crate) fn turn_start(
        &mut self,
        session_id: &str,
        command: &str,
        now_ms: u64,
    ) -> Result<Vec<String>, String> {
        let Some(t) = self.terminals.get_mut(session_id) else {
            return Err(error_frame(
                "unknown_session",
                "no terminal for that session",
            ));
        };
        // Shape first, state after: an empty command is bad regardless of
        // whether the terminal happens to be ready.
        if command.trim().is_empty() {
            return Err(error_frame("bad_request", "empty command"));
        }
        if t.state != TermState::Ready {
            return Err(error_frame("not_ready", "the terminal pane is not ready"));
        }
        if t.turn.is_some() {
            return Err(error_frame("busy", "a command turn is already running"));
        }
        self.next_turn += 1;
        let turn_id = format!("turn_{}", self.next_turn);
        let input = t.append(EventBody::Input {
            turn_id: turn_id.clone(),
            text: command.to_string(),
        });
        t.turn = Some(RunningTurn {
            turn_id,
            command: command.to_string(),
            started_at: now_ms,
        });
        Ok(vec![
            frame(json!({ "type": "events", "events": [input] })),
            t.state_frame(),
        ])
    }

    /// The OSC 641 marker fired: append `output` (tail) + `exit`, clear the
    /// turn, and hand the driver what settled (audit + push). Frames carry
    /// the appended events plus the turn-idle state.
    pub(crate) fn settle(
        &mut self,
        session_id: &str,
        exit_code: i64,
        tail: String,
        now_ms: u64,
    ) -> Option<(SettledTurn, Vec<String>)> {
        let t = self.terminals.get_mut(session_id)?;
        let turn = t.turn.take()?;
        let duration_ms = now_ms.saturating_sub(turn.started_at);
        let output = t.append(EventBody::Output {
            turn_id: turn.turn_id.clone(),
            text: tail,
        });
        let exit = t.append(EventBody::Exit {
            turn_id: turn.turn_id.clone(),
            exit_code,
            duration_ms,
        });
        let settled = SettledTurn {
            turn_id: turn.turn_id,
            command: turn.command,
            exit_code,
            duration_ms,
        };
        Some((
            settled,
            vec![
                frame(json!({ "type": "events", "events": [output, exit] })),
                t.state_frame(),
            ],
        ))
    }

    /// Phone `close`: close the record (the driver closes the pane). Same
    /// transition as the pane dying, minus the note.
    pub(crate) fn close(&mut self, session_id: &str) -> Vec<String> {
        self.gone(session_id, None)
    }

    /// The bound pane is gone (× button, shell exit, tab closed, restart):
    /// settle a running turn with a note, append `close`, state → `gone`.
    pub(crate) fn pane_gone(&mut self, session_id: &str) -> Vec<String> {
        self.gone(session_id, Some("pane closed while the turn was running"))
    }

    fn gone(&mut self, session_id: &str, running_note: Option<&str>) -> Vec<String> {
        let Some(t) = self.terminals.get_mut(session_id) else {
            return Vec::new();
        };
        let mut appended = Vec::new();
        if t.turn.take().is_some() {
            if let Some(note) = running_note {
                appended.push(t.append(EventBody::Note {
                    text: note.to_string(),
                }));
            }
        }
        t.spawn_pending = false;
        appended.push(t.append(EventBody::Close));
        t.state = TermState::Gone;
        let mut frames = vec![frame(json!({ "type": "events", "events": appended }))];
        frames.push(t.state_frame());
        frames
    }

    /// The workbench answered the spawn request (or a REMOTE pane restart
    /// re-bound a fresh tap): `opening` → `ready` with an `open` event. A
    /// turn still running against the replaced pane's dead PTY can never see
    /// its marker — settle it with a note. A late answer for a terminal that
    /// moved on (closed, link died) is dropped silently.
    pub(crate) fn pane_bound(&mut self, session_id: &str) -> Vec<String> {
        let Some(t) = self.terminals.get_mut(session_id) else {
            return Vec::new();
        };
        t.spawn_pending = false;
        if t.state == TermState::Gone {
            return Vec::new();
        }
        let mut appended = Vec::new();
        if t.turn.take().is_some() {
            appended.push(t.append(EventBody::Note {
                text: "pane replaced while the turn was running".to_string(),
            }));
        }
        t.state = TermState::Ready;
        appended.push(t.append(EventBody::Open { cwd: t.cwd.clone() }));
        vec![
            frame(json!({ "type": "events", "events": appended })),
            t.state_frame(),
        ]
    }

    /// The spawn was refused (pane cap / failure): surface the error and
    /// park the record `gone` — it never opened.
    pub(crate) fn pane_refused(
        &mut self,
        session_id: &str,
        code: &str,
        message: &str,
    ) -> Vec<String> {
        let mut frames = vec![error_frame(code, message)];
        if let Some(t) = self.terminals.get_mut(session_id) {
            t.spawn_pending = false;
            if t.state == TermState::Opening {
                t.state = TermState::Gone;
                frames.push(t.state_frame());
            }
        }
        frames
    }

    /// The control link died: every terminal goes `gone` (the hub already
    /// dropped the phone sockets — no frames to send; the logs survive for
    /// the post-reconnect `open`). A RUNNING turn is deliberately kept: the
    /// tap outlives the link (panes die with the app, not the link), so the
    /// settle tick keeps appending output/exit into the log even while
    /// disconnected, and the post-reconnect replay shows the turn's result
    /// (spec §Reconnect). Clearing it here would orphan the `input` event
    /// with no completion, forever.
    pub(crate) fn detach_all(&mut self) {
        for t in self.terminals.values_mut() {
            t.spawn_pending = false;
            t.state = TermState::Gone;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind_seq(frame: &str, kind: &str) -> Vec<(u64, String)> {
        let v: Value = serde_json::from_str(frame).unwrap();
        v["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["kind"] == kind)
            .map(|e| {
                (
                    e["seq"].as_u64().unwrap(),
                    e["text"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect()
    }

    fn ready_terminal() -> TerminalRegistry {
        let mut r = TerminalRegistry::new();
        let (outcome, _) = r.open("s1", "/tmp", false);
        assert_eq!(outcome, OpenOutcome::SpawnNeeded);
        r.pane_bound("s1");
        r
    }

    #[test]
    fn open_creates_spawn_needed_and_replays_from_zero() {
        let mut r = TerminalRegistry::new();
        let (outcome, frames) = r.open("s1", "/tmp", false);
        assert_eq!(outcome, OpenOutcome::SpawnNeeded);
        assert_eq!(frames.len(), 2);
        let state: Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(state["terminal"]["state"], "opening");
        assert_eq!(state["terminal"]["lastSeq"], 0);
        let events: Value = serde_json::from_str(&frames[1]).unwrap();
        assert_eq!(events["events"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn pane_bound_appends_open_event_and_flips_ready() {
        let mut r = TerminalRegistry::new();
        r.open("s1", "/tmp", false);
        let frames = r.pane_bound("s1");
        assert_eq!(frames.len(), 2);
        assert!(frames[0].contains("\"kind\":\"open\""));
        assert!(frames[1].contains("\"state\":\"ready\""));
        // open() on a live record stays idempotent — no second open event.
        let (outcome, frames) = r.open("s1", "/tmp", false);
        assert_eq!(outcome, OpenOutcome::AlreadyReady);
        assert!(frames[0].contains("\"state\":\"ready\""));
        assert_eq!(kind_seq(&r.replay("s1", 0)[0], "open").len(), 1);
    }

    #[test]
    fn turn_lifecycle_input_output_exit() {
        let mut r = ready_terminal();
        let frames = r.turn_start("s1", "npm test", 1_000).unwrap();
        assert_eq!(
            kind_seq(&frames[0], "input")[0],
            (2, "npm test".to_string())
        );
        let state: Value = serde_json::from_str(&frames[1]).unwrap();
        assert_eq!(state["terminal"]["turn"]["state"], "running");

        let second = r.turn_start("s1", "again", 1_100);
        assert!(second.unwrap_err().contains("\"busy\""));

        let (settled, frames) = r.settle("s1", 0, "ok\n".into(), 3_000).unwrap();
        assert_eq!(settled.exit_code, 0);
        assert_eq!(settled.duration_ms, 2_000);
        assert_eq!(kind_seq(&frames[0], "output")[0].1, "ok\n");
        let exit: Value = frames[0].parse().unwrap();
        assert_eq!(exit["events"][1]["kind"], "exit");
        assert_eq!(exit["events"][1]["exitCode"], 0);
        assert_eq!(exit["events"][1]["durationMs"], 2_000);
        // Turn cleared: a new turn is accepted.
        assert!(r.turn_start("s1", "next", 4_000).is_ok());
    }

    #[test]
    fn turn_guards_unknown_not_ready_bad_request() {
        let mut r = TerminalRegistry::new();
        assert!(r
            .turn_start("nope", "x", 0)
            .unwrap_err()
            .contains("unknown_session"));

        let (outcome, _) = r.open("s1", "/tmp", false);
        assert_eq!(outcome, OpenOutcome::SpawnNeeded);
        assert!(r
            .turn_start("s1", "x", 0)
            .unwrap_err()
            .contains("not_ready"));
        assert!(r.turn_start("s1", "   ", 0).is_err());
        assert!(r
            .turn_start("s1", "", 0)
            .unwrap_err()
            .contains("bad_request"));
    }

    #[test]
    fn replay_after_seq_returns_only_newer() {
        let mut r = ready_terminal();
        r.turn_start("s1", "one", 0).unwrap();
        r.settle("s1", 0, "a".into(), 5).unwrap();
        r.turn_start("s1", "two", 10).unwrap();
        r.settle("s1", 1, "b".into(), 15).unwrap();
        let seqs: Vec<u64> = {
            let t = r.get("s1").unwrap();
            t.events.iter().map(|e| e.seq).collect()
        };
        let mid = seqs[2]; // after the first turn's events
        let frames = r.replay("s1", mid);
        let v: Value = serde_json::from_str(&frames[0]).unwrap();
        let got: Vec<u64> = v["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect();
        assert!(got.iter().all(|s| *s > mid));
        assert_eq!(got.len(), seqs.len() - 3);
        assert!(r.replay("nope", 0)[0].contains("unknown_session"));
    }

    #[test]
    fn turn_tail_drops_the_echoed_marker_scaffolding() {
        // A realistic capture: the pty echo of the injected line (ANSI
        // highlight around it), the command's output, the standalone
        // interrupt marker's echo, and the OSC marker itself. Only the
        // command's own output may survive into the wire tail.
        let echo = b"\x1b[32mecho hi; printf '\\e]641;%s\\a' \"$?\"\x1b[0m\r\n";
        let mut capture = echo.to_vec();
        capture.extend_from_slice(b"hi\r\n");
        capture.extend_from_slice(b"^C\n");
        capture.extend_from_slice(b"printf '\\e]641;%s\\a' \"$?\"\r\n");
        capture.extend_from_slice(b"\x1b]641;130\x07");
        assert_eq!(turn_tail(&capture), "hi\n^C");
        // Without any scaffolding the capture passes through untouched.
        assert_eq!(turn_tail(b"plain\r\noutput\r\n"), "plain\noutput");
    }

    #[test]
    fn replay_burst_is_chunked_under_the_hub_frame_cap() {
        // 150 turns of 1.5 KiB tails ≈ 225 KiB of events: one frame would
        // trip the hub relay's 256 KiB maxPayload (it kills the control
        // socket), so the burst must arrive as several budget-sized frames.
        let mut r = ready_terminal();
        for i in 0..150 {
            r.turn_start("s1", &format!("cmd {i}"), i * 10).unwrap();
            r.settle("s1", 0, "x".repeat(1500), i * 10 + 5).unwrap();
        }
        let frames = r.replay("s1", 0);
        assert!(frames.len() > 1, "burst must be chunked");
        let mut seqs = Vec::new();
        for frame in &frames {
            assert!(frame.len() < 256 * 1024, "frame over the hub cap");
            let v: Value = serde_json::from_str(frame).unwrap();
            assert_eq!(v["type"], "events");
            seqs.extend(
                v["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e["seq"].as_u64().unwrap()),
            );
        }
        assert_eq!(seqs.len(), 200); // the ring caps the log at MAX_EVENTS
        assert!(seqs.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn close_and_pane_gone_append_close_and_go_gone() {
        let mut r = ready_terminal();
        let frames = r.close("s1");
        assert_eq!(frames.len(), 2);
        assert!(frames[0].contains("\"kind\":\"close\""));
        assert!(frames[1].contains("\"state\":\"gone\""));

        // A running turn dying with the pane settles with a note.
        let mut r = ready_terminal();
        r.turn_start("s1", "long", 0).unwrap();
        let frames = r.pane_gone("s1");
        assert!(frames[0].contains("\"kind\":\"note\""));
        assert!(!r.turn_running("s1"));
    }

    #[test]
    fn pane_refused_reports_and_parks_gone() {
        let mut r = TerminalRegistry::new();
        r.open("s1", "/tmp", false);
        let frames = r.pane_refused("s1", "pane_full", "terminal pane cap reached");
        assert!(frames[0].contains("pane_full"));
        assert!(frames[1].contains("\"state\":\"gone\""));
    }

    #[test]
    fn open_after_gone_rebounds_a_live_pane() {
        let mut r = ready_terminal();
        r.close("s1");
        // Control blip: detach_all keeps records; the driver kept its taps.
        r.detach_all();
        let (outcome, frames) = r.open("s1", "/tmp", true);
        assert_eq!(outcome, OpenOutcome::Rebound);
        assert!(frames[0].contains("\"state\":\"ready\""));
        // A second open event was appended (the log narrates the re-open).
        assert_eq!(kind_seq(&r.replay("s1", 0)[0], "open").len(), 2);
        assert!(r.turn_start("s1", "x", 0).is_ok());
    }

    #[test]
    fn open_while_spawn_pending_does_not_double_ask() {
        let mut r = TerminalRegistry::new();
        let (outcome, _) = r.open("s1", "/tmp", false);
        assert_eq!(outcome, OpenOutcome::SpawnNeeded);
        let (outcome, _) = r.open("s1", "/tmp", false);
        assert_eq!(outcome, OpenOutcome::SpawnPending);
        // One bind settles the pending request — a single open event.
        r.pane_bound("s1");
        assert_eq!(kind_seq(&r.replay("s1", 0)[0], "open").len(), 1);
    }

    #[test]
    fn event_log_is_a_ring_of_max_events() {
        let mut r = ready_terminal();
        for i in 0..(MAX_EVENTS + 20) {
            r.turn_start("s1", &format!("cmd{i}"), i as u64).unwrap();
            r.settle("s1", 0, String::new(), i as u64 + 1).unwrap();
        }
        let t = r.get("s1").unwrap();
        assert_eq!(t.events.len(), MAX_EVENTS);
        let seqs: Vec<u64> = t.events.iter().map(|e| e.seq).collect();
        assert!(seqs.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(*seqs.last().unwrap(), t.last_seq);
    }

    #[test]
    fn detach_all_keeps_a_running_turn_for_the_post_reconnect_replay() {
        let mut r = ready_terminal();
        r.turn_start("s1", "x", 0).unwrap();
        r.detach_all();
        // The turn survives the link death (the tap outlives the link), so
        // the settle below still lands in the LOG — frames go nowhere while
        // disconnected; the replay after reconnect carries the result.
        assert!(r.turn_running("s1"));
        let (_, frames) = r.settle("s1", 130, "aborted".into(), 5).unwrap();
        assert!(frames[0].contains("\"exitCode\":130"));
        let replay: Vec<Value> = r
            .replay("s1", 0)
            .iter()
            .map(|f| serde_json::from_str(f).unwrap())
            .collect();
        let kinds: Vec<&str> = replay
            .iter()
            .flat_map(|v| {
                v["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e["kind"].as_str().unwrap())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert!(kinds.contains(&"exit"));
        // The record itself still went gone; a re-open (pane died with the
        // link in this scenario) spawns fresh.
        let (outcome, frames) = r.open("s1", "/tmp", false);
        assert_eq!(outcome, OpenOutcome::SpawnNeeded);
        assert!(frames[0].contains("\"state\":\"opening\""));
    }

    #[test]
    fn terminal_ids_and_turn_ids_are_stable_and_unique() {
        let mut r = TerminalRegistry::new();
        r.open("s1", "/a", false);
        r.open("s2", "/b", false);
        let t1 = r.get("s1").unwrap().terminal_id.clone();
        let t2 = r.get("s2").unwrap().terminal_id.clone();
        assert_ne!(t1, t2);
        r.pane_bound("s1");
        r.pane_bound("s2");
        r.turn_start("s1", "a", 0).unwrap();
        r.turn_start("s2", "b", 0).unwrap();
        let id1 = r.get("s1").unwrap().turn.as_ref().unwrap().turn_id.clone();
        let id2 = r.get("s2").unwrap().turn.as_ref().unwrap().turn_id.clone();
        assert_ne!(id1, id2);
    }

    #[test]
    fn control_ws_url_swaps_scheme_and_targets_the_endpoint() {
        assert_eq!(
            control_ws_url("http://127.0.0.1:18377").unwrap(),
            "ws://127.0.0.1:18377/api/desktop-control"
        );
        assert_eq!(
            control_ws_url("https://hub.example.com/base/").unwrap(),
            "wss://hub.example.com/api/desktop-control"
        );
        assert!(control_ws_url("hub").is_err());
        assert!(control_ws_url("http://").is_err());
    }

    #[test]
    fn injection_line_appends_the_marker_and_trims_trailing_semis() {
        assert_eq!(
            injection_line("echo hi"),
            "echo hi; printf '\\e]641;%s\\a' \"$?\""
        );
        assert_eq!(
            injection_line("  sleep 1 ; "),
            "sleep 1; printf '\\e]641;%s\\a' \"$?\""
        );
    }

    #[test]
    fn turn_tail_strips_ansi_normalizes_crlf_and_cuts_lines() {
        let raw = b"$ \x1b[32mok\x1b[0m\r\nline2\r\r\n";
        // The CR CR/LF tail normalizes to two newlines; `lines` keeps the
        // one blank line they imply (only the FINAL newline is a terminator).
        assert_eq!(turn_tail(raw), "$ ok\nline2\n");
        assert!(!turn_tail(raw).contains('\x1b'));

        let many = (0..60)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tail = turn_tail(many.as_bytes());
        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines.len(), MAX_TAIL_LINES);
        assert_eq!(lines[0], "l20");

        let wide = vec![b'a'; MAX_TAIL_BYTES + 100];
        let tail = turn_tail(&wide);
        assert!(tail.len() <= MAX_TAIL_BYTES);
        assert!(tail.chars().all(|c| c == 'a'));
    }
}
