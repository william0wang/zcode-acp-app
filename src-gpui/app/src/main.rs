//! Desktop Workbench entry point (requirements §6): one window with the
//! connection status bar, the left dock (sessions / configuration panels),
//! the one-connection-per-tab session tabs, and the composer. The protocol
//! driver thread (`runtime`) owns every wire concern; this layer only paints.
//!
//! Quit wiring: ⌘Q is bound to the workbench's `QuitApp` action and carried
//! by a minimal native app menu; the window close rides
//! `on_window_should_close` (registered in `Workbench::new`). Both routes
//! run the quit guard — with any session running, a native confirm dialog
//! asks first (requirements §4). Closing the last window quits the app
//! (`QuitMode::LastWindowClosed`), so the confirmed close and a confirmed
//! ⌘Q end identically — and neither stops the hub or the sessions.

// `#[gpui_kit::test]` (gpui::test) nests deeply enough to need this.
#![recursion_limit = "256"]

mod approval_card;
#[cfg(test)]
mod acceptance;
mod file_browser;
mod golden_fixtures;
#[cfg(target_os = "macos")]
mod golden_capture;
mod http_transport;
mod i18n;
mod models_page;
mod pending_store;
mod quota_page;
mod runtime;
mod system_status;
mod ui_prefs;
mod usage_page;
mod workbench;
mod workflow_launches;
mod workflows_page;
mod zconfig_pages;

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{
    px, size, App, AppContext as _, Bounds, Hsla, KeyBinding, Menu, MenuItem, QuitMode, TitlebarOptions,
    WindowBounds, WindowOptions, rgb,
};
use runtime::ProtocolRuntime;
use workbench::{
    FocusSessionSearch, QuitApp, SelectTab1, SelectTab2, SelectTab3, SelectTab4, SelectTab5,
    SelectTab6, SelectTab7, SelectTab8, SelectTab9, StopTurn, Workbench,
};

// The shared catalogs (requirements §2): the mobile/web i18next JSONs are
// compiled in straight from src/i18n — dotted keys, `en` fallback — with no
// forked copy (module doc in i18n.rs for the `{{name}}` seam).
rust_i18n::i18n!("../../src/i18n", fallback = "en");

/// The dark palette polish (requirements §6 M4): the kit's Default Dark is
/// shadcn zinc; the product's dark theme is the mobile/web "quiet dark"
/// (src/index.css) — stacked surfaces over one flat background, hairline
/// borders, ink/dim text. This maps that palette onto the theme fields the
/// workbench paints with (accent joins the TS app's emerald family: running
/// dots, approve actions), so the desktop reads as the same product. Light
/// mode stays the kit default — P0 is dark-only (requirements §2).
fn apply_quiet_dark(cx: &mut App) {
    let surface = rgb(0x161619).into(); // --color-surface
    let hairline: Hsla = rgb(0x2a2a2f).into(); // --color-hairline
    Theme::update(cx, |theme| {
        theme.colors.background = rgb(0x0d0d0f).into(); // --color-canvas
        theme.colors.title_bar = surface;
        theme.colors.status_bar = surface;
        theme.colors.sidebar = surface;
        theme.colors.popover = surface;
        theme.colors.secondary = rgb(0x1e1e22).into(); // --color-raised
        theme.colors.border = hairline;
        theme.colors.title_bar_border = hairline;
        theme.colors.status_bar_border = hairline;
        theme.colors.sidebar_border = hairline;
        theme.colors.foreground = rgb(0xececf1).into(); // --color-ink
        theme.colors.muted_foreground = rgb(0xa6a6b0).into(); // --color-dim
        theme.colors.accent = rgb(0x34d399).into(); // emerald-400 (TS running dots)
        // Amber-300/500: the quota page's own amber pair, made theme-wide so
        // the awaiting ring and restart notes match it (kit dark ships
        // yellow-600 as warning foreground — muddy as text on this canvas).
        theme.colors.warning_foreground = rgb(0xfcd34d).into();
        theme.colors.warning = rgb(0xf59e0b).into();
    });
}

