//! The embedded terminal dock: the vendored `gpui-terminal` view in a
//! bottom panel under the chat column (workbench `render_chat_column`
//! splits against it with `v_resizable`, the left dock's horizontal
//! pattern turned vertical).
//!
//! This module owns everything shell-side — the portable-pty PTY pair, the
//! login shell spawn (cwd = the owning tab's session workspace), the resize
//! plumbing, and the OSC 52 clipboard store — while `workbench.rs` only
//! toggles, renders, and yields Esc. The terminal is view-local layout
//! state, never wire state: hiding keeps the shell, while a shell exit or
//! the restart button drops the dock (drop → child kill) and the next open
//! spawns fresh in the current session's directory.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use gpui::{div, px, AnyElement, Context, Entity, FocusHandle, WeakEntity};
// The kit's root glob carries the `gpui` alias (`pub use crate as gpui`) —
// the app's one-gpui-identity trick every sibling module shares.
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    h_resizable, resizable_panel, ActiveTheme as _, Disableable as _, Icon, Sizable as _,
};
use gpui_kit::*;
use gpui_kit::{assets::IconName as KitIconName, TestSupportExt as _};
use gpui_terminal::{ColorPalette, TerminalConfig, TerminalView};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use rust_i18n::t;

use crate::ui_prefs::ui_text;
use crate::workbench::Workbench;

/// One live terminal pane: the vendored view plus the process handles that
/// outlive it. Dropping the dock kills the shell — a closed panel never
/// leaves a headless process behind.
pub(crate) struct TerminalDock {
    pub(crate) view: Entity<TerminalView>,
    /// The view's focus handle, cloned at spawn — the workbench's Esc
    /// ladder yields to the shell while this is the focused element.
    pub(crate) focus: FocusHandle,
    /// The pane's remote side-channel (ADR-0011): v1 remote turns ride it,
    /// and the Terminal Mirror (2026-11-20) reads it for EVERY pane — busy
    /// markers scan all streams, interrupts can target any pane.
    pub(crate) tap: RemotePaneTap,
    /// The dock's own handle on the PTY master (the resize callback holds
    /// the other Arc clone) — kept so the struct reads as the owner. `None`
    /// for the in-memory test dock, which has no PTY.
    _pty: Option<Arc<Mutex<Box<dyn MasterPty + Send>>>>,
    /// Taken by `Drop` — see there for why the kill runs detached.
    child: Mutex<Option<Box<dyn Child + Send + Sync>>>,
}

