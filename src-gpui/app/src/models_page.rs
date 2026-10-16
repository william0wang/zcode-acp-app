//! The Models configuration page (M3, requirements §4: "High-frequency
//! configuration pages: Models … (Effect Class and Backend Restart
//! semantics included)") — the desktop port of
//! `src/screens/config/ModelsPage.tsx`, semantics identical:
//!
//! - the payload is an OBJECT (`available` + `modelRules`), the context
//!   window joined on `{providerId}/{modelId}` through
//!   `protocol::config_decode` — the module the TS rule says the screens
//!   must call, never fork;
//! - editing goes through POST /settings/models (an UPSERT: one route adds
//!   a model to a provider and rewrites its personal rule), so blank
//!   fields are OMITTED from the write ("keep" — the API cannot clear a
//!   context window or a levels list) and the levels are chips of the
//!   levels this machine's rules already declare;
//! - the write's Effect Class is surfaced, never assumed: `immediate` is
//!   done, a `needs-restart` answer arms the Backend Restart affordance
//!   (CONTEXT.md) — addressed per instance and never triggered
//!   automatically, because restarting cancels in-flight turns.
//!
//! Card strings come from the shared rust-i18n catalogs (`zconfig.*`,
//! `common.*`, `notice.*`; requirements §2).

use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, Context, Entity, SharedString, Window, div, px};
use gpui_kit::component;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::radio::RadioGroup;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _,
};
use gpui_kit::*;
use protocol::config_decode::{
    context_window_for, provider_options, rule_for, ModelRef, ProviderOption,
};
use protocol::settings::ModelUpsert;
use rust_i18n::t;

use crate::runtime::{ModelsPageView, UiCommand};
use crate::workbench::Workbench;

/// The config-screen notice (TS `notice`): a `notice.*` i18n key resolves
/// through the shared catalog, anything else is plain failure text shown
/// as-is (t! falls back to its input when the key misses).
fn notice_text(notice: &str) -> String {
    t!(notice).into_owned()
}

/// The add/edit form's state, owned by the workbench (the TS `ModelForm`
/// component's local state). One form at a time; rebuilt on open with the
/// rule's prefill (`rule_for` — the join that survives the two coding-plan
/// spellings, see config_decode).
pub struct ModelsForm {
    /// The model being edited; `None` = adding one that may not exist yet
    /// (TS `target`).
    target: Option<ModelRef>,
    /// Add-mode provider picker: an index into `providers`, where 0 is the
    /// "—" no-selection row (TS `<option value="">—</option>`).
    provider_index: usize,
    /// `provider_options` of the payload at open time — `account:*` left
    /// out (the write route refuses them outright).
    providers: Vec<ProviderOption>,
    /// Add mode's model-id input (edit mode shows the ids read-only).
    model_id: Entity<InputState>,
    context_window: Entity<InputState>,
    /// Selected reasoning levels (order = click order, like the TS array).
    levels: Vec<String>,
    /// Every level any rule declares plus this model's own — the chips.
    candidates: Vec<String>,
    enabled: bool,
}

impl Workbench {
    // ---- form state ----

