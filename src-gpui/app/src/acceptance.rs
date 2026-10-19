//! The automated acceptance suite: one journey-level walk per shipped
//! feature area, on the REAL Workbench (headless `#[gpui_kit::test]`
//! windows, inert runtime, element-id clicks). The per-feature tests that
//! pin individual behaviors stay in their owning modules; this file chains
//! them the way a user actually travels and fills the journeys nothing
//! else walked end-to-end.
//!
//! `scripts/acceptance.sh` maps every implemented feature to its guarding
//! tests via `scripts/acceptance-manifest.txt` (including these
//! `accept_*` walks) and prints the per-feature verdict. A feature with no
//! matching test is reported as uncovered — the manifest, not this file,
//! is the feature inventory.

use crate::runtime::{
    ConfigPageKind, ConfigSectionPageView, FsBrowseView, HistoryProjectRow, ModelsPageView,
    NewSessionView, Phase, ProtocolRuntime, QuotaCardView, QuotaPageView, RuntimeEvent, TabPhase,
    TabSummary, TabView, UsagePageView, UsageTab, UiCommand, WorkbenchSnapshot, WorkflowDetailView,
    WorkflowsPageView,
};
use crate::workbench::{StopTurn, Workbench};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    px, AnyWindowHandle, App, AppContext as _, Bounds, Entity, Point, Size, TestAppContext,
    WindowBounds, WindowOptions, Window,
};
use protocol::hub::{FsEntry, FsEntryKind, FsListing};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// One connected tab (`sess_1`) — the settled state every chat-side walk
/// starts from.
fn connected_snapshot(active: Option<TabView>) -> WorkbenchSnapshot {
    let mut snap = WorkbenchSnapshot::initial();
    snap.phase = Phase::Connected;
    snap.tabs = vec![TabSummary {
        id: 7,
        session_id: Some("sess_1".to_string()),
        session_title: None,
        phase: TabPhase::Connected,
        running: false,
        pending: 0,
        awaiting: false,
    }];
    snap.active_tab = Some(7);
    snap.active = active;
    snap
}

fn tab_view(running: bool) -> TabView {
    TabView {
        session_id: Some("sess_1".to_string()),
        agent_version: None,
        phase: TabPhase::Connected,
        running,
        loading_session: false,
        messages: Vec::new(),
        pending: Vec::new(),
        has_more: false,
        total_messages: None,
        loading_earlier: false,
        approval: None,
        config_options: Vec::new(),
        usage: None,
        cache_hit: None,
        turn_stats: None,
        plan_entries: None,
        fs_capable: true,
        available_commands: Vec::new(),
    }
}

fn open(
    cx: &mut App,
    snap: WorkbenchSnapshot,
    height: f32,
) -> (AnyWindowHandle, Entity<Workbench>, ProtocolRuntime) {
    let runtime = ProtocolRuntime::inert();
    let (handle, wb) = gpui_kit::open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: Point::default(),
                size: Size {
                    width: px(800.),
                    height: px(height),
                },
            })),
            ..Default::default()
        },
        cx,
        |window, cx| cx.new(|cx| Workbench::new(window, cx, runtime.clone(), snap)),
    )
    .expect("open test window");
    (handle, wb, runtime)
}

/// Feed a follow-up snapshot through the exact path the driver's events
/// take, so a journey can advance state mid-test.
fn advance(cx: &mut TestAppContext, wb: &Entity<Workbench>, snap: WorkbenchSnapshot) {
    cx.update(|cx| {
        wb.update(cx, |wb, cx| wb.on_runtime_event(RuntimeEvent::Snapshot(snap), cx));
    });
}

fn sent(runtime: &ProtocolRuntime, predicate: impl Fn(&UiCommand) -> bool) -> bool {
    runtime.recorded_commands().iter().any(predicate)
}

fn count(runtime: &ProtocolRuntime, predicate: impl Fn(&UiCommand) -> bool) -> usize {
    runtime
        .recorded_commands()
        .iter()
        .filter(|c| predicate(c))
        .count()
}

/// The side panels' quota card fixture: every provider section the hub
/// reports (GLM success with a level, Go success, Ollama auth-expired, the
/// CLIProxy pool the server added after the TS card was written).
fn quota_usage_fixture() -> protocol::quota::AccountUsageStats {
    use protocol::quota::*;
    AccountUsageStats {
        glm: GlmUsageStats {
            kind: "success".to_string(),
            level: Some("max".to_string()),
            items: Some(vec![QuotaItem {
                key: "token_5h".to_string(),
                label: "5h".to_string(),
                used_percent: 42.0,
                left_percent: Some(58.0),
                used_count: None,
                total_count: None,
                next_reset_time: Some(1_700_000_000_000),
                detail: None,
            }]),
        },
        opencode: GoUsageStats {
            kind: "success".to_string(),
            windows: Some(vec![GoWindowEntry {
                key: GoWindowKey::Rolling,
                label: "5h".to_string(),
                usage_percent: 10.0,
                resets_at: 1_700_000_000_000,
            }]),
        },
        ollama: Some(OllamaUsageStats {
            kind: "auth_error".to_string(),
            windows: None,
            usage: None,
        }),
        cliproxy: Some(CpUsageStats {
            kind: "success".to_string(),
            accounts: None,
            combined: Some(vec![CpWindowEntry {
                window: "5h".to_string(),
                used_percent: 5.0,
                resets_at: None,
            }]),
            other_accounts: None,
        }),
    }
}