impl Drop for TerminalDock {
    fn drop(&mut self) {
        // portable-pty's kill is SIGHUP + a poll ladder (~200ms) + SIGKILL
        // with no reap: off the UI thread, and wait() closes the zombie
        // gap the SIGKILL branch leaves. The master dropping here first
        // only HUPs the slave a second time.
        let child = self.child.lock().unwrap().take();
        if let Some(mut child) = child {
            std::thread::spawn(move || {
                let _ = child.kill();
                let _ = child.wait();
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Remote pane tap (ADR-0011): the write/read side-channel the driver's
// Remote Terminal turns ride. Every dock gets one — local panes simply never
// arm the tee, and the shared writer costs one uncontended mutex lock.
// ---------------------------------------------------------------------------

/// Captured turn output: raw PTY bytes from `arm()` until the OSC 641
/// marker fires. The marker itself never renders in the pane (alacritty
/// swallows unknown OSC codes), so detection lives HERE, on the byte stream.
#[derive(Debug, Default)]
struct TeeBuffer {
    armed: bool,
    buf: Vec<u8>,
    exit: Option<i32>,
    /// The pane's reader hit EOF/error — the shell behind the tap is dead.
    /// The view's exit callback only runs on render, so a HIDDEN dock never
    /// reports the death; the driver's settle tick polls this instead.
    closed: bool,
    /// Terminal Mirror (2026-11-20 addition): OSC 642/643 busy markers parse
    /// on EVERY pane's stream (armed or not) into this queue; the unprocessed
    /// tail window below carries split markers across reads.
    busy_events: VecDeque<BusyEvent>,
    busy_scan: Vec<u8>,
}

/// Keep the tail window bounded: a `yes`-grade turn must not grow memory;
/// anything older than this window is droppable the moment it arrives (the
/// pane keeps its own 10k-line scrollback).
const TEE_CAP: usize = 64 * 1024;
/// Marker prefix (`ESC ] 641 ;`) + slack for a code split across reads.
const MARKER_CARRY: usize = 16;

impl TeeBuffer {
    fn arm(&mut self) {
        self.buf.clear();
        self.exit = None;
        self.armed = true;
    }

    fn append(&mut self, bytes: &[u8]) {
        // The mirror's markers scan every pane's stream, armed or not.
        self.scan_busy(bytes);
        if !self.armed {
            return;
        }
        let new_len = bytes.len();
        self.buf.extend_from_slice(bytes);
        if self.buf.len() > TEE_CAP {
            let drop = self.buf.len() - TEE_CAP;
            self.buf.drain(..drop);
        }
        self.scan_marker(new_len);
    }

    /// Find `ESC ] 641 ; <digits> (BEL | ESC \)` in the freshly appended
    /// window (plus carry-back for a marker split across reads); on a hit,
    /// record the code, end the capture AT the marker, and stop appending.
    fn scan_marker(&mut self, new_len: usize) {
        let from = self.buf.len().saturating_sub(new_len + MARKER_CARRY);
        let Some((rel, code)) = find_marker(&self.buf[from..]) else {
            return;
        };
        self.exit = Some(code);
        self.buf.truncate(from + rel);
        self.armed = false;
    }

    fn take_output(&mut self) -> Vec<u8> {
        self.armed = false;
        self.exit = None;
        std::mem::take(&mut self.buf)
    }
}

/// The first COMPLETE marker in `window`: `(offset, exit_code)`. A marker
/// whose terminator has not arrived yet (digits running to the window end)
/// is left for the next chunk.
fn find_marker(window: &[u8]) -> Option<(usize, i32)> {
    const PREFIX: &[u8] = b"]641;";
    const ESC: u8 = 0x1b;
    for i in 0..window.len().saturating_sub(PREFIX.len() + 1) {
        if window[i] != ESC || &window[i + 1..i + 1 + PREFIX.len()] != PREFIX {
            continue;
        }
        let mut j = i + 1 + PREFIX.len();
        let digits_start = j;
        while j < window.len() && window[j].is_ascii_digit() {
            j += 1;
        }
        if j == digits_start {
            continue; // prefix but no code — not ours (or not yet)
        }
        if j < window.len() && window[j] == 0x07 {
            return Some((i, parse_code(&window[digits_start..j])));
        }
        if j + 1 < window.len() && window[j] == ESC && window[j + 1] == b'\\' {
            return Some((i, parse_code(&window[digits_start..j])));
        }
        // digits at the very end without a terminator: possibly split —
        // keep scanning (a later complete marker still wins).
    }
    None
}

fn parse_code(digits: &[u8]) -> i32 {
    let mut code: i32 = 0;
    for d in digits {
        code = code.saturating_mul(10).saturating_add((d - b'0') as i32);
    }
    code
}

// ---------------------------------------------------------------------------
// Terminal Mirror busy markers (2026-11-20 addition): OSC 642 (command
// start) / OSC 643 (prompt return) emitted by the shell hooks the spawn
// injects. Like 641, the emulator swallows them from display — detection
// lives HERE, on the byte stream, for every pane.
// ---------------------------------------------------------------------------

/// One parsed busy marker, drained by the driver's mirror tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BusyEvent {
    /// OSC 642 — a command started (preexec), the hook-sanitized line.
    CommandStart(String),
    /// OSC 643 — the prompt returned (precmd), the shell's `$?`.
    PromptReturn(i32),
}

/// Bound the busy scan: a partial marker survives the trim (they are ~200
/// bytes with payload), a `yes`-grade flood keeps the memory bounded.
const BUSY_WINDOW_CAP: usize = 1024;
const BUSY_WINDOW_KEEP: usize = 768;
const MAX_BUSY_EVENTS: usize = 64;

impl TeeBuffer {
    /// Feed the busy scanner and drain what completed. Markers may split
    /// across reads; the unprocessed window keeps the tail for the next one.
    fn scan_busy(&mut self, bytes: &[u8]) {
        self.busy_scan.extend_from_slice(bytes);
        while let Some((consume, event)) = find_busy_marker(&self.busy_scan) {
            self.busy_scan.drain(..consume);
            if self.busy_events.len() >= MAX_BUSY_EVENTS {
                self.busy_events.pop_front();
            }
            self.busy_events.push_back(event);
        }
        if self.busy_scan.len() > BUSY_WINDOW_CAP {
            let drop = self.busy_scan.len() - BUSY_WINDOW_KEEP;
            self.busy_scan.drain(..drop);
        }
    }
}

/// The first COMPLETE busy marker in `window`: `(bytes to consume — any
/// prefix plus the marker through its terminator — the event)`. A marker
/// whose terminator has not arrived yet is left for the next chunk (a later
/// complete marker still wins). Payload rules: 642 takes anything but the
/// two terminators (the hook already stripped control bytes), 643 digits
/// only — anything else is not ours (v1's 641 included).
fn find_busy_marker(window: &[u8]) -> Option<(usize, BusyEvent)> {
    const MAX_PAYLOAD: usize = 512;
    let mut i = 0;
    while i + 5 < window.len() {
        if window[i] != 0x1b || window.get(i + 1) != Some(&b']') {
            i += 1;
            continue;
        }
        let kind = match (
            window.get(i + 2),
            window.get(i + 3),
            window.get(i + 4),
            window.get(i + 5),
        ) {
            (Some(b'6'), Some(b'4'), Some(b'2'), Some(b';')) => 2,
            (Some(b'6'), Some(b'4'), Some(b'3'), Some(b';')) => 3,
            _ => {
                i += 1;
                continue;
            }
        };
        let start = i + 6;
        let mut j = start;
        let terminated = if kind == 2 {
            while j < window.len()
                && window[j] != 0x07
                && window[j] != 0x1b
                && j - start < MAX_PAYLOAD
            {
                j += 1;
            }
            j < window.len()
                && (window[j] == 0x07
                    || (window[j] == 0x1b && window.get(j + 1) == Some(&b'\\')))
        } else {
            while j < window.len() && window[j].is_ascii_digit() {
                j += 1;
            }
            j > start
                && j < window.len()
                && (window[j] == 0x07
                    || (window[j] == 0x1b && window.get(j + 1) == Some(&b'\\')))
        };
        if !terminated {
            i += 1;
            continue;
        }
        let end = if window[j] == 0x07 { j + 1 } else { j + 2 };
        let payload = &window[start..j];
        let event = if kind == 2 {
            BusyEvent::CommandStart(String::from_utf8_lossy(payload).to_string())
        } else {
            BusyEvent::PromptReturn(parse_code(payload))
        };
        return Some((end, event));
    }
    None
}

/// Writer both the view's key handling and the driver's remote turns write
/// through — one mutex serializes the two onto the same PTY master.
struct SharedPtyWriter(Arc<Mutex<Box<dyn std::io::Write + Send>>>);

impl std::io::Write for SharedPtyWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

/// Reader the view's feed runs through: passthrough bytes (the emulator
/// consumes the marker invisibly) + a tee copy for the turn tail.
struct TeeReader {
    inner: Box<dyn std::io::Read + Send>,
    tee: Arc<Mutex<TeeBuffer>>,
}

impl std::io::Read for TeeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = match self.inner.read(buf) {
            Ok(n) => n,
            Err(e) => {
                // EOF's sibling on some platforms (EIO once the slave side
                // is fully gone): a dead pane either way.
                self.tee.lock().unwrap().closed = true;
                return Err(e);
            }
        };
        let mut tee = self.tee.lock().unwrap();
        if n > 0 {
            tee.append(&buf[..n]);
        } else {
            // EOF: the shell exited (or every slave fd closed).
            tee.closed = true;
        }
        Ok(n)
    }
}

/// The driver's handle onto one pane's PTY (ADR-0011): input lines and
/// interrupts down, the captured turn tail up. Cheap to clone; the workbench
/// reports pane death separately (`UiCommand::TerminalPaneGone`).
#[derive(Clone)]
pub(crate) struct RemotePaneTap {
    writer: Arc<Mutex<Box<dyn std::io::Write + Send>>>,
    tee: Arc<Mutex<TeeBuffer>>,
}

impl std::fmt::Debug for RemotePaneTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemotePaneTap")
            .field("armed", &self.tee.lock().unwrap().armed)
            .finish_non_exhaustive()
    }
}