    /// Opens the add (`target == None`) or edit form, prefilled from the
    /// rule (TS `useState(rule?.…)`): context window, levels, and
    /// `enabled !== false`.
    pub fn open_models_form(
        &mut self,
        target: Option<ModelRef>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let payload = self
            .snap
            .models_page
            .as_ref()
            .and_then(|page| page.models.as_ref());
        let providers = provider_options(payload);
        let rules = payload
            .and_then(|p| p.models.as_ref())
            .and_then(|b| b.model_rules.as_deref())
            .unwrap_or(&[]);
        let rule = target.as_ref().and_then(|t| rule_for(t, rules));

        // Every level any rule declares, plus the ones this model already
        // carries (TS candidates): a value the machine has never seen
        // elsewhere is still this model's own.
        let mut candidates: Vec<String> = Vec::new();
        let push = |level: &str, out: &mut Vec<String>| {
            if !out.iter().any(|l| l == level) {
                out.push(level.to_string());
            }
        };
        if let Some(levels) = rule.and_then(|r| r.reasoning_levels.as_deref()) {
            for level in levels {
                push(level, &mut candidates);
            }
        }
        for r in rules {
            if let Some(levels) = &r.reasoning_levels {
                for level in levels {
                    push(level, &mut candidates);
                }
            }
        }

        let model_id = cx.new(|cx| InputState::new(window, cx).placeholder("glm-4.6"));
        let context_window = cx.new(|cx| InputState::new(window, cx).placeholder("200000"));
        // Prefill the context window from the rule (add mode starts blank).
        if let Some(window_value) = rule.and_then(|r| r.context_window) {
            let text = window_value.to_string();
            context_window.update(cx, |state, cx| state.set_value(text, window, cx));
        }
        // Re-render per keystroke: canSubmit depends on the typed values.
        let sub_model = cx.subscribe(&model_id, |_, _, _: &InputEvent, cx| cx.notify());
        let sub_window =
            cx.subscribe(&context_window, |_, _, _: &InputEvent, cx| cx.notify());

        self.models_form = Some(ModelsForm {
            target,
            provider_index: 0,
            providers,
            model_id,
            context_window,
            levels: rule
                .and_then(|r| r.reasoning_levels.clone())
                .unwrap_or_default(),
            candidates,
            // TS `rule?.enabled !== false` — absent means on.
            enabled: rule.and_then(|r| r.enabled) != Some(false),
        });
        self.models_form_subs = vec![sub_model, sub_window];
        cx.notify();
    }

    /// Closes the form (the header back / a successful save).
    fn close_models_form(&mut self, cx: &mut Context<Self>) {
        self.models_form = None;
        self.models_form_subs.clear();
        cx.notify();
    }

    /// Toggles one reasoning-level chip (TS `setLevels(on ? filter :
    /// [...levels, l])`).
    fn toggle_models_level(&mut self, level: &str, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.models_form {
            if let Some(pos) = form.levels.iter().position(|l| l == level) {
                form.levels.remove(pos);
            } else {
                form.levels.push(level.to_string());
            }
        }
        cx.notify();
    }

    /// The effective provider/model ids of the form — the edit target's,
    /// or the picker + typed input in add mode.
    fn models_form_ids(&self, cx: &Context<Self>) -> (String, String) {
        let Some(form) = &self.models_form else {
            return (String::new(), String::new());
        };
        match &form.target {
            Some(t) => (
                t.provider_id.clone().unwrap_or_default(),
                t.model_id.clone().unwrap_or_default(),
            ),
            None => {
                // Index 0 is the "—" row; providers follow it.
                let provider = form
                    .providers
                    .get(form.provider_index.saturating_sub(1))
                    .map(|p| p.id.clone())
                    .unwrap_or_default();
                (provider, form.model_id.read(cx).value().to_string())
            }
        }
    }

    /// TS `canSubmit`: provider and model named, and the typed context
    /// window is blank or a positive integer — a value the route would
    /// refuse disables Save instead of toasting after the fact.
    fn models_form_can_submit(&self, cx: &Context<Self>) -> bool {
        let Some(form) = &self.models_form else {
            return false;
        };
        let (provider_id, model_id) = self.models_form_ids(cx);
        let window = form.context_window.read(cx).value().to_string();
        !provider_id.trim().is_empty()
            && !model_id.trim().is_empty()
            && context_window_ok(&window)
    }

