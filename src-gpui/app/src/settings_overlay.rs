//! The desktop settings overlay (ADR-0014 §1): the full-width surface the
//! 380px drawer became. A grouped left rail (three groups, upstream's
//! model — general / agent capabilities / data & stats), a fluid content
//! pane on the right, and one back-to-workspace top bar. The rail
//! collapses to icons at narrow widths; no fixed form widths remain.
//!
//! State is view-local like the old drawer was: the rail's section pick
//! seeds the driver-side page opens (loads, probes), and closing the
//! overlay retires them. The last-visited section persists across
//! reopens (acceptance scenario 1).

use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, Context, Window, div, px};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _,
};
use gpui_kit::assets::IconName as KitIconName;
use gpui_kit::TestSupportExt as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::i18n::now_ms;
use crate::runtime::{ConfigPageKind, UiCommand};
use crate::ui_prefs::ui_text;
use crate::workbench::Workbench;

/// One rail destination. `Page` covers the six ZCode-config sections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsSection {
    /// Connection & general: servers, language, font, OSC 52, hub upgrade,
    /// and the workflow-gate switch (always reachable — the Workflows
    /// section itself hides while the gate is off, so this row is the only
    /// way in, fail-closed).
    General,
    Models,
    Page(ConfigPageKind),
    Workflows,
    Usage,
    Quota,
    System,
}

impl SettingsSection {
    /// Stable element-id fragment for the rail row and tests.
    fn key(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Models => "models",
            Self::Page(ConfigPageKind::Skills) => "skills",
            Self::Page(ConfigPageKind::Mcp) => "mcp",
            Self::Page(ConfigPageKind::Hooks) => "hooks",
            Self::Page(ConfigPageKind::Agents) => "agents",
            Self::Page(ConfigPageKind::Backups) => "backups",
            Self::Page(ConfigPageKind::AppUpdate) => "app-update",
            Self::Workflows => "workflows",
            Self::Usage => "usage",
            Self::Quota => "quota",
            Self::System => "system",
        }
    }

    /// The top bar's title (the section's catalog key).
    fn title_key(self) -> &'static str {
        match self {
            Self::General => "settings.general",
            Self::Models => "zconfig.models",
            Self::Page(kind) => kind.title(),
            Self::Workflows => "zconfig.entryWorkflows",
            Self::Usage => "zconfig.usage",
            Self::Quota => "quota.title",
            Self::System => "system.title",
        }
    }

    /// The load-seeding command an open sends (None = view-local content).
    fn open_command(self) -> Option<UiCommand> {
        match self {
            Self::General => None,
            Self::Models => Some(UiCommand::OpenModels),
            Self::Page(kind) => Some(UiCommand::OpenConfigPage { page: kind }),
            Self::Workflows => Some(UiCommand::OpenWorkflows),
            Self::Usage => Some(UiCommand::OpenUsage),
            Self::Quota => Some(UiCommand::OpenQuota),
            Self::System => Some(UiCommand::OpenSystemStatus),
        }
    }

    /// The top bar's refresh, where the section has one (Usage's tabs and
    /// General re-read on interaction).
    fn refresh_command(self) -> Option<UiCommand> {
        match self {
            Self::General | Self::Usage => None,
            Self::Models => Some(UiCommand::RefreshModels),
            Self::Page(kind) => Some(UiCommand::RefreshConfigPage { page: kind }),
            Self::Workflows => Some(UiCommand::RefreshWorkflows),
            Self::Quota => Some(UiCommand::RefreshQuota),
            Self::System => Some(UiCommand::RefreshSystemStatus),
        }
    }
}

/// One clickable rail row.
struct RailRow {
    section: SettingsSection,
    icon: KitIconName,
    title_key: &'static str,
}

