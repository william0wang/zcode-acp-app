//! The local-hub edit form (the servers section's 本机 hub row): edits the
//! dial parameters Hub Bootstrap reads from `~/.config/zcode-acp/config.json`
//! (`remote.hubHost/hubPort/token`). The implicit local entry has no book
//! record, so unlike `ServerForm` there is no name and no URL field — the
//! host and port are the config's own fields, written back with everything
//! else preserved (`plan_remote_edit`). Saving restarts the dial when local
//! is the active target (the driver's `UpdateLocalHub` rule).
//!
//! Same view/subscription shape as `ServerForm`: the three inputs notify on
//! every keystroke so the submit button's enabled state stays live.

use gpui::{div, App, Context, Entity, IntoElement, Render, Subscription, Window};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _};
use gpui_kit::*;
use protocol::bootstrap::LocalHubConfig;
use rust_i18n::t;

use crate::runtime::{ProtocolRuntime, UiCommand};
use crate::ui_prefs::ui_text;

pub struct LocalHubForm {
    runtime: ProtocolRuntime,
    host: Entity<InputState>,
    port: Entity<InputState>,
    token: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl LocalHubForm {
    /// Opens the dialog over `window`; `prefill` is the local hub's current
    /// config (empty fields when the app has not dialed local this run).
    pub fn open(
        prefill: Option<&LocalHubConfig>,
        runtime: ProtocolRuntime,
        window: &mut Window,
        cx: &mut App,
    ) {
        let title = t!("connect.editLocalHub").into_owned();
        let form = cx.new(|cx| Self::new(prefill, runtime, window, cx));
        // First run only (ServerForm's rule): later dialog-layer rebuilds
        // must not steal focus from the field being typed in.
        let initial_focus = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if initial_focus.replace(false) {
                let focus = form.read(cx).host.read(cx).focus_handle(cx);
                window.focus(&focus, cx);
            }
            dialog.title(title.clone()).child(form.clone())
        });
    }

    fn new(
        prefill: Option<&LocalHubConfig>,
        runtime: ProtocolRuntime,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let host =
            cx.new(|cx| InputState::new(window, cx).placeholder("127.0.0.1"));
        let port = cx.new(|cx| InputState::new(window, cx).placeholder("8377"));
        let token = cx.new(|cx| InputState::new(window, cx));
        if let Some(config) = prefill {
            host.update(cx, |s, cx| s.set_value(config.hub_host.clone(), window, cx));
            port.update(cx, |s, cx| {
                s.set_value(config.hub_port.to_string(), window, cx)
            });
            if let Some(value) = &config.token {
                token.update(cx, |s, cx| s.set_value(value.clone(), window, cx));
            }
        }
        let subscriptions = [&host, &port, &token]
            .into_iter()
            .map(|state| {
                cx.subscribe_in(state, window, |this, _, event: &InputEvent, window, cx| {
                    match event {
                        InputEvent::PressEnter {
                            secondary: false,
                            shift: false,
                        } => this.submit(window, cx),
                        _ => cx.notify(),
                    }
                })
            })
            .collect();
        Self {
            runtime,
            host,
            port,
            token,
            _subscriptions: subscriptions,
        }
    }

    /// All three fields required; the port must parse into 1..=65535 (the
    /// config's own validation — `valid_port`).
    fn can_submit(&self, cx: &App) -> bool {
        let host = self.host.read(cx).value().trim().to_owned();
        let port = self.port.read(cx).value().trim().to_owned();
        let token = self.token.read(cx).value().trim().to_owned();
        !host.is_empty() && matches!(port.parse::<u16>(), Ok(p) if p > 0) && !token.is_empty()
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_submit(cx) {
            return;
        }
        let host = self.host.read(cx).value().trim().to_string();
        let port = self.port.read(cx).value().trim().parse::<u16>().unwrap();
        let token = self.token.read(cx).value().trim().to_string();
        self.runtime
            .send(UiCommand::UpdateLocalHub { host, port, token });
        window.close_dialog(cx);
    }

    fn field(&self, label: &str, input: &Entity<InputState>, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_size(ui_text(12.))
                    .text_color(theme.muted_foreground)
                    .child(label.to_string()),
            )
            .child(Input::new(input).small())
    }
}

impl Render for LocalHubForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let can_submit = self.can_submit(cx);
        div()
            .id("local-hub-form")
            .test_support()
            .flex()
            .flex_col()
            .gap_3()
            .child(self.field(&t!("connect.hubHost"), &self.host, cx))
            .child(self.field(&t!("connect.hubPort"), &self.port, cx))
            .child(self.field(&t!("connect.token"), &self.token, cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .pt_1()
                    .child(
                        Button::new("local-hub-form-cancel")
                            .secondary()
                            .small()
                            .label(t!("common.cancel").into_owned())
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("local-hub-form-save")
                            .primary()
                            .small()
                            .label(t!("connect.saveServer").into_owned())
                            .disabled(!can_submit)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}
