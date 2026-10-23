//! The embedded terminal dock: the vendored `gpui-terminal` view in a
//! bottom panel under the chat column (workbench `render_chat_column`
//! splits against it with `v_resizable`, the left dock's horizontal
//! pattern turned vertical).
//!
//! This module owns everything shell-side — the portable-pty PTY pair, the
//! login shell spawn (cwd = the active session's workspace), the resize
//! plumbing, and the OSC 52 clipboard store — while `workbench.rs` only
//! toggles, renders, and yields Esc. The terminal is view-local layout
//! state, never wire state: hiding keeps the shell, while a shell exit or
//! the restart button drops the dock (drop → child kill) and the next open
//! spawns fresh in the current session's directory.

use std::sync::{Arc, Mutex};

use gpui::{div, px, AnyElement, Context, Entity, FocusHandle, WeakEntity};
// The kit's root glob carries the `gpui` alias (`pub use crate as gpui`) —
// the app's one-gpui-identity trick every sibling module shares.
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _};
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
    /// The dock's own handle on the PTY master (the resize callback holds
    /// the other Arc clone) — kept so the struct reads as the owner.
    _pty: Arc<Mutex<Box<dyn MasterPty + Send>>>,
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

/// Spawn the login shell in a fresh PTY at `workspace` (None = home) and
/// wire the view's callbacks into the workbench. `wb` is weak: the exit
/// callback closes the panel from the next frame, past the view's own
/// render borrow.
pub(crate) fn spawn_dock(
    workspace: Option<&str>,
    wb: WeakEntity<Workbench>,
    cx: &mut Context<Workbench>,
) -> Result<TerminalDock, String> {
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
    let child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
    let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
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
    let view = cx.new(|cx| {
        TerminalView::new(writer, reader, config, cx)
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
    Ok(TerminalDock {
        view,
        focus,
        _pty: pty,
        child: Mutex::new(Some(child)),
    })
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

/// The panel: a header strip (title + workspace leaf, restart, close) over
/// the terminal view — the drawer frame's shape at dock scale. `None` dock
/// paints the quiet stand-in (tests never spawn a real shell; a failed
/// spawn reads the same until retried).
pub(crate) fn render_terminal_panel(
    dock: Option<&TerminalDock>,
    workspace_leaf: Option<&str>,
    cx: &Context<Workbench>,
) -> AnyElement {
    let theme = cx.theme();
    let wb = cx.entity();
    let title = match workspace_leaf {
        Some(leaf) => format!("{} · {}", t!("terminal.title"), leaf),
        None => t!("terminal.title").into_owned(),
    };
    div()
        .id("terminal-panel")
        .test_support()
        .flex()
        .flex_col()
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
                    Button::new("terminal-restart")
                        .ghost()
                        .xsmall()
                        .icon(KitIconName::RotateCcw)
                        .tooltip(t!("terminal.restart").into_owned())
                        .on_click({
                            let wb = wb.clone();
                            move |_, _, cx| {
                                wb.update(cx, |wb, cx| wb.restart_terminal(cx));
                            }
                        }),
                )
                .child(
                    Button::new("terminal-close")
                        .ghost()
                        .xsmall()
                        .icon(KitIconName::X)
                        .tooltip(t!("terminal.hide").into_owned())
                        .on_click(move |_, _, cx| {
                            wb.update(cx, |wb, cx| wb.toggle_terminal(cx));
                        }),
                ),
        )
        .child(match dock {
            Some(dock) => div()
                .flex_1()
                .min_h_0()
                .child(dock.view.clone())
                .into_any_element(),
            None => div()
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