impl RemotePaneTap {
    /// One input line into the shell, CR-terminated like a typed Enter.
    pub(crate) fn write_line(&self, line: &str) {
        self.write_raw(format!("{line}\r").as_bytes());
    }

    /// Raw bytes (the interrupt is `\u{3}`).
    pub(crate) fn write_raw(&self, bytes: &[u8]) {
        use std::io::Write as _;
        // Best-effort, like the view's own send: a dead PTY surfaces through
        // the pane-gone path instead.
        let _ = self.writer.lock().unwrap().write_all(bytes);
    }

    /// Begin capturing for a new turn (drops any previous capture).
    pub(crate) fn arm(&self) {
        self.tee.lock().unwrap().arm();
    }

    /// The OSC 641 exit code once the marker fired (peeked by the tick).
    pub(crate) fn exit_code(&self) -> Option<i32> {
        self.tee.lock().unwrap().exit
    }

    /// Drain the captured output (resets armed + exit).
    pub(crate) fn take_output(&self) -> Vec<u8> {
        self.tee.lock().unwrap().take_output()
    }

    /// The pane's reader hit EOF/error — the shell behind the tap is dead.
    pub(crate) fn is_closed(&self) -> bool {
        self.tee.lock().unwrap().closed
    }

    /// Drain the parsed busy markers (Terminal Mirror; the driver's tick).
    pub(crate) fn take_busy_events(&self) -> Vec<BusyEvent> {
        self.tee.lock().unwrap().busy_events.drain(..).collect()
    }

    /// A sink-backed tap with no PTY (tests): registry-flow wiring only.
    #[cfg(test)]
    pub(crate) fn in_memory_for_test() -> Self {
        Self::in_memory().0
    }

    /// Simulate PTY output arriving through the pane's reader (tests — the
    /// real feed is the `TeeReader` the view's thread pulls).
    #[cfg(test)]
    pub(crate) fn feed_output_for_test(&self, bytes: &[u8]) {
        self.tee.lock().unwrap().append(bytes);
    }