/// The three rail groups (upstream's sidebar model, our mapping — the
/// alignment table). Group headers are labels, never clickable.
fn rail_groups() -> Vec<(&'static str, Vec<RailRow>)> {
    vec![
        (
            "settings.groupGeneral",
            vec![RailRow {
                section: SettingsSection::General,
                icon: KitIconName::Settings2,
                title_key: "settings.general",
            }],
        ),
        (
            "settings.groupAgent",
            vec![
                RailRow {
                    section: SettingsSection::Models,
                    icon: KitIconName::Boxes,
                    title_key: "zconfig.models",
                },
                RailRow {
                    section: SettingsSection::Page(ConfigPageKind::Skills),
                    icon: KitIconName::Puzzle,
                    title_key: "zconfig.skills",
                },
                RailRow {
                    section: SettingsSection::Page(ConfigPageKind::Mcp),
                    icon: KitIconName::PlugZap,
                    title_key: "zconfig.mcp",
                },
                RailRow {
                    section: SettingsSection::Page(ConfigPageKind::Hooks),
                    icon: KitIconName::Webhook,
                    title_key: "zconfig.hooks",
                },
                RailRow {
                    section: SettingsSection::Page(ConfigPageKind::Agents),
                    icon: KitIconName::Bot,
                    title_key: "zconfig.agents",
                },
            ],
        ),
        (
            "settings.groupData",
            vec![
                RailRow {
                    section: SettingsSection::Usage,
                    icon: KitIconName::ChartColumn,
                    title_key: "zconfig.usage",
                },
                RailRow {
                    section: SettingsSection::Quota,
                    icon: KitIconName::Gauge,
                    title_key: "quota.title",
                },
                RailRow {
                    section: SettingsSection::System,
                    icon: KitIconName::Activity,
                    title_key: "system.title",
                },
                RailRow {
                    section: SettingsSection::Page(ConfigPageKind::Backups),
                    icon: KitIconName::ArchiveRestore,
                    title_key: "zconfig.backups",
                },
                RailRow {
                    section: SettingsSection::Page(ConfigPageKind::AppUpdate),
                    icon: KitIconName::Download,
                    title_key: "zconfig.appUpdate",
                },
            ],
        ),
    ]
}

/// Below this viewport width the rail drops its labels (icon rail) — the
/// overlay stays usable in narrow web-SPA windows without horizontal
/// clipping (acceptance scenario 8).
const RAIL_COLLAPSE_AT: f32 = 900.;
const RAIL_WIDTH: f32 = 232.;
const RAIL_WIDTH_COLLAPSED: f32 = 48.;

/// The pane's not-yet-arrived placeholder: the section's open command just
/// left, its snapshot lands in a beat — a centered spinner bridges the gap
/// (the pages themselves `expect` their view).
fn settings_pending_pane() -> AnyElement {
    div()
        .flex()
        .flex_1()
        .min_h_0()
        .items_center()
        .justify_center()
        .child(Spinner::new())
        .into_any_element()
}

impl Workbench {
    // ---- state ----

    /// Open (or switch to) a section: retire the previous section's driver
    /// page first — its polls must not run under a surface that no longer
    /// shows them — then seed the new section's reads.
    pub(crate) fn open_settings(&mut self, section: SettingsSection, cx: &mut gpui::Context<Self>) {
        self.quota_drawer = false; // one overlay slot
        if self.settings != Some(section) {
            if self.settings_form_rung(cx) {
                // An open form answers the switch first (the form contract);
                // the switch itself lands on the next request.
                cx.notify();
                return;
            }
            self.close_settings_pages();
        }
        self.settings = Some(section);
        self.settings_last = section;
        if let Some(cmd) = section.open_command() {
            self.runtime.send(cmd);
        } else {
            // The general pane's quota card and the fail-closed gate probe
            // (the old drawer's open seeds, carried over).
            self.runtime.send(UiCommand::RefreshQuota);
            if self.snap.workflow_gate.is_none() {
                self.runtime.send(UiCommand::ProbeWorkflowGate);
            }
        }
        cx.notify();
    }

    /// Toggle from the rail gear / ⌘,: closed opens the last section, open
    /// closes (the top bar's back affordance lands here too).
    pub(crate) fn toggle_settings(&mut self, cx: &mut gpui::Context<Self>) {
        if self.settings.is_some() {
            // The form rung answers first (the form contract): an armed or
            // just-closed form keeps the overlay open this press.
            if self.settings_form_rung(cx) {
                cx.notify();
                return;
            }
            self.settings = None;
            self.close_settings_pages();
        } else {
            let section = self.settings_last;
            self.open_settings(section, cx);
        }
        cx.notify();
    }