// ---------------------------------------------------------------------------
// The geometry sweep (paint gate, layer A)
// ---------------------------------------------------------------------------

/// Every OBSERVED element must lay out non-zero and inside the viewport
/// after a real frame — the "looks broken" family (zero-size, clipped
/// off-window) that element-tree visibility assertions cannot see. Plain
/// divs are only observable with `.id().test_support()`, so the sweep
/// covers exactly the ids the journeys pin.
///
/// Bounds are UNCLIPPED: an `overflow_y_scroll` list still paints its
/// below-the-fold rows, so a fixture taller than the window would
/// false-positive by design — every journey keeps its content within one
/// frame (the settings journey's 2400px window exists for exactly that).
fn sweep_geometry(window: &Window) {
    let viewport = window.viewport_size();
    for snap in gpui_kit::base::test_support::snapshots(window) {
        let id = format!("{:?}", snap.path().last());
        let b = snap.bounds();
        assert!(
            b.size.width > px(0.) && b.size.height > px(0.),
            "{id} laid out at a zero size"
        );
        assert!(
            b.origin.x >= px(0.) && b.origin.y >= px(0.),
            "{id} starts off-window (bounds {b:?})"
        );
        assert!(
            b.right() <= viewport.width && b.bottom() <= viewport.height,
            "{id} exceeds the viewport {viewport:?} (bounds {b:?})"
        );
    }
}

/// Pairwise-disjoint bounds for every observed id containing `prefix` —
/// the overlap detector for list rows (edge-touching is legal layout,
/// strict overlap is not). Returns how many rows matched, so callers can
/// pin the count the fixture implies.
fn rows_disjoint(window: &Window, prefix: &str) -> usize {
    let rows: Vec<(String, Bounds<gpui_kit::Pixels>)> =
        gpui_kit::base::test_support::snapshots(window)
    .into_iter()
    .filter(|s| format!("{:?}", s.path().last()).contains(prefix))
    .map(|s| (format!("{:?}", s.path().last()), s.bounds()))
    .collect();
    for (i, (a_id, a)) in rows.iter().enumerate() {
        for (b_id, b) in rows.iter().skip(i + 1) {
            let overlaps = a.origin.x < b.right()
                && b.origin.x < a.right()
                && a.origin.y < b.bottom()
                && b.origin.y < a.bottom();
            assert!(
                !overlaps,
                "{a_id} overlaps {b_id} ({a:?} vs {b:?}) — the layout-trap family"
            );
        }
    }
    rows.len()
}

// ---------------------------------------------------------------------------
// Cold start & landing (the attach-only rule + the empty state)
// ---------------------------------------------------------------------------

/// A hub whose instances hold no open session must land on the EMPTY state
/// — never mint a `session/new` tab (the user's call: the + button or
/// history). Once a session exists, the chat surface replaces the landing.
#[gpui_kit::test]
fn accept_cold_start_landing_and_first_session(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        cx.set_reduce_motion(true);
    });
    let note = "no open session on the hub yet — start one from the editor, the + \
                button, or pick one from history below";
    let mut snap = WorkbenchSnapshot::initial();
    snap.phase = Phase::Discovering;
    snap.hub_addr = Some("http://127.0.0.1:8377".to_string());
    snap.bootstrap_note = Some(note.to_string());
    let (handle, wb, _runtime) = cx.update(|cx| open(cx, snap, 600.));
    cx.update_window(handle.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        // The landing state, not a minted session.
        assert!(window.find("chat-empty-state").visible());
        assert!(
            window.find("status-bar-note").visible(),
            "the discovery note rides the status bar"
        );
        assert!(window.find("new-tab").visible(), "+ stays the way in");
    })
    .unwrap();

    // The hub now reports one session with a message: the same event path
    // the driver uses swaps the landing state for the conversation.
    let mut settled_tab = tab_view(false);
    settled_tab.messages = vec![protocol::chat::ChatMessage {
        id: "u1".to_string(),
        role: protocol::chat::ChatRole::User,
        parts: vec![protocol::chat::ChatPart::Text {
            text: "hello desktop".to_string(),
        }],
        created_at: 1,
        collapsed: None,
    }];
    let mut settled = connected_snapshot(Some(settled_tab));
    settled.hub_addr = Some("http://127.0.0.1:8377".to_string());
    advance(cx, &wb, settled);
    cx.update_window(handle.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        assert!(
            window.try_find("chat-empty-state").is_none(),
            "a landed session replaces the empty state"
        );
        assert!(window.find("send").visible(), "the composer is live");
        sweep_geometry(window);
    })
    .unwrap();
}