    /// Simulate the pane's reader hitting EOF (tests — a dead shell).
    #[cfg(test)]
    pub(crate) fn feed_eof_for_test(&self) {
        self.tee.lock().unwrap().closed = true;
    }

    /// An in-memory tap (tests): writes land in a sink you can read back,
    /// the tee is real. Returns the tap plus the sink.
    #[cfg(test)]
    pub(crate) fn in_memory() -> (Self, Arc<Mutex<Vec<u8>>>) {
        let sink = Arc::new(Mutex::new(Vec::new()));
        struct SinkWriter(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for SinkWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        (
            Self {
                writer: Arc::new(Mutex::new(Box::new(SinkWriter(sink.clone())))),
                tee: Arc::new(Mutex::new(TeeBuffer::default())),
            },
            sink,
        )
    }
}

/// zsh's init-file shims, written under `config_dir()/terminal-shim/zsh`:
/// ZDOTDIR points there, the ORIGINAL zdotdir rides `ZCODE_ACP_ORIG_ZDOTDIR`,
/// both files pass through to the user's own init first (the hooks append
/// AFTER the user's rc so a framework cannot order itself ahead of them).
const ZSH_ZSHENV_SHIM: &str = r#"__zcode_orig="${ZCODE_ACP_ORIG_ZDOTDIR:-$HOME}"
[[ -f "$__zcode_orig/.zshenv" ]] && source "$__zcode_orig/.zshenv"
unset __zcode_orig
"#;

const ZSH_ZSHRC_SHIM: &str = r#"__zcode_orig="${ZCODE_ACP_ORIG_ZDOTDIR:-$HOME}"
[[ -f "$__zcode_orig/.zshrc" ]] && source "$__zcode_orig/.zshrc"
# Terminal Mirror busy hooks: printable-only payload (no BEL/ESC can smuggle
# past the marker terminator), capped where the scanner stops caring.
__zcode_acp_preexec() {
  local c=${1//[^[:print:]]/}
  printf '\e]642;%s\a' "${c[1,200]}"
}
__zcode_acp_precmd() { printf '\e]643;%s\a' "$?" }
preexec_functions+=(__zcode_acp_preexec)
precmd_functions+=(__zcode_acp_precmd)
unset __zcode_orig
"#;

/// bash runs with `--rcfile` pointing here (the user's own rc sources
/// first). bash has no preexec — the DEBUG trap approximates it and fires
/// per simple command; the mirror keeps the last text while busy, and our
/// own hook commands are skipped so the prompt machinery stays quiet.
const BASH_RC_SHIM: &str = r#"[[ -f "$HOME/.bashrc" ]] && source "$HOME/.bashrc"
__zcode_acp_preexec() {
  case "$BASH_COMMAND" in __zcode_acp_*) return ;; esac
  local c="$BASH_COMMAND"
  c=${c//[^[:print:]]/}
  printf '\e]642;%s\a' "${c:0:200}"
}
__zcode_acp_precmd() { printf '\e]643;%s\a' "$?"; }
trap '__zcode_acp_preexec' DEBUG
PROMPT_COMMAND="__zcode_acp_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
"#;

/// Inject the busy hooks for shells we know how to shim (zsh via ZDOTDIR,
/// bash via --rcfile); foreign shells and failed shim writes run unhooked
/// (Busy State `unknown` — the spec's degradation, never a spawn failure).
fn apply_busy_shim(cmd: &mut CommandBuilder, shell: &str) {
    let base = std::path::Path::new(shell)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let shim_dir = protocol::profiles::config_dir().join("terminal-shim");
    if base.contains("zsh") {
        let dir = shim_dir.join("zsh");
        if write_shim(&dir, ".zshenv", ZSH_ZSHENV_SHIM)
            && write_shim(&dir, ".zshrc", ZSH_ZSHRC_SHIM)
        {
            cmd.env("ZDOTDIR", &dir);
            // The original ZDOTDIR the user's init lives in ($HOME when
            // unset — the common case; keep empty-var noise out of the env).
            let orig = std::env::var("ZDOTDIR").unwrap_or_default();
            let orig = if orig.is_empty() {
                std::env::var("HOME").unwrap_or_default()
            } else {
                orig
            };
            if !orig.is_empty() {
                cmd.env("ZCODE_ACP_ORIG_ZDOTDIR", orig);
            }
        }
    } else if base.contains("bash") {
        if write_shim(&shim_dir, "bashrc", BASH_RC_SHIM) {
            cmd.arg("--rcfile");
            cmd.arg(shim_dir.join("bashrc"));
        }
    }
}

/// Best-effort shim write (dir + file); false means "run unhooked".
fn write_shim(dir: &std::path::Path, name: &str, contents: &str) -> bool {
    std::fs::create_dir_all(dir).is_ok() && std::fs::write(dir.join(name), contents).is_ok()
}

/// Spawn the login shell in a fresh PTY at `workspace` (None = home) and
/// wire the view's callbacks into the workbench. `wb` is weak: the exit
/// callback closes the panel from the next frame, past the view's own
/// render borrow. The returned tap is the pane's remote side-channel
/// (ADR-0011) — the mirror scans it on every pane, v1 turns arm it.
pub(crate) fn spawn_dock(
    workspace: Option<&str>,
    wb: WeakEntity<Workbench>,
    cx: &mut Context<Workbench>,
) -> Result<(TerminalDock, RemotePaneTap), String> {
    let theme = cx.theme().clone();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| e.to_string())?;
    let mut cmd = CommandBuilder::new(&shell);
    if let Some(dir) = workspace {
        cmd.cwd(dir);
    }
    // Richer output on: the same env the vendored crate's README asks for.
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    // Terminal Mirror busy hooks (2026-11-20): the app owns this spawn, so
    // it hands the shell preexec/precmd hooks that report command start and
    // prompt return as OSC 642/643 markers. Shells we cannot shim (or a
    // failed shim write) run unhooked — the pane's Busy State stays
    // `unknown`, interrupts still work (`\x03` needs no detection).
    apply_busy_shim(&mut cmd, &shell);
    let child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
    let writer: Arc<Mutex<Box<dyn std::io::Write + Send>>> =
        Arc::new(Mutex::new(pair.master.take_writer().map_err(|e| e.to_string())?));
    let reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
    let pty = Arc::new(Mutex::new(pair.master));
    drop(pair.slave);

    let config = TerminalConfig {
        cols: 80,
        rows: 24,
        font_family: theme.mono_font_family.to_string(),
        font_size: ui_text(13.),
        scrollback: 10_000,
        line_height_multiplier: 1.0,
        padding: gpui::Edges::all(px(6.)),
        colors: terminal_palette(),
    };
    let pty_for_resize = pty.clone();
    let tap = RemotePaneTap {
        writer: writer.clone(),
        tee: Arc::new(Mutex::new(TeeBuffer::default())),
    };
    let view = cx.new(|cx| {
        TerminalView::new(
            Box::new(SharedPtyWriter(writer)),
            Box::new(TeeReader { inner: reader, tee: tap.tee.clone() }),
            config,
            cx,
        )
            .with_resize_callback(move |cols, rows| {
                let _ = pty_for_resize.lock().unwrap().resize(PtySize {
                    cols: cols as u16,
                    rows: rows as u16,
                    pixel_width: 0,
                    pixel_height: 0,
                });
            })
            .with_clipboard_store_callback(|_, cx, text| {
                // OSC 52 (tmux/vim copy): into the app clipboard.
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.to_string()));
            })
            // OSC 52 READ (tmux/vim paste) seeds from the persisted gate —
            // off unless the settings panel turned it on (fail-closed).
            .with_clipboard_read_enabled(crate::ui_prefs::terminal_osc52_read())
            .with_bell_callback({
                let wb = wb.clone();
                move |_, cx| {
                    // The terminal bell (req 2.9): no sound (the OS owns
                    // that surface); a throttled notice says which pane
                    // rang — the workbench drops repeats inside the window.
                    let wb = wb.clone();
                    cx.spawn(async move |_, cx| {
                        let _ = wb.update(cx, |wb, cx| wb.terminal_bell(cx));
                    })
                    .detach();
                }
            })
            .with_exit_callback(move |_, cx| {
                // The shell exited (`exit`, EOF, kill): close the panel on
                // the NEXT frame — dropping the dock here would drop the
                // view whose render is mid-borrow. The view's entity id
                // rides along: the previous dock's late exit (restart
                // killed its shell while the old view still sat in the
                // last drawn frame) must not close the fresh dock.
                let wb = wb.clone();
                let exited = cx.entity().entity_id();
                cx.spawn(async move |_, cx| {
                    let _ = wb.update(cx, |wb, cx| wb.close_terminal(exited, cx));
                })
                .detach();
            })
    });
    let focus = view.read(cx).focus_handle().clone();
    Ok((
        TerminalDock {
            view,
            focus,
            tap: tap.clone(),
            _pty: Some(pty),
            child: Mutex::new(Some(child)),
        },
        tap,
    ))
}