    /// Save: the TS `submit` — blank fields OMITTED (merge = keep), levels
    /// only when at least one is selected (an empty array is a no-op, not a
    /// clear), enabled always sent.
    fn submit_models_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &self.models_form else {
            return;
        };
        let adding = form.target.is_none();
        let (provider_id, model_id) = self.models_form_ids(cx);
        let window_text = form.context_window.read(cx).value().trim().to_string();
        let mut body = ModelUpsert {
            provider_id: provider_id.trim().to_string(),
            model_id: model_id.trim().to_string(),
            enabled: Some(form.enabled),
            ..Default::default()
        };
        if !window_text.is_empty() {
            // Validated by can_submit; parse again only to carry the value.
            body.context_window = parse_positive_integer(&window_text);
        }
        if !form.levels.is_empty() {
            body.reasoning_levels = Some(form.levels.clone());
        }
        self.runtime.send(UiCommand::UpsertModel { adding, body });
        cx.notify();
    }

    // ---- the page ----

    /// The Models page: a full page of its own below the status bar (the TS
    /// ConfigPageFrame — back header, refresh, notice banner, then the
    /// unsupported / error / loading / list states).
    pub fn render_models_page(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let page = self.snap.models_page.clone().unwrap_or_default();
        let mut column = div().flex().flex_col().flex_1().min_h_0();

        // Header: back, title, refresh.
        let runtime = self.runtime.clone();
        column = column.child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .h_9()
                .px_3()
                .flex_none()
                .border_b_1()
                .border_color(theme.border)
                .bg(theme.title_bar)
                .child(
                    Button::new("models-back")
                        .ghost()
                        .xsmall()
                        .icon(IconName::ArrowLeft)
                        .tooltip("Back to the workbench")
                        .on_click(move |_, _, _| runtime.send(UiCommand::CloseModels)),
                )
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme.foreground)
                        .child(t!("zconfig.models").into_owned()),
                )
                .child(div().flex_1())
                .child({
                    let runtime = self.runtime.clone();
                    Button::new("models-refresh")
                        .ghost()
                        .xsmall()
                        .icon(IconName::RefreshCw)
                        .tooltip("Reload the models list")
                        .disabled(page.loading)
                        .on_click(move |_, _, _| runtime.send(UiCommand::RefreshModels))
                }),
        );

        // The notice banner (effect-class and restart outcomes land here —
        // the config pages replace the chat surface, which is the only
        // other place a banner renders).
        if let Some(notice) = self.snap.config_notice.as_deref() {
            let runtime = self.runtime.clone();
            column = column.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .mx_3()
                    .mt_2()
                    .px_3()
                    .py_2()
                    .flex_none()
                    .rounded_md()
                    .bg(theme.secondary)
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .text_size(px(11.))
                    .text_color(theme.foreground)
                    .child(notice_text(notice)),
            )
                    .child(
                        Button::new("models-notice-dismiss")
                            .ghost()
                            .xsmall()
                            .label("×")
                            .tooltip("Dismiss")
                            .on_click(move |_, _, _| {
                                runtime.send(UiCommand::DismissConfigNotice)
                            }),
                    ),
            );
        }

        // The three frame states, in the TS order: unsupported → error →
        // first-load spinner; anything else paints the list.
        let body = if page.supported == Some(false) {
            // A capability gap, not a failure: the hub predates the
            // settings API (bridge < 0.47.0).
            state_card(&t!("zconfig.unsupported"), &t!("zconfig.unsupportedHint"), theme)
        } else if let Some(error) = &page.error {
            state_card(&t!("zconfig.loadFailed"), error, theme)
        } else if page.loading && page.models.is_none() {
            // Spinner only while NOTHING has landed yet — a refresh over a
            // populated list keeps the list on screen.
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .child(Spinner::new())
                .into_any_element()
        } else {
            self.render_models_list(&page, cx)
        };
        column.child(body).into_any_element()
    }

    /// The list body: selectable models (context window joined from the
    /// rules), the add control, and the Backend Restart affordance.
    fn render_models_list(&self, page: &ModelsPageView, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let mut list = div()
            .id("models-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .pb_4();

        let block = page.models.as_ref().and_then(|p| p.models.as_ref());
        let available = block
            .and_then(|b| b.available.as_deref())
            .unwrap_or(&[]);
        let rules = block
            .and_then(|b| b.model_rules.as_deref())
            .unwrap_or(&[]);

        if available.is_empty() {
            list = list.child(
                div()
                    .px_4()
                    .py_8()
                    .text_size(px(13.))
                    .text_color(theme.muted_foreground)
                    .child(t!("zconfig.modelsEmpty").into_owned()),
            );
        } else {
            let mut rows = div().flex().flex_col();
            for (index, m) in available.iter().enumerate() {
                rows = rows.child(self.render_model_row(index, m, rules, cx));
            }
            list = list.child(
                div()
                    .m_3()
                    .rounded_xl()
                    .bg(theme.title_bar)
                    .border_1()
                    .border_color(theme.border)
                    .overflow_hidden()
                    .child(rows),
            );
        }

        // The add control (TS modelsAdd).
        let wb = cx.entity();
        list = list.child(
            div().px_3().child(
                Button::new("models-add")
                    .secondary()
                    .small()
                    .w_full()
                    .icon(IconName::Plus)
                    .label(t!("zconfig.modelsAdd").into_owned())
                    .on_click(move |_, window, cx| {
                        wb.update(cx, |this, cx| this.open_models_form(None, window, cx));
                    }),
            ),
        );

        // The Backend Restart affordance (ZCodeConfigScreen `RestartNote`):
        // shown ONLY when a needs-restart write is armed — absent, not
        // disabled, when nothing is pending. Restarting cancels in-flight
        // turns, so it is never automatic; with no session open there is no
        // bridge to name and the affordance says so.
        if self.snap.pending_restart {
            let runtime = self.runtime.clone();
            let restart_card = div()
                .m_3()
                .p_3()
                .rounded_xl()
                .bg(theme.title_bar)
                .border_1()
                .border_color(theme.border)
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.foreground)
                        .child(t!("zconfig.restartPending").into_owned()),
                )
                .child(
                    div()
                        .pt_1()
                        .text_size(px(11.))
                        .text_color(theme.muted_foreground)
                        .child(t!("zconfig.restartWarning").into_owned()),
                )
                .child(if self.snap.active_tab.is_some() {
                    Button::new("models-restart")
                        .secondary()
                        .small()
                        .w_full()
                        .icon(IconName::RefreshCw)
                        .label(t!("zconfig.restart").into_owned())
                        .on_click(move |_, _, _| runtime.send(UiCommand::RestartBackend))
                        .into_any_element()
                } else {
                    div()
                        .pt_2()
                        .text_size(px(11.))
                        .text_color(theme.warning_foreground)
                        .child(t!("zconfig.restartNoInstance").into_owned())
                        .into_any_element()
                });
            list = list.child(restart_card);
        }
        list.into_any_element()
    }

    /// One selectable model row (TS available.map): the model id, the
    /// provider, and the context window joined from the rules —
    /// `{providerName ?? providerId ?? "—"}` plus `· {n}k context` when a
    /// rule carries a window.
    fn render_model_row(
        &self,
        index: usize,
        m: &ModelRef,
        rules: &[protocol::config_decode::ModelRule],
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let key = format!(
            "{}/{}",
            m.provider_id.as_deref().unwrap_or(""),
            m.model_id.as_deref().unwrap_or("")
        );
        let title = m.model_id.clone().unwrap_or_else(|| key.clone());
        let provider = m
            .provider_name
            .clone()
            .or_else(|| m.provider_id.clone())
            .unwrap_or_else(|| "—".to_string());
        // TS `(contextWindow / 1000).toFixed(0)` — and a FALSY window (0)
        // omits the suffix entirely.
        let window_note = context_window_for(m, Some(rules))
            .filter(|w| *w > 0)
            .map(|w| format!(" · {}k context", (w as f64 / 1000.0).round() as u64));
        let subtitle = match window_note {
            Some(note) => format!("{provider}{note}"),
            None => provider,
        };
        let wb = cx.entity();
        let target = m.clone();
        div()
            .id(SharedString::from(format!("model-row-{index}-{key}")))
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .py_3()
            .cursor_pointer()
            .hover(|this| this.bg(theme.secondary.opacity(0.6)))
            .on_click(move |_, window, cx| {
                wb.update(cx, |this, cx| {
                    this.open_models_form(Some(target.clone()), window, cx)
                });
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.foreground)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(subtitle),
                    ),
            )
            .child(
                div()
                    .size_3()
                    .flex_none()
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(IconName::ChevronRight)),
            )
            .into_any_element()
    }

    // ---- the form ----

    /// The add/edit form (TS `ConfigFormSheet`: a full screen of its own —
    /// a half-open form never sits behind the section list).
    pub fn render_models_form(&self, cx: &Context<Workbench>) -> AnyElement {
        let theme = cx.theme();
        let Some(form) = &self.models_form else {
            return div().into_any_element();
        };
        let busy = self
            .snap
            .models_page
            .as_ref()
            .map(|p| p.busy)
            .unwrap_or(false);
        let adding = form.target.is_none();
        let mut column = div().flex().flex_col().flex_1().min_h_0();

        // Header: back (never submits), title.
        let wb = cx.entity();
        column = column.child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .h_9()
                .px_3()
                .flex_none()
                .border_b_1()
                .border_color(theme.border)
                .bg(theme.title_bar)
                .child(
                    Button::new("model-form-back")
                        .ghost()
                        .xsmall()
                        .icon(IconName::ArrowLeft)
                        .tooltip("Back to the models list")
                        .on_click(move |_, _, cx| {
                            wb.update(cx, |this, cx| this.close_models_form(cx));
                        }),
                )
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme.foreground)
                        .child(if adding {
                            t!("zconfig.modelsAdd").into_owned()
                        } else {
                            t!("zconfig.modelsEdit").into_owned()
                        }),
                ),
        );

        let mut body = div().flex().flex_col().gap_2().p_4();

        if let Some(target) = &form.target {
            // Edit mode: the ids are fixed — TS prints them as
            // `[providerId, modelId].filter(Boolean).join(" / ")`.
            let ids = [target.provider_id.as_deref(), target.model_id.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" / ");
            body = body.child(
                div()
                    .font_family(theme.mono_font_family.clone())
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child(ids),
            );
        } else {
            // Add mode: the provider picker (a radio group — one of the
            // machine's non-account providers, or none) and the model id.
            body = body.child(field_label(&t!("zconfig.modelProvider"), None, theme)).child(
                RadioGroup::vertical("model-provider")
                    .selected_index(Some(form.provider_index))
                    .on_change({
                        let wb = cx.entity();
                        move |ix: &usize, _, cx| {
                            wb.update(cx, |this, cx| {
                                if let Some(form) = &mut this.models_form {
                                    form.provider_index = *ix;
                                }
                                cx.notify();
                            });
                        }
                    })
                    .children(
                        std::iter::once("—".to_string())
                            .chain(form.providers.iter().map(|p| p.name.clone())),
                    ),
            );
            body = body
                .child(field_label(&t!("zconfig.modelId"), None, theme))
                .child(
                    div()
                        .max_w(rems(46.))
                        .child(Input::new(&form.model_id)),
                );
        }

        // Context window: blank keeps the current value (the API has no
        // way to clear one).
        body = body
            .child(field_label(
                &t!("zconfig.modelContextWindow"),
                Some(&t!("zconfig.modelKeepBlank")),
                theme,
            ))
            .child(
                div()
                    .max_w(rems(46.))
                    .child(Input::new(&form.context_window)),
            );

        // Reasoning levels: chips of the candidates (this model's own plus
        // every rule's), or the none-configured note.
        body = body.child(field_label(
            &t!("zconfig.modelReasoningLevels"),
            Some(&t!("zconfig.modelLevelsHint")),
            theme,
        ));
        if form.candidates.is_empty() {
            body = body.child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child(t!("zconfig.modelLevelsNone").into_owned()),
            );
        } else {
            let mut chips = div().flex().flex_wrap().gap_2();
            for level in &form.candidates {
                let on = form.levels.iter().any(|l| l == level);
                let wb = cx.entity();
                let picked = level.clone();
                chips = chips.child(
                    Button::new(SharedString::from(format!("level-{}", slug(level))))
                        .xsmall()
                        .label(level.clone())
                        .when(on, |b| b.primary())
                        .when(!on, |b| b.secondary().outline())
                        .on_click(move |_, _, cx| {
                            wb.update(cx, |this, cx| {
                                this.toggle_models_level(&picked, cx);
                            });
                        }),
                );
            }
            body = body.child(chips);
        }

        // Enabled (TS `rule?.enabled !== false` prefill).
        body = body.child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .pt_2()
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.foreground)
                        .child(t!("zconfig.enabled").into_owned()),
                )
                .child(Switch::new("model-enabled").checked(form.enabled).on_click({
                    let wb = cx.entity();
                    move |checked: &bool, _, cx| {
                        wb.update(cx, |this, cx| {
                            if let Some(form) = &mut this.models_form {
                                form.enabled = *checked;
                            }
                            cx.notify();
                        });
                    }
                })),
        );

        // Save: disabled while busy or invalid; the write outcome (not the
        // click) closes the form.
        let wb = cx.entity();
        body = body.child(
            div()
                .pt_4()
                .max_w(rems(46.))
                .child(
                    Button::new("model-save")
                        .primary()
                        .w_full()
                        .label(if busy {
                            t!("zconfig.saving").into_owned()
                        } else {
                            t!("common.save").into_owned()
                        })
                        .disabled(busy || !self.models_form_can_submit(cx))
                        .on_click(move |_, _, cx| {
                            wb.update(cx, |this, cx| this.submit_models_form(cx));
                        }),
                ),
        );

        column
            .child(
                div()
                    .id("model-form-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body),
            )
            .into_any_element()
    }
}