// ---------------------------------------------------------------------------
// C1 + C3: stop the running turn, work the pending queue
// ---------------------------------------------------------------------------

/// A running turn with a queued draft: the queue lists text + image-only
/// rows with their actions; Send now / Edit / Discard reach the runtime
/// with the right index, and Esc cancels the running turn (ChatView's
/// Cancel — driven through the bound handler; the headless harness cannot
/// deliver key bindings).
#[gpui_kit::test]
fn accept_running_turn_stop_and_pending_queue(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        cx.set_reduce_motion(true);
    });
    use crate::runtime::PendingDraftView;
    let mut tab = tab_view(true);
    tab.pending = vec![
        PendingDraftView {
            text: "first question".to_string(),
            images: 0,
        },
        PendingDraftView {
            text: String::new(),
            images: 2,
        },
    ];
    let snap = connected_snapshot(Some(tab));
    let (handle, wb, runtime) = cx.update(|cx| open(cx, snap, 600.));
    cx.update_window(handle.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        // Both queue rows render with their full action set (the tab is
        // connected and not replaying, so Send now is up).
        assert!(window.find(("pending-send", 0usize)).visible());
        assert!(window.find(("pending-edit", 1usize)).visible());
        assert!(window.find(("pending-discard", 1usize)).visible());

        window.click(("pending-send", 0usize), cx);
        window.click(("pending-edit", 1usize), cx);
        window.click(("pending-discard", 1usize), cx);
        assert!(sent(&runtime, |c| matches!(
            c,
            UiCommand::ForceSendPendingAt { index } if *index == 0
        )));
        assert!(sent(&runtime, |c| matches!(
            c,
            UiCommand::EditPending { index } if *index == 1
        )));
        assert!(sent(&runtime, |c| matches!(
            c,
            UiCommand::DiscardPending { index } if *index == 1
        )));

        // Esc on a running turn: session/cancel (C1).
        wb.update(cx, |wb, cx| wb.on_stop_turn(&StopTurn, window, cx));
        assert!(sent(&runtime, |c| matches!(c, UiCommand::CancelTurn)));
        sweep_geometry(window);
    })
    .unwrap();
}

// ---------------------------------------------------------------------------
// C2: the thought card
// ---------------------------------------------------------------------------

/// The thought card auto-opens while it is the turn's live tail
/// ("thinking"), auto-collapses once the answer settles ("thought"), and a
/// manual click toggles it permanently.
#[gpui_kit::test]
fn accept_thought_card_folding(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        cx.set_reduce_motion(true);
    });
    use protocol::chat::{ChatMessage, ChatPart, ChatRole};
    let messages = || {
        vec![ChatMessage {
            id: "a1".to_string(),
            role: ChatRole::Assistant,
            parts: vec![ChatPart::Thought {
                text: "weighing the options".to_string(),
            }],
            created_at: 1,
            collapsed: None,
        }]
    };

    // Streaming tail: the card is open.
    let mut streaming = tab_view(true);
    streaming.messages = messages();
    let (handle, wb, _runtime) = cx.update(|cx| open(cx, connected_snapshot(Some(streaming)), 600.));
    cx.update_window(handle.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        assert!(window.find("thought-a1").visible());
        assert!(
            window.find("thought-body-a1").visible(),
            "the live tail auto-opens"
        );
    })
    .unwrap();

    // The turn settles: auto-collapse (the label flips to "Thought").
    let mut settled = tab_view(false);
    settled.messages = messages();
    advance(cx, &wb, connected_snapshot(Some(settled)));
    cx.update_window(handle.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        assert!(window.find("thought-a1").visible());
        assert!(
            window.try_find("thought-body-a1").is_none(),
            "a settled thought auto-collapses"
        );
        // Manual toggle: click re-opens, click again closes — the manual
        // choice sticks over the auto rule.
        window.click("thought-a1", cx);
        window.draw(cx).clear(cx);
        assert!(window.find("thought-body-a1").visible());
        window.click("thought-a1", cx);
        window.draw(cx).clear(cx);
        assert!(window.try_find("thought-body-a1").is_none());
        sweep_geometry(window);
    })
    .unwrap();
}

// ---------------------------------------------------------------------------
// B: the new-session chooser
// ---------------------------------------------------------------------------