/// A pane backed by in-memory streams instead of a real shell (tests only).
/// Key-routing tests want the view's focus handle and keystroke handling,
/// not a process: a spawned PTY drags a reader thread plus the dock's
/// detached kill-on-drop thread into the gpui test scheduler, which then
/// fails the run at random with "activity on another thread".
#[cfg(test)]
pub(crate) fn spawn_memory_dock(cx: &mut Context<Workbench>) -> TerminalDock {
    let theme = cx.theme().clone();
    let config = TerminalConfig {
        cols: 80,
        rows: 24,
        font_family: theme.mono_font_family.to_string(),
        font_size: ui_text(13.),
        scrollback: 10_000,
        line_height_multiplier: 1.0,
        padding: gpui::Edges::all(px(6.)),
        colors: terminal_palette(),
    };
    let view = cx.new(|cx| {
        TerminalView::new(
            Box::new(std::io::sink()) as Box<dyn std::io::Write + Send>,
            Box::new(std::io::empty()) as Box<dyn std::io::Read + Send>,
            config,
            cx,
        )
        .with_clipboard_read_enabled(crate::ui_prefs::terminal_osc52_read())
    });
    let focus = view.read(cx).focus_handle().clone();
    TerminalDock {
        view,
        focus,
        tap: RemotePaneTap::in_memory_for_test(),
        _pty: None,
        child: Mutex::new(None),
    }
}