    /// Close the overlay and retire every section page it hosted.
    pub(crate) fn close_settings(&mut self, cx: &mut gpui::Context<Self>) {
        // Same rung as the toggle: a form that is armed or just closed keeps
        // the overlay open; "Back to workspace" lands on the next press.
        if self.settings_form_rung(cx) {
            cx.notify();
            return;
        }
        self.settings = None;
        self.close_settings_pages();
        cx.notify();
    }

    /// Retire any open driver-side section page (the overlay is their only
    /// host now, so a page outliving its section would poll unseen).
    pub(crate) fn close_settings_pages(&mut self) {
        if let Some(page) = self.snap.config_section_page.as_ref() {
            self.runtime
                .send(UiCommand::CloseConfigPage { page: page.kind });
        }
        if self.snap.models_page.is_some() {
            self.runtime.send(UiCommand::CloseModels);
        }
        if self.snap.system_status.is_some() {
            self.runtime.send(UiCommand::CloseSystemStatus);
        }
        if self.snap.quota_page.is_some() {
            self.runtime.send(UiCommand::CloseQuota);
        }
        if self.snap.usage_page.is_some() {
            self.runtime.send(UiCommand::CloseUsage);
        }
        if self.snap.workflows_page.is_some() {
            self.runtime.send(UiCommand::CloseWorkflows);
        }
    }

    // ---- the search palette (ADR-0014 §4) ----