/// The chooser lists known projects (a row click creates the session),
/// switches into browse mode (directories walk, Start-here gated by
/// creatable), and every gesture dispatches with the exact workspace.
#[gpui_kit::test]
fn accept_new_session_chooser_browse_and_create(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        cx.set_reduce_motion(true);
    });
    let mut snap = connected_snapshot(None);
    snap.new_session = Some(NewSessionView {
        projects: Some(vec![
            HistoryProjectRow {
                workspace_path: "/w/alpha".to_string(),
                sessions: 2,
                last_active: 9,
            },
            HistoryProjectRow {
                workspace_path: "/w/beta".to_string(),
                sessions: 1,
                last_active: 5,
            },
        ]),
        error: None,
        browse: None,
        creating: None,
    });
    let (handle, _wb, runtime) = cx.update(|cx| open(cx, snap, 600.));
    cx.update_window(handle.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        assert!(window.find("ns-close").visible());
        assert!(window.find("ns-project-/w/alpha").visible());
        assert!(window.find("ns-project-/w/beta").visible());
        window.click("ns-project-/w/alpha", cx);
        assert!(sent(&runtime, |c| matches!(
            c,
            UiCommand::NewSessionCreate { workspace_path } if workspace_path == "/w/alpha"
        )));
        sweep_geometry(window);
    })
    .unwrap();

    // Browse mode: a walked directory with a parent, one subdirectory, and
    // a creatable root — walking in dispatches the resolved path and
    // Start-here creates for the walked directory.
    let mut browse = connected_snapshot(None);
    browse.new_session = Some(NewSessionView {
        projects: None,
        error: None,
        creating: None,
        browse: Some(FsBrowseView {
            path: "/w".to_string(),
            parent: Some("/".to_string()),
            entries: vec!["gamma".to_string()],
            creatable: true,
            truncated: false,
            loading: false,
            error: None,
        }),
    });
    let (handle, _wb, runtime) = cx.update(|cx| open(cx, browse, 600.));
    cx.update_window(handle.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        assert!(window.find("ns-parent").visible());
        assert!(window.find("ns-dir-/w/gamma").visible());
        assert!(window.find("ns-start-here").visible());
        window.click("ns-dir-/w/gamma", cx);
        assert!(sent(&runtime, |c| matches!(
            c,
            UiCommand::NewSessionBrowse {
                path: Some(p)
            } if p == "/w/gamma"
        )));
        window.click("ns-start-here", cx);
        assert!(sent(&runtime, |c| matches!(
            c,
            UiCommand::NewSessionCreate { workspace_path } if workspace_path == "/w"
        )));
        sweep_geometry(window);
    })
    .unwrap();
}

// ---------------------------------------------------------------------------
// Settings home: servers book + config entries + the workflow gate
// ---------------------------------------------------------------------------

