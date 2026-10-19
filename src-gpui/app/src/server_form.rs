//! The remote-server form of the config dock's server book (TS
//! ConnectScreen's add/edit form): one kit dialog serves both Add and Edit,
//! so the servers section itself stays a plain list.
//!
//! The form is its own view: the three inputs notify it on every keystroke,
//! which keeps the submit button's enabled state live. The earlier inline
//! form had no such subscription — its Save sat grey until some unrelated
//! repaint happened to pick the typed values up, which read as "cannot edit
//! a saved hub".

use std::cell::Cell;

use gpui::{div, px, App, Context, Entity, IntoElement, Render, Subscription, Window};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _};
use gpui_kit::*;
use protocol::hub::SavedServer;

use crate::runtime::{ProtocolRuntime, UiCommand};

pub struct ServerForm {
    runtime: ProtocolRuntime,
    /// `Some(id)` saves that server in place (`UpdateServer`); `None` adds a
    /// new one and connects to it (`AddServer`).
    editing: Option<String>,
    name: Entity<InputState>,
    hub_url: Entity<InputState>,
    token: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl ServerForm {
    /// Opens the dialog over `window`; `prefill` is the server being edited.
    pub fn open(
        prefill: Option<&SavedServer>,
        runtime: ProtocolRuntime,
        window: &mut Window,
        cx: &mut App,
    ) {
        let title = if prefill.is_some() {
            "Edit remote server"
        } else {
            "Add remote server"
        };
        let form = cx.new(|cx| Self::new(prefill, runtime, window, cx));
        // The builder reruns whenever the dialog layer rebuilds; the name
        // field takes focus on the first run only, so later rebuilds never
        // steal it from the field being typed in.
        let initial_focus = Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if initial_focus.replace(false) {
                let focus = form.read(cx).name.read(cx).focus_handle(cx);
                window.focus(&focus, cx);
            }
            dialog.title(title).child(form.clone())
        });
    }

    fn new(
        prefill: Option<&SavedServer>,
        runtime: ProtocolRuntime,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Name (e.g. dev box)"));
        let hub_url =
            cx.new(|cx| InputState::new(window, cx).placeholder("https://hub.example:8377"));
        let token = cx.new(|cx| InputState::new(window, cx).placeholder("Bearer token"));
        if let Some(server) = prefill {
            name.update(cx, |s, cx| s.set_value(server.name.clone(), window, cx));
            hub_url.update(cx, |s, cx| s.set_value(server.hub_url.clone(), window, cx));
            token.update(cx, |s, cx| s.set_value(server.token.clone(), window, cx));
        }
        let subscriptions = [&name, &hub_url, &token]
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
            editing: prefill.map(|s| s.id.clone()),
            name,
            hub_url,
            token,
            _subscriptions: subscriptions,
        }
    }

    /// TS ConnectScreen:147 — all three fields are required, the token too.
    fn can_submit(&self, cx: &App) -> bool {
        [&self.name, &self.hub_url, &self.token]
            .iter()
            .all(|state| !state.read(cx).value().trim().is_empty())
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_submit(cx) {
            return;
        }
        let name = self.name.read(cx).value().trim().to_string();
        let hub_url = self.hub_url.read(cx).value().trim().to_string();
        let token = self.token.read(cx).value().trim().to_string();
        self.runtime.send(match self.editing.clone() {
            Some(id) => UiCommand::UpdateServer {
                id,
                name,
                hub_url,
                token,
            },
            None => UiCommand::AddServer {
                name,
                hub_url,
                token,
            },
        });
        window.close_dialog(cx);
    }

    fn field(&self, label: &'static str, input: &Entity<InputState>, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child(label),
            )
            .child(Input::new(input).small())
    }
}

impl Render for ServerForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let can_submit = self.can_submit(cx);
        div()
            .id("server-form")
            .test_support()
            .flex()
            .flex_col()
            .gap_3()
            .child(self.field("Name", &self.name, cx))
            .child(self.field("Hub URL", &self.hub_url, cx))
            .child(self.field("Token", &self.token, cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .pt_1()
                    .child(
                        Button::new("server-form-cancel")
                            .secondary()
                            .small()
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("server-form-save")
                            .primary()
                            .small()
                            .label(if self.editing.is_some() {
                                "Save"
                            } else {
                                "Add and connect"
                            })
                            .disabled(!can_submit)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}