    /// Open the palette: build the input on first use, focus it deferred
    /// (the panel paints first — the ⌘K discipline), and start at the top.
    pub(crate) fn open_settings_search(
        &mut self,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.settings_search_input.is_none() {
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(t!("settings.searchPlaceholder").into_owned())
            });
            let sub = cx.subscribe(&input, |this: &mut Workbench, _emitter, event: &InputEvent, cx| match event {
                InputEvent::PressEnter { .. } => this.commit_settings_search(cx),
                // Any edit re-derives the list — the highlight resets with it.
                InputEvent::Change => {
                    this.settings_search_selected = 0;
                    cx.notify();
                }
                _ => {}
            });
            sub.detach();
            self.settings_search_input = Some(input);
        }
        self.settings_search_open = true;
        self.settings_search_selected = 0;
        let input = self.settings_search_input.clone();
        window.defer(cx, move |window, cx| {
            if let Some(input) = input {
                let handle = input.read(cx).focus_handle(cx).clone();
                window.focus(&handle, cx);
            }
        });
        cx.notify();
    }

    pub(crate) fn close_settings_search(&mut self, cx: &mut gpui::Context<Self>) {
        self.settings_search_open = false;
        cx.notify();
    }

    /// Enter: navigate to the highlighted hit, ring its mounted row ~2s
    /// (no focus steal), and fold the palette.
    pub(crate) fn commit_settings_search(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(input) = self.settings_search_input.clone() else {
            return;
        };
        let query = input.read(cx).value().to_string();
        let hits = crate::settings_search::search(&query);
        let Some(hit) = hits.get(self.settings_search_selected.min(hits.len().saturating_sub(1)))
        else {
            return;
        };
        let section = hit.section;
        let anchor = hit.anchor.to_string();
        self.close_settings_search(cx);
        self.open_settings(section, cx);
        self.settings_highlight = Some((anchor, now_ms() + 2_000));
        cx.notify();
    }

    /// The palette's arrows (the keystroke interceptor's body): one step
    /// through the CURRENT result list, clamped.
    pub(crate) fn move_settings_search(
        &mut self,
        down: bool,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(input) = self.settings_search_input.clone() else {
            return;
        };
        let query = input.read(cx).value().to_string();
        let len = crate::settings_search::search(&query).len();
        if len == 0 {
            return;
        }
        let next = if down {
            (self.settings_search_selected + 1).min(len - 1)
        } else {
            self.settings_search_selected.saturating_sub(1)
        };
        self.settings_search_selected = next;
        cx.notify();
    }

    /// Whether a mounted row's ~2s search highlight is still live.
    pub(crate) fn settings_row_highlighted(&self, anchor: &str) -> bool {
        self.settings_highlight
            .as_ref()
            .is_some_and(|(a, until)| a == anchor && now_ms() < *until)
    }

    /// The palette pane (replaces the content pane while open; the rail
    /// stays): input on top, results below, click or Enter to navigate.
    fn render_settings_search_pane(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let wb = cx.entity();
        let Some(input) = self.settings_search_input.clone() else {
            return settings_pending_pane();
        };
        let query = input.read(cx).value().to_string();
        let hits = crate::settings_search::search(&query);
        let selected = self.settings_search_selected.min(hits.len().saturating_sub(1));

        let mut list = div()
            .id("settings-search-results")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .p_2()
            .gap_1();
        if hits.is_empty() {
            list = list.child(
                div()
                    .id("settings-search-empty")
                    .test_support()
                    .px_2()
                    .py_4()
                    .text_size(ui_text(12.))
                    .text_color(theme.muted_foreground)
                    .child(t!("settings.searchEmpty").into_owned()),
            );
        }
        for (ix, hit) in hits.iter().enumerate() {
            let wb_row = wb.clone();
            let row_selected = ix == selected;
            let label = t!(hit.key).into_owned();
            let section_title = t!(hit.section.title_key()).into_owned();
            list = list.child(
                div()
                    .id(SharedString::from(format!("settings-search-row-{ix}")))
                    .test_support()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py(px(6.))
                    .rounded_md()
                    .cursor_pointer()
                    .when(row_selected, |this| this.bg(theme.secondary))
                    .when(!row_selected, |this| {
                        this.hover(|this| this.bg(crate::ui_tokens::hover_bg()))
                    })
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_size(ui_text(12.))
                            .text_color(theme.foreground)
                            .child(label),
                    )
                    .child(
                        div()
                            .flex_none()
                            .truncate()
                            .max_w(px(180.))
                            .text_size(ui_text(10.))
                            .text_color(theme.muted_foreground)
                            .child(section_title),
                    )
                    .on_click(move |_, _, cx| {
                        wb_row.update(cx, |wb, cx| {
                            wb.settings_search_selected = ix;
                            wb.commit_settings_search(cx);
                        });
                    }),
            );
        }

        div()
            .id("settings-search")
            .test_support()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
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
                    .bg(theme.title_bar)
                    .child(Input::new(&input).small()),
            )
            .child(list)
            .into_any_element()
    }

    // ---- the overlay ----

    /// The whole settings surface: top bar over rail + content pane. It
    /// replaces the workbench content region (like the config pages it
    /// hosts did); the status bar stays above it.
    pub(crate) fn render_settings_overlay(
        &self,
        window: &mut Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let section = self.settings.unwrap_or(self.settings_last);
        let collapsed = window.viewport_size().width < px(RAIL_COLLAPSE_AT);

        let content = match section {
            SettingsSection::General => self.render_general_settings_pane(cx),
            SettingsSection::Models => {
                if self.provider_form.is_some() {
                    self.render_provider_form(cx)
                } else if self.models_form.is_some() {
                    self.render_models_form(cx)
                } else {
                    self.render_models_page(cx)
                }
            }
            SettingsSection::Page(_) => {
                if self.zconfig_form.is_some() {
                    self.render_zconfig_form(cx)
                } else if self.snap.config_section_page.is_some() {
                    crate::zconfig_pages::render_config_section_page(self, cx)
                } else {
                    settings_pending_pane()
                }
            }
            SettingsSection::Workflows => {
                if self.workflow_start_form.is_some() {
                    self.render_workflow_start_form(cx)
                } else if self.workflow_amend_form.is_some() {
                    self.render_workflow_amend_form(cx)
                } else if self.workflow_meta_form.is_some() {
                    self.render_workflow_meta_form(cx)
                } else if self.snap.workflows_page.is_some() {
                    crate::workflows_page::render_workflows_page(self, cx)
                } else {
                    settings_pending_pane()
                }
            }
            SettingsSection::Usage => match self.snap.usage_page.as_ref() {
                Some(page) => crate::usage_page::render_usage_page(
                    page,
                    self.snap.config_notice.as_ref(),
                    self.snap.pending_restart,
                    // The restart addresses the ACTIVE TAB's bridge; with no
                    // tab open the note shows its no-instance hint instead.
                    self.snap.active_tab.is_some(),
                    &self.runtime,
                    &theme,
                ),
                None => settings_pending_pane(),
            },
            SettingsSection::Quota => self.render_quota_page(cx),
            SettingsSection::System => match self.snap.system_status.as_ref() {
                Some(page) => crate::system_status::render_system_status_page(
                    page,
                    self.snap.config_notice.as_ref(),
                    self.snap.pending_restart,
                    self.snap.active_tab.is_some(),
                    self.snap.toast.as_ref().map(|t| t.text.as_str()),
                    &self.runtime,
                    cx,
                ),
                None => settings_pending_pane(),
            },
        };

        div()
            .id("settings-overlay")
            .test_support()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .bg(theme.background)
            .child(self.render_settings_top_bar(section, cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_settings_rail(section, collapsed, cx))
                    // The pane owns its scrolling; the rail never scrolls
                    // with it. The search palette replaces the pane while
                    // open (the rail stays — it IS the results' namespace).
                    .child(
                        div().flex().flex_1().min_w_0().min_h_0().child(if self.settings_search_open {
                            self.render_settings_search_pane(cx)
                        } else {
                            content
                        }),
                    ),
            )
            .into_any_element()
    }

    /// Back-to-workspace, the section title, the section's refresh, and
    /// the search entry — the page bodies lost their own headers when
    /// they moved in here.
    fn render_settings_top_bar(
        &self,
        section: SettingsSection,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let wb = cx.entity();
        let wb_search = cx.entity();
        let refresh = section.refresh_command().map(|cmd| {
            let runtime = self.runtime.clone();
            let disabled = match section {
                // The busy list state owns the spinner story; a refresh
                // during load only stacks reads.
                SettingsSection::Models => self
                    .snap
                    .models_page
                    .as_ref()
                    .is_some_and(|p| p.loading),
                SettingsSection::Workflows => self
                    .snap
                    .workflows_page
                    .as_ref()
                    .is_some_and(|p| p.loading),
                SettingsSection::Page(_) => self
                    .snap
                    .config_section_page
                    .as_ref()
                    .is_some_and(|p| p.loading),
                _ => false,
            };
            Button::new("settings-refresh")
                .ghost()
                .xsmall()
                .icon(IconName::RefreshCw)
                .tooltip(t!("common.reload"))
                .disabled(disabled)
                .on_click(move |_, _, _| runtime.send(cmd.clone()))
                .into_any_element()
        });
        div()
            .flex()
            .items_center()
            .gap_1()
            .h_9()
            .px_2()
            .flex_none()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.title_bar)
            .child(
                Button::new("settings-back")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ArrowLeft)
                    .label(t!("settings.backToWorkspace").into_owned())
                    .tooltip(t!("settings.backToWorkspace"))
                    .on_click(move |_, _, cx| {
                        wb.update(cx, |wb, cx| wb.close_settings(cx));
                    }),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .text_size(ui_text(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.foreground)
                    .child(t!(section.title_key()).into_owned()),
            )
            .when_some(refresh, |this, refresh| this.child(refresh))
            .child(
                Button::new("settings-search-toggle")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Search)
                    .tooltip(t!("settings.search"))
                    .on_click(move |_, window, cx| {
                        wb_search.update(cx, |wb, cx| wb.open_settings_search(window, cx));
                    }),
            )
            .into_any_element()
    }

    /// The grouped rail. Group headers are captions; rows highlight the
    /// active section; the Workflows row joins only while the gate verdict
    /// says ON (fail-closed — the General pane's switch is the way in).
    fn render_settings_rail(
        &self,
        active: SettingsSection,
        collapsed: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let wb = cx.entity();
        let width = if collapsed {
            RAIL_WIDTH_COLLAPSED
        } else {
            RAIL_WIDTH
        };
        let mut column = div()
            .id("settings-rail")
            .w(px(width))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(10.))
            .p_2()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.title_bar);

        let workflows_on = self
            .snap
            .workflow_gate
            .as_ref()
            .is_some_and(|g| g.enabled);
        for (group_key, rows) in rail_groups() {
            let mut group = div().flex().flex_col().gap(px(2.));
            if !collapsed {
                group = group.child(
                    div()
                        .px_2()
                        .pt_1()
                        .pb(px(2.))
                        .text_size(ui_text(10.))
                        .text_color(theme.muted_foreground)
                        .child(t!(group_key).into_owned()),
                );
            }
            for row in rows {
                let selected = row.section == active;
                let label = t!(row.title_key).into_owned();
                let wb_row = wb.clone();
                let mut el = div()
                    .id(SharedString::from(format!(
                        "settings-nav-{}",
                        row.section.key()
                    )))
                    .test_support()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py(px(6.))
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |this| this.bg(theme.secondary))
                    .when(!selected, |this| {
                        this.hover(|this| this.bg(crate::ui_tokens::hover_bg()))
                    })
                    .child(
                        Icon::new(row.icon)
                            .size_4()
                            .flex_none()
                            .text_color(if selected {
                                theme.foreground
                            } else {
                                theme.muted_foreground
                            }),
                    );
                if !collapsed {
                    el = el.child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_size(ui_text(12.))
                            .text_color(if selected {
                                theme.foreground
                            } else {
                                theme.muted_foreground
                            })
                            .child(label.clone()),
                    );
                }
                el = el.tooltip(move |window, cx| {
                    Tooltip::new(label.clone()).build(window, cx)
                });
                let section = row.section;
                group = group.child(
                    el.on_click(move |_, _, cx| {
                        wb_row.update(cx, |wb, cx| wb.open_settings(section, cx));
                    }),
                );
            }
            // The gated Workflows row rides the agent group's tail.
            if group_key == "settings.groupAgent" && workflows_on {
                let label = t!("zconfig.entryWorkflows").into_owned();
                let selected = SettingsSection::Workflows == active;
                let wb_row = wb.clone();
                group = group.child(
                    div()
                        .id("settings-nav-workflows")
                        .test_support()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py(px(6.))
                        .rounded_md()
                        .cursor_pointer()
                        .when(selected, |this| this.bg(theme.secondary))
                        .when(!selected, |this| {
                            this.hover(|this| this.bg(crate::ui_tokens::hover_bg()))
                        })
                        .child(
                            Icon::new(KitIconName::Workflow)
                                .size_4()
                                .flex_none()
                                .text_color(if selected {
                                    theme.foreground
                                } else {
                                    theme.muted_foreground
                                }),
                        )
                        .when(!collapsed, |this| {
                            this.child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .truncate()
                                    .text_size(ui_text(12.))
                                    .text_color(if selected {
                                        theme.foreground
                                    } else {
                                        theme.muted_foreground
                                    })
                                    .child(label.clone()),
                            )
                        })
                        .on_click(move |_, _, cx| {
                            wb_row.update(cx, |wb, cx| {
                                wb.open_settings(SettingsSection::Workflows, cx)
                            });
                        }),
                );
            }
            column = column.child(group);
        }
        column.into_any_element()
    }

    /// One General-pane row, wrapped for the search navigation's ~2s
    /// highlight (the anchor's ring — no focus steal, upstream
    /// `SettingsSearchTarget`).
    fn general_pane_row(
        &self,
        anchor: &'static str,
        row: AnyElement,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .id(anchor)
            .when(self.settings_row_highlighted(anchor), |this| {
                this.rounded_md()
                    .border_1()
                    .border_color(theme.accent.opacity(0.7))
            })
            .child(row)
    }

    /// The General pane: the old drawer's inline sections as first-class
    /// rows (the alignment table's "Connection & general"), closing with
    /// the always-visible workflow-gate switch.
    fn render_general_settings_pane(&self, cx: &Context<Self>) -> AnyElement {
        let mut column = div()
            .id("settings-general")
            .test_support()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .overflow_x_hidden()
            .flex()
            .flex_col()
            .gap_2()
            .p_2();
        column = column
            .child(self.general_pane_row("settings-row-servers", self.render_servers_section(cx), cx))
            .child(self.general_pane_row("settings-row-language", self.render_language_section(cx), cx))
            .child(self.general_pane_row("settings-row-font", self.render_font_size_section(cx), cx))
            .child(self.general_pane_row("settings-row-osc52", self.render_osc52_section(cx), cx))
            .child(self.general_pane_row("settings-row-upgrade", self.render_hub_upgrade_section(cx), cx))
            .child(self.general_pane_row("settings-row-gate", self.render_workflow_gate_entry(cx), cx));
        column.into_any_element()
    }
}