/// The settings panel: the servers book renders (active remote, the local
/// row's switch back, two-click remove), every config entry is reachable,
/// the Workflows entry follows the gate verdict while the gate SWITCH row
/// never hides.
#[gpui_kit::test]
fn accept_settings_servers_entries_and_gate(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        cx.set_reduce_motion(true);
    });
    let mut snap = connected_snapshot(Some(tab_view(false)));
    snap.servers = vec![protocol::hub::SavedServer {
        id: "srv-1".to_string(),
        name: "Remote dev box".to_string(),
        hub_url: "https://hub.example:8377".to_string(),
        token: "tok".to_string(),
    }];
    snap.active_server_id = Some("srv-1".to_string());
    snap.workflow_gate = Some(protocol::settings::WorkflowGateBlock {
        enabled: false,
        mode: "env".to_string(),
        source: "remote".to_string(),
        r#override: None,
    });
    snap.quota_card = QuotaCardView {
        usage: Some(quota_usage_fixture()),
        quota_unavailable: false,
        usage_stats_at: Some(1),
    };
    // Tall window: the config panel scrolls and only painted rows register
    // ids — the whole entry list must fit one frame.
    let (handle, wb, runtime) = cx.update(|cx| open(cx, snap, 2400.));
    cx.update_window(handle.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        window.click("dock-rail-config", cx);
        window.draw(cx).clear(cx);

        // Servers book: the active remote renders with a (two-click) remove,
        // the local row offers its switch back.
        assert!(window.find("server-remove-srv-1").visible());
        assert!(window.find("server-local-switch").visible());

        // Edit (TS saveServer): the row's Edit prefills the shared form with
        // its own values; Save writes them back through UpdateServer and
        // drops back to the add form.
        window.click("server-edit-srv-1", cx);
        window.draw(cx).clear(cx);
        assert!(window.find("server-save").visible());
        assert!(window.find("server-edit-cancel").visible());
        window.click("server-save", cx);
        assert!(sent(&runtime, |c| matches!(
            c,
            UiCommand::UpdateServer { id, name, hub_url, token }
                if id == "srv-1"
                    && name == "Remote dev box"
                    && hub_url == "https://hub.example:8377"
                    && token == "tok"
        )));
        window.draw(cx).clear(cx);

        window.click("server-remove-srv-1", cx);
        window.draw(cx).clear(cx);
        assert_eq!(
            window.find("server-remove-srv-1").label().as_deref(),
            Some("Confirm remove"),
            "the first click arms the confirm window"
        );
        assert!(!sent(&runtime, |c| matches!(
            c,
            UiCommand::RemoveServer { .. }
        )));
        window.click("server-remove-srv-1", cx);
        assert!(sent(&runtime, |c| matches!(
            c,
            UiCommand::RemoveServer { id } if id == "srv-1"
        )));

        // Every config entry renders and opens its page.
        for entry in [
            "config-entry-Models",
            "config-entry-Usage statistics",
            "config-entry-Plan quota",
            "config-entry-System status",
            "config-entry-Skills",
            "config-entry-MCP servers",
            "config-entry-Hooks",
            "config-entry-Agents",
            "config-entry-Backups",
            "config-entry-App update",
        ] {
            assert!(
                window.find(entry).visible(),
                "{entry} must render in the settings panel"
            );
        }
        window.click("config-entry-Models", cx);
        assert!(sent(&runtime, |c| matches!(c, UiCommand::OpenModels)));

        // The hub staleness check: one POST /api/upgrade behind the button.
        window.click("hub-upgrade-check", cx);
        assert!(sent(&runtime, |c| matches!(c, UiCommand::UpgradeHub)));

        // The account quota card (TS SettingsPanel's QuotaSection): docking
        // the panel refreshed it once (the mount effect); every reported
        // provider section paints, and the refresh button re-reads.
        let refreshes = count(&runtime, |c| matches!(c, UiCommand::RefreshQuota));
        assert!(refreshes >= 1, "docking the config panel must refresh the quota card");
        assert!(window.find("quota-card").visible());
        assert!(window.find("quota-card-glm").visible());
        assert!(window.find("quota-card-go").visible());
        assert!(window.find("quota-card-ollama").visible());
        assert!(window.find("quota-card-cliproxy").visible());
        window.click("quota-card-refresh", cx);
        assert_eq!(
            count(&runtime, |c| matches!(c, UiCommand::RefreshQuota)),
            refreshes + 1
        );

        // Gate off: the Workflows ENTRY hides, the gate SWITCH row never does
        // (the override is the way in).
        assert!(window.try_find("config-entry-Workflows").is_none());
        assert!(window.find("config-entry-Workflow switch").visible());
    })
    .unwrap();

    // Gate on: the entry appears and opens the page.
    let mut gate_on = connected_snapshot(Some(tab_view(false)));
    gate_on.workflow_gate = Some(protocol::settings::WorkflowGateBlock {
        enabled: true,
        mode: "env".to_string(),
        source: "remote".to_string(),
        r#override: None,
    });
    advance(cx, &wb, gate_on);
    cx.update_window(handle.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        assert!(window.find("config-entry-Workflows").visible());
        window.click("config-entry-Workflows", cx);
        assert!(sent(&runtime, |c| matches!(c, UiCommand::OpenWorkflows)));
        sweep_geometry(window);
    })
    .unwrap();
}

/// C: Esc on an open config page is the settings screen's back button — the
/// page closes and the running turn is NOT cancelled; with the page gone the
/// next Esc is ChatView's cancel.
#[gpui_kit::test]
fn accept_config_page_esc_closes_before_cancel(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        cx.set_reduce_motion(true);
    });
    let skills_page = || ConfigSectionPageView {
        kind: ConfigPageKind::Skills,
        supported: Some(true),
        loading: false,
        error: None,
        payload: Some(serde_json::json!({ "ok": true, "skills": [] })),
        busy: false,
        models: None,
        update_channel: "stable".to_string(),
        install: None,
    };
    let mut snap = connected_snapshot(Some(tab_view(true)));
    snap.config_section_page = Some(skills_page());
    let (handle, wb, runtime) = cx.update(|cx| open(cx, snap, 600.));
    cx.update_window(handle.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        wb.update(cx, |wb, cx| wb.on_stop_turn(&StopTurn, window, cx));
        assert!(sent(&runtime, |c| matches!(
            c,
            UiCommand::CloseConfigPage { page: ConfigPageKind::Skills }
        )));
        assert!(
            !sent(&runtime, |c| matches!(c, UiCommand::CancelTurn)),
            "Esc must close the page, not cancel the running turn"
        );
    })
    .unwrap();

    // Page closed: the same Esc is the turn's cancel.
    advance(cx, &wb, connected_snapshot(Some(tab_view(true))));
    cx.update_window(handle.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        wb.update(cx, |wb, cx| wb.on_stop_turn(&StopTurn, window, cx));
        assert!(sent(&runtime, |c| matches!(c, UiCommand::CancelTurn)));
    })
    .unwrap();
}