/// The ANSI palette: a fixed dark scheme (upstream demo values). Terminal
/// panes stay dark on both theme modes — the common IDE default; a
/// theme-derived palette is a follow-up (needs an Hsla→rgb conversion).
fn terminal_palette() -> ColorPalette {
    ColorPalette::builder()
        .background(0x16, 0x16, 0x17)
        .foreground(0xC9, 0xC7, 0xCD)
        .cursor(0xC9, 0xC7, 0xCD)
        .black(0x10, 0x10, 0x10)
        .red(0xEF, 0xA6, 0xA2)
        .green(0x80, 0xC9, 0x90)
        .yellow(0xA6, 0x94, 0x60)
        .blue(0xA3, 0xB8, 0xEF)
        .magenta(0xE6, 0xA3, 0xDC)
        .cyan(0x50, 0xCA, 0xCD)
        .white(0x80, 0x80, 0x80)
        .bright_black(0x39, 0x41, 0x4E)
        .bright_red(0xE0, 0xAF, 0x85)
        .bright_green(0x5A, 0xCC, 0xAF)
        .bright_yellow(0xC8, 0xC8, 0x74)
        .bright_blue(0xCC, 0xAC, 0xED)
        .bright_magenta(0xF2, 0xA1, 0xC2)
        .bright_cyan(0x74, 0xC3, 0xE4)
        .bright_white(0xC0, 0xC0, 0xC0)
        .build()
}

/// How many side-by-side panes one tab's terminal dock may hold. The split
/// button disables at the cap; three ~300px panes are the practical floor
/// for a usable shell on a laptop window.
pub(crate) const MAX_TERMINAL_PANES: usize = 3;

/// The panel: a header strip (title + workspace leaf, split, close) over
/// the pane row. One pane renders bare (today's single dock); two or more
/// lay out in a horizontal resizable keyed by the tab id (each tab's split
/// sizes are its own). No pane at all paints the quiet stand-in (tests
/// before the first split; a failed spawn reads the same until retried).
pub(crate) fn render_terminal_panel(
    panes: Option<&[TerminalDock]>,
    tab: Option<u64>,
    workspace_leaf: Option<&str>,
    cx: &Context<Workbench>,
) -> AnyElement {
    let theme = cx.theme();
    let wb = cx.entity();
    let title = match workspace_leaf {
        Some(leaf) => format!("{} · {}", t!("terminal.title"), leaf),
        None => t!("terminal.title").into_owned(),
    };
    let at_cap = panes
        .map(|panes| panes.len() >= MAX_TERMINAL_PANES)
        .unwrap_or(false);
    div()
        .id("terminal-panel")
        .test_support()
        .flex()
        .flex_col()
        // flex_1: the resizable panel's root is a ROW-direction flex box —
        // a child without a grow/width of its own shrinks to its min-content
        // (the header's ~153px) instead of spanning the column. The chat
        // stack above the split grows the same way.
        .flex_1()
        .min_h_0()
        .min_w_0()
        .border_t_1()
        .border_color(theme.border)
        .bg(theme.title_bar)
        .child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .h_9()
                .px_2()
                .flex_none()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    Icon::new(KitIconName::SquareTerminal)
                        .size_4()
                        .flex_none()
                        .text_color(theme.muted_foreground),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_size(ui_text(13.))
                        .text_color(theme.foreground)
                        .child(title),
                )
                .child(
                    Button::new("terminal-split-add")
                        .ghost()
                        .xsmall()
                        .icon(KitIconName::Columns2)
                        .tooltip(t!("terminal.split").into_owned())
                        .disabled(at_cap)
                        .on_click({
                            let wb = wb.clone();
                            move |_, window, cx| {
                                wb.update(cx, |wb, cx| wb.split_terminal(window, cx));
                            }
                        }),
                )
                .child(
                    Button::new("terminal-close")
                        .ghost()
                        .xsmall()
                        .icon(KitIconName::X)
                        .tooltip(t!("terminal.hide").into_owned())
                        .on_click({
                            let wb = wb.clone();
                            move |_, window, cx| {
                                wb.update(cx, |wb, cx| wb.toggle_terminal(window, cx));
                            }
                        }),
                ),
        )
        .child(match panes {
            Some([one]) => div()
                .flex_1()
                .min_h_0()
                .child(one.view.clone())
                .into_any_element(),
            Some(panes) if panes.len() >= 2 => div()
                .flex_1()
                .min_h_0()
                .child(
                    h_resizable(("terminal-panes", tab.unwrap_or(0))).children(
                        panes.iter().enumerate().map(|(ix, pane)| {
                            resizable_panel()
                                .size(px(400.))
                                .size_range(px(160.)..px(6000.))
                                .child(render_pane(pane, ix, &wb, theme))
                        }),
                    ),
                )
                .into_any_element(),
            _ => div()
                .id("terminal-placeholder")
                .test_support()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .text_size(ui_text(12.))
                .text_color(theme.muted_foreground)
                .child(t!("terminal.testPlaceholder").into_owned())
                .into_any_element(),
        })
        .into_any_element()
}