/// A labelled form field (TS `ConfigField`): label, control, hint.
fn field_label(label: &str, hint: Option<&str>, theme: &component::Theme) -> gpui::Div {
    let mut field = div().flex().flex_col().gap_1().pt_2();
    field = field.child(
        div()
            .text_size(px(11.))
            .text_color(theme.foreground)
            .child(label.to_string()),
    );
    if let Some(hint) = hint {
        field = field.child(
            div()
                .text_size(px(10.))
                .text_color(theme.muted_foreground)
                .child(hint.to_string()),
        );
    }
    field
}

/// The unsupported / error card (TS ConfigPageFrame's two states): a title
/// line plus the detail under it.
fn state_card(title: &str, detail: &str, theme: &component::Theme) -> AnyElement {
    div()
        .m_3()
        .px_4()
        .py_6()
        .rounded_xl()
        .bg(theme.title_bar)
        .border_1()
        .border_color(theme.border)
        .flex()
        .flex_col()
        .items_center()
        .child(
            div()
                .text_size(px(12.))
                .text_color(theme.foreground)
                .child(title.to_string()),
        )
        .child(
            div()
                .pt_2()
                .text_size(px(11.))
                .text_color(theme.muted_foreground)
                .child(detail.to_string()),
        )
        .into_any_element()
}

/// TS `contextWindowOk` (ModelsPage.tsx): blank is fine; anything else
/// must be a POSITIVE INTEGER — the route throws on `1.5`, `0`, or `abc`.
fn context_window_ok(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.is_empty() || parse_positive_integer(trimmed).is_some()
}

/// `Number(text)` + integer check: f64 semantics so `1e3` validates
/// exactly like the TS form, then narrowed into the u64 the body carries.
fn parse_positive_integer(text: &str) -> Option<u64> {
    let n: f64 = text.trim().parse().ok()?;
    if !n.is_finite() || n.fract() != 0.0 || n <= 0.0 || n >= u64::MAX as f64 {
        return None;
    }
    Some(n as u64)
}

/// Element-id-safe slug (ids carry ':', '/', …).
fn slug(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect()
}