/// The right session panel (TS SessionPanel): the toggle in the tab strip
/// opens it and refreshes the quota card (the mount effect); the session's
/// config options render as pickers, its turn readouts (context / cache /
/// speed) spell out the title bar's segments, the gated workflows entry
/// opens the page, and close takes the panel away again.
#[gpui_kit::test]
fn accept_session_panel_stats_config_and_quota(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        cx.set_reduce_motion(true);
    });
    use protocol::acp::{ConfigOption, ConfigOptionChoice};
    let mut tab = tab_view(false);
    tab.usage = Some(protocol::acp::ContextUsage { used: 35_000, size: 200_000 });
    tab.cache_hit = Some(82);
    tab.turn_stats = Some(protocol::chat::TurnStats {
        tps: 41.0,
        output_tokens: 820,
        seconds: 20.0,
    });
    tab.config_options = vec![ConfigOption {
        id: "model".to_string(),
        name: Some("Model".to_string()),
        category: None,
        current_value: Some("glm-4.7".to_string()),
        options: Some(vec![
            ConfigOptionChoice {
                value: "glm-4.7".to_string(),
                name: "GLM-4.7".to_string(),
            },
            ConfigOptionChoice {
                value: "glm-4-air".to_string(),
                name: "GLM-4-Air".to_string(),
            },
        ]),
    }];
    let mut snap = connected_snapshot(Some(tab));
    snap.workflow_gate = Some(protocol::settings::WorkflowGateBlock {
        enabled: true,
        mode: "env".to_string(),
        source: "remote".to_string(),
        r#override: None,
    });
    snap.quota_card = QuotaCardView {
        usage: Some(quota_usage_fixture()),
        quota_unavailable: false,
        usage_stats_at: Some(1),
    };
    let (handle, _wb, runtime) = cx.update(|cx| open(cx, snap, 900.));
    cx.update_window(handle.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        assert!(
            window.try_find("session-panel").is_none(),
            "the panel starts closed"
        );
        window.click("session-panel-toggle", cx);
        window.draw(cx).clear(cx);
        assert!(window.find("session-panel").visible());
        // Opening refreshed the quota card (the TS mount effect).
        assert!(count(&runtime, |c| matches!(c, UiCommand::RefreshQuota)) >= 1);
        // Config picker rows + the spelled-out turn readouts.
        assert!(window.find("session-cfg-model").visible());
        assert!(window.find("session-panel-stats").visible());
        // The quota card rides along.
        assert!(window.find("quota-card").visible());
        assert!(window.find("quota-card-glm").visible());

        // The gated workflows entry opens the management page.
        assert!(window.find("session-panel-workflows").visible());
        window.click("session-panel-workflows", cx);
        assert!(sent(&runtime, |c| matches!(c, UiCommand::OpenWorkflows)));

        window.click("session-panel-close", cx);
        window.draw(cx).clear(cx);
        assert!(window.try_find("session-panel").is_none());
        sweep_geometry(window);
    })
    .unwrap();
}

// ---------------------------------------------------------------------------
// The three account pages: Models / Usage / Quota
// ---------------------------------------------------------------------------