/// One pane in the split row: a slim strip (index + restart + close) over
/// the view. The strip exists only in the multi-pane layout — a lone dock
/// stays chromeless like before.
fn render_pane(
    pane: &TerminalDock,
    ix: usize,
    wb: &Entity<Workbench>,
    theme: &gpui_kit::component::Theme,
) -> impl IntoElement {
    let entity = pane.view.entity_id();
    div()
        .id(("terminal-pane", ix))
        .flex()
        .flex_col()
        // flex_1: the resizable panel root is a ROW flex box — without a
        // grow the pane collapses to its min-content width.
        .flex_1()
        .min_h_0()
        .min_w_0()
        .child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .h_6()
                .px_2()
                .flex_none()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_size(ui_text(12.))
                        .text_color(theme.muted_foreground)
                        .child(format!("{} {}", t!("terminal.title"), ix + 1)),
                )
                .child(
                    Button::new(("terminal-pane-restart", ix))
                        .ghost()
                        .xsmall()
                        .icon(KitIconName::RotateCcw)
                        .tooltip(t!("terminal.restart").into_owned())
                        .on_click({
                            let wb = wb.clone();
                            move |_, _, cx| {
                                wb.update(cx, |wb, cx| wb.restart_terminal(entity, cx));
                            }
                        }),
                )
                .child(
                    Button::new(("terminal-pane-close", ix))
                        .ghost()
                        .xsmall()
                        .icon(KitIconName::X)
                        .tooltip(t!("terminal.closePane").into_owned())
                        .on_click({
                            let wb = wb.clone();
                            move |_, _, cx| {
                                wb.update(cx, |wb, cx| wb.close_terminal(entity, cx));
                            }
                        }),
                ),
        )
        .child(div().flex_1().min_h_0().child(pane.view.clone()))
}

#[cfg(test)]
mod remote_tap_tests {
    // Explicit imports only — the file-root kit glob shadows #[test] with
    // gpui's macro (see workbench.rs's test modules / pending_store.rs).
    use super::{BusyEvent, RemotePaneTap, TeeReader, TEE_CAP};
    use std::io::Read;

    /// Feed bytes through a tap the way the view's reader thread would.
    fn feed(tap: &RemotePaneTap, bytes: &[u8]) {
        tap.tee.lock().unwrap().append(bytes);
    }

    fn marker(code: i32) -> Vec<u8> {
        format!("\x1b]641;{code}\x07").into_bytes()
    }

    #[test]
    fn marker_fires_exit_and_cuts_the_capture() {
        let (tap, _) = RemotePaneTap::in_memory();
        tap.arm();
        feed(&tap, b"$ npm test\n");
        feed(&tap, b"ok\n");
        feed(&tap, &marker(0));
        feed(&tap, b"$ "); // prompt redraw after the marker: ignored
        assert_eq!(tap.exit_code(), Some(0));
        assert_eq!(tap.take_output(), b"$ npm test\nok\n".to_vec());
        // Drained: a second take is empty, the code cleared.
        assert_eq!(tap.take_output(), Vec::<u8>::new());
        assert_eq!(tap.exit_code(), None);
    }

    #[test]
    fn marker_split_across_reads_is_still_found() {
        let (tap, _) = RemotePaneTap::in_memory();
        tap.arm();
        feed(&tap, b"out\x1b]64");
        feed(&tap, b"1;13");
        feed(&tap, "\x07".as_bytes());
        assert_eq!(tap.exit_code(), Some(13));
        assert_eq!(tap.take_output(), b"out".to_vec());
    }

    #[test]
    fn st_terminated_marker_and_multi_digit_codes() {
        let (tap, _) = RemotePaneTap::in_memory();
        tap.arm();
        feed(&tap, b"x\x1b]641;130\x1b\\y");
        assert_eq!(tap.exit_code(), Some(130));
        // The capture ends BEFORE the marker; `y` arrived disarmed.
        assert_eq!(tap.take_output(), b"x".to_vec());
    }

    #[test]
    fn echo_of_the_injection_line_is_not_a_marker() {
        // The typed line's echo carries the LITERAL text `printf '\e]641;...'`
        // (backslash-e), never the byte ESC — no false settle.
        let (tap, _) = RemotePaneTap::in_memory();
        tap.arm();
        feed(&tap, b"$ echo hi; printf '\\e]641;%s\\a' \"$?\"\r\nhi\r\n");
        assert_eq!(tap.exit_code(), None);
    }