fn main() {
    // Golden-fixture mode (the acceptance script's pixel stage): render one
    // canned snapshot in a PINNED window with an inert runtime — no driver,
    // no network — self-capture the window (TCC-exempt), write the PNG, and
    // exit. Args are checked before the app boots.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().position(|a| a == name);
    let golden_fixture = if args.is_empty() {
        None
    } else if let Some(i) = flag("--golden-fixture") {
        Some(
            args.get(i + 1)
                .cloned()
                .unwrap_or_else(|| {
                    eprintln!("--golden-fixture needs a name");
                    std::process::exit(2);
                })
                .to_string(),
        )
    } else {
        eprintln!("unknown arguments: {args:?}");
        std::process::exit(2);
    };
    let golden_out = flag("--golden-out").and_then(|i| args.get(i + 1).cloned());
    let golden = golden_fixture.as_deref().map(|name| {
        let Some(snap) = golden_fixtures::snapshot(name) else {
            eprintln!(
                "unknown golden fixture {name:?} — pick one of {:?}",
                golden_fixtures::NAMES
            );
            std::process::exit(2);
        };
        (name.to_string(), snap)
    });

    // Full Lucide catalog: the composer's attachment controls (ImagePlus)
    // live outside the default icon subset.
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        // Closing the last window quits (macOS defaults to Explicit — a
        // closed-window app with a live menu bar is not a workbench shape).
        .with_quit_mode(QuitMode::LastWindowClosed)
        .run(move |cx| {
            gpui_kit::init(cx);
            // Locale: the persisted settings-panel pick (batch H3) wins;
            // with no stored preference the system decides (the shared
            // rust-i18n global also drives gpui-kit's own strings). The
            // golden mode pins English — captures must not follow the
            // user's locale.
            rust_i18n::set_locale(match &golden {
                Some(_) => "en",
                None => &ui_prefs::load_locale().unwrap_or_else(i18n::detect_locale),
            });
            // Dark theme only in P0 (requirements §2), polished onto the
            // product's quiet-dark palette (see apply_quiet_dark).
            Theme::change(ThemeMode::Dark, None, cx);
            apply_quiet_dark(cx);

            // ⌘Q / Ctrl-Q dispatch the QuitApp action, which the workbench
            // handles through the quit guard. The menu item gives macOS its
            // conventional application-menu Quit (key equivalent derived
            // from the binding).
            cx.bind_keys([
                KeyBinding::new("cmd-q", QuitApp, None),
                KeyBinding::new("ctrl-q", QuitApp, None),
                // The basic desktop shortcuts (requirements §4): ⌘K focuses
                // the sessions panel's search field; ⌘1..9 select the tab
                // strip's nth tab; Esc stops the running turn (ChatView's
                // Cancel — the workbench also closes the new-session sheet
                // with it first). (Enter-send lives in the composer.)
                KeyBinding::new("cmd-k", FocusSessionSearch, None),
                KeyBinding::new("ctrl-k", FocusSessionSearch, None),
                KeyBinding::new("escape", StopTurn, None),
                KeyBinding::new("cmd-1", SelectTab1, None),
                KeyBinding::new("cmd-2", SelectTab2, None),
                KeyBinding::new("cmd-3", SelectTab3, None),
                KeyBinding::new("cmd-4", SelectTab4, None),
                KeyBinding::new("cmd-5", SelectTab5, None),
                KeyBinding::new("cmd-6", SelectTab6, None),
                KeyBinding::new("cmd-7", SelectTab7, None),
                KeyBinding::new("cmd-8", SelectTab8, None),
                KeyBinding::new("cmd-9", SelectTab9, None),
            ]);
            cx.set_menus(vec![Menu::new("zcode-acp-remote").items([MenuItem::action(
                "Quit zcode-acp-remote",
                QuitApp,
            )])]);

            let (runtime, event_rx) = match &golden {
                Some(_) => (ProtocolRuntime::inert(), None),
                None => {
                    let (runtime, event_rx) = ProtocolRuntime::start();
                    (runtime, Some(event_rx))
                }
            };

            let options = WindowOptions {
                window_bounds: Some(match &golden {
                    // Pinned frame: the capture script's screencapture
                    // region must match exactly (golden_fixtures'
                    // GOLDEN_WINDOW).
                    Some(_) => WindowBounds::Windowed(golden_fixtures::GOLDEN_WINDOW),
                    None => WindowBounds::Windowed(Bounds::centered(
                        None,
                        size(px(1080.), px(760.)),
                        cx,
                    )),
                }),
                titlebar: Some(TitlebarOptions {
                    title: Some("zcode-acp-remote".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let initial_snapshot = golden
                .as_ref()
                .map(|(_, snap)| snap.clone())
                .unwrap_or_else(runtime::WorkbenchSnapshot::initial);
            let (window_handle, workbench) =
                gpui_kit::open_window(options, cx, |window, cx| {
                    let runtime = runtime.clone();
                    cx.new(|cx| Workbench::new(window, cx, runtime, initial_snapshot))
                })
                .expect("open the workbench window");

            // The settings fixture paints the config dock open — drive the
            // same toggle the rail button does.
            if let Some((name, _)) = &golden {
                if golden_fixtures::wants_config_dock(name) {
                    workbench.update(cx, |wb, cx| {
                        wb.toggle_dock_panel(workbench::DockPanel::Config, cx)
                    });
                }
            }

            // The self-capture + exit (golden mode only): a beat for the
            // first frames to land, then the window PNG and a clean exit —
            // the acceptance script bounds the wait and falls back to
            // `screencapture` when the Metal surface refuses readback
            // (verified empty from both threads: a surface-property
            // limitation, not a timing one).
            if let Some(out) = golden_out {
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    #[cfg(target_os = "macos")]
                    match golden_capture::capture_own_window_png(std::path::Path::new(&out)) {
                        Ok(()) => std::process::exit(0),
                        Err(golden_capture::CaptureError::Empty) => {
                            eprintln!("golden capture: empty readback (Metal surface)");
                            std::process::exit(3);
                        }
                        Err(golden_capture::CaptureError::Other(e)) => {
                            eprintln!("golden capture failed: {e}");
                            std::process::exit(1);
                        }
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        let _ = out;
                        eprintln!("golden capture is macOS-only");
                        std::process::exit(1);
                    }
                });
            }

            // The quit guard's ⌘Q half: the action may fire while a dialog
            // layer holds focus, so listen globally rather than only on the
            // workbench's element tree (which also listens — harmless).
            let weak = workbench.downgrade();
            let quit_window = window_handle;
            cx.on_action::<QuitApp>(move |_, cx| {
                let weak = weak.clone();
                let _ = quit_window.update(cx, move |_, window, cx| {
                    let _ = weak.update(cx, |wb, cx| wb.request_quit(window, cx));
                });
            });

            // The runtime event pump: snapshots from the driver thread flow into
            // the workbench entity on the main thread. The golden mode has no
            // driver (inert runtime) — nothing to pump.
            if let Some(mut event_rx) = event_rx {
                cx.spawn(async move |cx| {
                    while let Some(event) = event_rx.recv().await {
                        let _ = cx.update_entity(&workbench, |wb, cx| {
                            wb.on_runtime_event(event, cx)
                        });
                    }
                })
                .detach();
            }
        });
}