/// Models lists its rows and a row opens the edit form; Usage renders both
/// tabs and the pill switch dispatches; Quota renders the GLM section body.
#[gpui_kit::test]
fn accept_models_usage_quota_pages(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        cx.set_reduce_motion(true);
    });
    use protocol::config_decode::{ModelRef, ModelsBlock, ModelsPayload};
    use protocol::quota::QuotaItem;

    // Models: one selectable row; clicking it opens the model form.
    let mut models = connected_snapshot(None);
    models.models_page = Some(ModelsPageView {
        supported: Some(true),
        loading: false,
        error: None,
        models: Some(ModelsPayload {
            ok: Some(true),
            models: Some(ModelsBlock {
                available: Some(vec![ModelRef {
                    provider_id: Some("bigmodel".to_string()),
                    provider_name: Some("BigModel".to_string()),
                    model_id: Some("glm-4.7".to_string()),
                }]),
                providers: None,
                model_rules: None,
            }),
        }),
        busy: false,
    });
    let (handle, _wb, _runtime) = cx.update(|cx| open(cx, models, 600.));
    cx.update_window(handle.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        assert!(window.find("models-list").visible());
        assert!(window.find("model-row-0-bigmodel/glm-4.7").visible());
        window.click("model-row-0-bigmodel/glm-4.7", cx);
        window.draw(cx).clear(cx);
        assert!(window.find("model-form-body").visible(), "a row opens the form");
        sweep_geometry(window);
    })
    .unwrap();

    // Usage: both tab pills render; switching to the local tab dispatches.
    let mut usage = connected_snapshot(None);
    usage.usage_page = Some(UsagePageView {
        tab: UsageTab::Platform,
        supported: Some(true),
        loading: false,
        local: None,
        local_range: "7d".to_string(),
        platform: None,
        platform_range: "7d".to_string(),
        platform_supported: true,
        platform_error: None,
    });
    let (handle, _wb, runtime) = cx.update(|cx| open(cx, usage, 600.));
    cx.update_window(handle.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        assert!(window.find("usage-tab-platform").visible());
        assert!(window.find("usage-tab-local").visible());
        assert!(
            window.find("usage-platform-range-7d").visible(),
            "the range pills ride the platform tab"
        );
        window.click("usage-tab-local", cx);
        assert!(sent(&runtime, |c| matches!(
            c,
            UiCommand::UsageTab { platform: false }
        )));
        sweep_geometry(window);
    })
    .unwrap();

    // Quota: the GLM section renders its window rows.
    let mut quota = connected_snapshot(None);
    quota.quota_page = Some(QuotaPageView {
        supported: Some(true),
        loading: false,
        error: None,
        quota_unavailable: false,
        glm_kind: Some("coding".to_string()),
        glm_items: vec![QuotaItem {
            key: "token_5h".to_string(),
            label: "5h".to_string(),
            used_percent: 42.0,
            left_percent: Some(58.0),
            used_count: None,
            total_count: None,
            next_reset_time: None,
            detail: None,
        }],
        usage_stats_at: Some(1),
        eligible: Vec::new(),
        credentials: true,
        provider_id: None,
        cards: None,
        nonce: None,
        busy: false,
        next_try_at: None,
        now_ms: 0,
    });
    let (handle, _wb, _runtime) = cx.update(|cx| open(cx, quota, 600.));
    cx.update_window(handle.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        assert!(window.find("quota-body").visible());
        sweep_geometry(window);
    })
    .unwrap();
}

// ---------------------------------------------------------------------------
// F3: the file viewer's local affordances
// ---------------------------------------------------------------------------

/// The viewer's Open-with-default / Show-in-Finder buttons exist only for a
/// loopback hub with a listing root (the file must live on this machine);
/// a remote hub renders neither. Never clicked — both shell out to `open`.
#[gpui_kit::test]
fn accept_file_viewer_local_buttons_follow_loopback(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        cx.set_reduce_motion(true);
    });
    use crate::file_browser::FileKind;
    use crate::runtime::{FileViewerView, FilesPanelView};
    let viewer = FileViewerView {
        image: None,
        path: "README.md".to_string(),
        name: "README.md".to_string(),
        size: 128,
        kind: FileKind::Text,
        first_line: Some(1),
        text: Some("line one".to_string()),
        loading: false,
        error: None,
    };
    let files = FilesPanelView {
        path: String::new(),
        listing: Some(FsListing {
            root: "/Users/me/proj".to_string(),
            entries: vec![FsEntry {
                name: "README.md".to_string(),
                kind: FsEntryKind::File,
                size: 128,
                mtime: Some(1),
            }],
            truncated: false,
        }),
        loading: false,
        error: None,
        viewing: Some(viewer),
    };

    // Loopback hub: both affordances render.
    let mut local = connected_snapshot(Some(tab_view(false)));
    local.hub_addr = Some("http://127.0.0.1:8377".to_string());
    local.files = files.clone();
    let (handle, _wb, _runtime) = cx.update(|cx| open(cx, local, 600.));
    cx.update_window(handle.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        assert!(window.find("file-viewer").visible());
        assert!(window.find("file-viewer-open").visible());
        assert!(window.find("file-viewer-reveal").visible());
        sweep_geometry(window);
    })
    .unwrap();

    // Remote hub: neither — the file is not on this machine.
    let mut remote = connected_snapshot(Some(tab_view(false)));
    remote.hub_addr = Some("https://hub.example:8377".to_string());
    remote.files = files;
    let (handle, _wb, _runtime) = cx.update(|cx| open(cx, remote, 600.));
    cx.update_window(handle.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        assert!(window.find("file-viewer").visible());
        assert!(window.try_find("file-viewer-open").is_none());
        assert!(window.try_find("file-viewer-reveal").is_none());
        sweep_geometry(window);
    })
    .unwrap();
}

// ---------------------------------------------------------------------------
// The geometry walk (paint gate, layer A): the workflows page's densest
// surfaces under real layout
// ---------------------------------------------------------------------------