    #[test]
    fn prefix_without_digits_is_ignored() {
        let (tap, _) = RemotePaneTap::in_memory();
        tap.arm();
        feed(&tap, b"\x1b]641;abc\x07");
        assert_eq!(tap.exit_code(), None);
    }

    #[test]
    fn unarmed_tee_captures_nothing() {
        let (tap, _) = RemotePaneTap::in_memory();
        feed(&tap, b"noise");
        assert_eq!(tap.take_output(), Vec::<u8>::new());
        tap.arm();
        feed(&tap, &marker(2));
        assert_eq!(tap.exit_code(), Some(2));
    }

    // ---- Terminal Mirror busy markers (2026-11-20 addition) ----

    #[test]
    fn busy_markers_scan_armed_or_not_and_split_across_reads() {
        let (tap, _) = RemotePaneTap::in_memory();
        feed(&tap, b"\x1b]642;npm test\x07");
        feed(&tap, b"\x1b]643;"); // terminator arrives in a later read
        feed(&tap, b"0\x07");
        assert_eq!(
            tap.take_busy_events(),
            vec![
                BusyEvent::CommandStart("npm test".to_string()),
                BusyEvent::PromptReturn(0),
            ]
        );
        // Drained.
        assert!(tap.take_busy_events().is_empty());
    }

    #[test]
    fn busy_marker_st_terminator_and_flood_bounded_window() {
        let (tap, _) = RemotePaneTap::in_memory();
        feed(&tap, b"\x1b]642;hello\x1b\\");
        assert_eq!(
            tap.take_busy_events(),
            vec![BusyEvent::CommandStart("hello".to_string())]
        );
        // A marker-less flood keeps the scan window bounded; a later marker
        // still parses.
        feed(&tap, &vec![b'x'; 4 * 1024]);
        feed(&tap, b"\x1b]643;130\x07");
        assert_eq!(
            tap.take_busy_events(),
            vec![BusyEvent::PromptReturn(130)]
        );
    }

    #[test]
    fn busy_markers_ignore_641_and_unterminated_or_malformed_payloads() {
        let (tap, _) = RemotePaneTap::in_memory();
        feed(&tap, b"\x1b]641;0\x07"); // v1's exit marker — not ours
        feed(&tap, b"\x1b]642;never-terminated");
        feed(&tap, b"\x1b]643;x\x07"); // non-digit payload
        feed(&tap, b"\x1b]642;\x07"); // empty command: valid, empty text
        assert_eq!(
            tap.take_busy_events(),
            vec![BusyEvent::CommandStart(String::new())]
        );
    }

    #[test]
    fn long_output_keeps_only_the_tail_window() {
        let (tap, _) = RemotePaneTap::in_memory();
        tap.arm();
        let chunk = vec![b'x'; 8 * 1024];
        for _ in 0..40 {
            feed(&tap, &chunk); // 320 KiB total > TEE_CAP
        }
        feed(&tap, &marker(0));
        let out = tap.take_output();
        // The marker's own bytes were appended then truncated from the tee
        // copy, so the window holds TEE_CAP minus the marker's 8 bytes.
        assert_eq!(out.len(), TEE_CAP - 8);
        assert!(out.iter().all(|b| *b == b'x'));
    }

    #[test]
    fn write_line_carries_the_carriage_return() {
        let (tap, sink) = RemotePaneTap::in_memory();
        tap.write_line("echo hi");
        assert_eq!(*sink.lock().unwrap(), b"echo hi\r".to_vec());
        tap.write_raw(&[0x03]);
        assert_eq!(sink.lock().unwrap().as_slice(), b"echo hi\r\x03");
    }

    /// The reader marks the tap closed on EOF and on read error — the settle
    /// tick's only signal that a hidden dock's shell died.
    #[test]
    fn tee_reader_marks_the_tap_closed_on_eof_and_error() {
        // EOF: a reader whose stream ends (shell exited).
        let (tap, _) = RemotePaneTap::in_memory();
        tap.arm();
        let mut eof = TeeReader {
            inner: Box::new(std::io::Cursor::new(b"last words".to_vec())),
            tee: tap.tee.clone(),
        };
        let mut buf = Vec::new();
        eof.read_to_end(&mut buf).unwrap();
        assert_eq!(tap.take_output(), b"last words".to_vec());
        assert!(tap.is_closed());

        // Error: EIO once the slave side is fully gone (Linux PTY).
        let (tap, _) = RemotePaneTap::in_memory();
        struct Broken;
        impl std::io::Read for Broken {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from(std::io::ErrorKind::Other))
            }
        }
        let mut err = TeeReader {
            inner: Box::new(Broken),
            tee: tap.tee.clone(),
        };
        assert!(err.read(&mut buf).is_err());
        assert!(tap.is_closed());
    }
}
