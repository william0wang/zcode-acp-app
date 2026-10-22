//! Desktop Workbench entry point (requirements §6): one window with the
//! connection status bar, the left dock (the sessions panel), the
//! one-connection-per-tab session tabs, the composer, the right panel (the
//! active session's file browser), and the left overlay drawer (settings /
//! the all-providers quota display — non-modal, floats above the layout).
//! The protocol driver thread (`runtime`) owns every wire concern; this
//! layer only paints.
//!
//! Quit wiring: ⌘Q is bound to the workbench's `QuitApp` action and carried
//! by a minimal native app menu; the window close rides
//! `on_window_should_close` (registered in `Workbench::new`). Both routes
//! run the quit guard — with turns this app started still running, the quit
//! dialog asks first (Quit Only, the default, or Stop Turns & Quit;
//! requirements §4); anything else closes at once. Closing the last window
//! quits the app (`QuitMode::LastWindowClosed`), so the confirmed close and
//! a confirmed ⌘Q end identically — and neither stops the hub or other
//! clients' turns.

// `#[gpui_kit::test]` (gpui::test) nests deeply enough to need this.
#![recursion_limit = "256"]

mod approval_card;
mod diff_view;
mod file_browser;
mod http_transport;
mod i18n;
mod local_time;
mod models_page;
mod pending_store;
mod quota_card;
mod quota_page;
mod readout_store;
mod runtime;
mod server_form;
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
        // emerald-700 — the third step down (user feedback, three rounds:
        // 400 → 500 → 600 still read as a glare on every selected state and
        // filled control that rides the accent; 700 is the deepest step that
        // still reads unmistakably green against the near-black canvas).
        theme.colors.accent = rgb(0x047857).into();
        // Amber-300/500: the quota page's own amber pair, made theme-wide so
        // the awaiting ring and restart notes match it (kit dark ships
        // yellow-600 as warning foreground — muddy as text on this canvas).
        theme.colors.warning_foreground = rgb(0xfcd34d).into();
        theme.colors.warning = rgb(0xf59e0b).into();
    });
}

fn main() {
    // Full Lucide catalog: the composer's attachment controls (ImagePlus)
    // live outside the default icon subset.
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        // Closing the last window quits (macOS defaults to Explicit — a
        // closed-window app with a live menu bar is not a workbench shape).
        .with_quit_mode(QuitMode::LastWindowClosed)
        .run(|cx| {
            gpui_kit::init(cx);
            // Locale: the persisted settings-panel pick (batch H3) wins;
            // with no stored preference the system decides (the shared
            // rust-i18n global also drives gpui-kit's own strings).
            rust_i18n::set_locale(ui_prefs::load_locale().unwrap_or_else(i18n::detect_locale));
            // Font scale: the persisted pick seeds the paint-time chrome
            // factor (ui_prefs::ui_text); the settings panel's pick
            // republishes it live.
            ui_prefs::set_current_ui_scale(
                ui_prefs::load_font_size().unwrap_or(ui_prefs::FontScale::Medium),
            );
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
            cx.set_menus(vec![Menu::new("ZCode ACP").items([MenuItem::action(
                "Quit ZCode ACP",
                QuitApp,
            )])]);

            let (runtime, mut event_rx) = ProtocolRuntime::start();

            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1080.), px(760.)),
                    cx,
                ))),
                titlebar: Some(TitlebarOptions {
                    title: Some("ZCode ACP".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let (window_handle, workbench) =
                gpui_kit::open_window(options, cx, |window, cx| {
                    let runtime = runtime.clone();
                    cx.new(|cx| {
                        Workbench::new(window, cx, runtime, runtime::WorkbenchSnapshot::initial())
                    })
                })
                .expect("open the workbench window");

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
            // the workbench entity on the main thread.
            cx.spawn(async move |cx| {
                while let Some(event) = event_rx.recv().await {
                    let _ = cx.update_entity(&workbench, |wb, cx| {
                        wb.on_runtime_event(event, cx)
                    });
                }
            })
            .detach();
        });
}