use protocol::workflow::{
    WorkflowDetailResponse, WorkflowEntry, WorkflowHubGroup, WorkflowOverviewResponse,
    WorkflowRunRow, WorkflowScope,
};

fn zone_run(run_id: &str, name: &str, status: &str, session: Option<&str>) -> WorkflowRunRow {
    WorkflowRunRow {
        run_id: run_id.to_string(),
        name: Some(name.to_string()),
        status: status.to_string(),
        stop_reason: None,
        created_at: Some(1),
        updated_at: Some(1),
        spent_tokens: Some(1200),
        parent_session_id: None,
        acp_session_id: session.map(str::to_string),
        tool_call_id: None,
        args: None,
        cwd: None,
        artifacts: None,
        resumed_from: None,
        superseded_by: None,
        owner_instance_id: Some("inst-a".to_string()),
    }
}

fn zone_page(detail: Option<WorkflowDetailView>) -> WorkflowsPageView {
    let mut last_runs = std::collections::BTreeMap::new();
    last_runs.insert(
        "deploy".to_string(),
        zone_run("run-r1", "deploy", "completed", Some("s3")),
    );
    WorkflowsPageView {
        loading: false,
        error: None,
        overview: Some(WorkflowOverviewResponse {
            ok: true,
            groups: vec![WorkflowHubGroup {
                instance_id: "inst-a".to_string(),
                scope: WorkflowScope::Global,
                workspace: String::new(),
                workflows: vec![WorkflowEntry {
                    name: "deploy".to_string(),
                    description: Some("Deploy the app".to_string()),
                    scope: Some("global".to_string()),
                }],
                invalid: Vec::new(),
                last_runs,
                error: None,
            }],
            active_runs: vec![
                zone_run("run-a1", "deploy", "running", Some("s1")),
                zone_run("run-a2", "audit", "pending", Some("s2")),
            ],
            recent_runs: vec![
                zone_run("run-r1", "deploy", "completed", Some("s3")),
                zone_run("run-r2", "audit", "cancelled", None),
            ],
        }),
        detail,
        run: None,
        busy: false,
        action_instance: Some("inst-a".to_string()),
        launches: std::collections::HashMap::new(),
        session_runs: std::collections::HashMap::new(),
    }
}

fn zone_detail() -> WorkflowDetailView {
    WorkflowDetailView {
        scope: WorkflowScope::Global,
        name: "deploy".to_string(),
        instance_id: "inst-a".to_string(),
        loading: false,
        detail: Some(WorkflowDetailResponse {
            ok: true,
            name: "deploy".to_string(),
            path: Some("/tmp/deploy.js".to_string()),
            scope: WorkflowScope::Global,
            meta: None,
            script: Some("export default {}".to_string()),
        }),
        runs: Some(vec![
            zone_run("run-r1", "deploy", "completed", Some("s3")),
            zone_run("run-a1", "deploy", "running", Some("s1")),
        ]),
        summaries: std::collections::HashMap::new(),
    }
}

/// The zones list first (two live rows, two finished — one sessionless),
/// then the detail page (runs block + clear-finished): every observed
/// element lays out non-zero and inside the viewport, zone rows are
/// strictly disjoint, and the clear-finished control sits ABOVE the first
/// run row (TS renders it first inside the runs block).
#[gpui_kit::test]
fn accept_geometry_sweep_workflow_zones_and_detail(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        cx.set_reduce_motion(true);
    });

    // The overview: live + finished zones plus a group card.
    let mut snap = connected_snapshot(None);
    snap.workflows_page = Some(zone_page(None));
    let (handle, wb, _runtime) = cx.update(|cx| open(cx, snap, 900.));
    cx.update_window(handle.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        sweep_geometry(window);
        let active = rows_disjoint(window, "wf-active-");
        assert!(
            active >= 4,
            "two live rows with Detail+Stop each, got {active} observed"
        );
        let recent = rows_disjoint(window, "wf-recent-");
        assert!(
            recent >= 3,
            "finished rows with their controls, got {recent} observed"
        );
    })
    .unwrap();

    // The detail page: the runs block with clear-finished above its rows.
    let mut detail = connected_snapshot(None);
    detail.workflows_page = Some(zone_page(Some(zone_detail())));
    advance(cx, &wb, detail);
    cx.update_window(handle.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        sweep_geometry(window);
        let rows = rows_disjoint(window, "wf-detail-");
        assert!(
            rows >= 5,
            "two history rows with open/session/dismiss controls, got {rows} observed"
        );
        let clear = window.find("wf-clear-finished").bounds();
        let first_row = window.find("wf-detail-open-run-r1").bounds();
        assert!(
            clear.bottom() <= first_row.origin.y,
            "clear-finished must sit above the first run row ({clear:?} vs {first_row:?})"
        );
    })
    .unwrap();
}
