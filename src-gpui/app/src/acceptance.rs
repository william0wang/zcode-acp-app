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
    FsBrowseView, HistoryProjectRow, ModelsPageView, NewSessionView, Phase, ProtocolRuntime,
    QuotaPageView, RuntimeEvent, TabPhase, TabSummary, TabView, UsagePageView, UsageTab,
    UiCommand, WorkbenchSnapshot,
};
use crate::workbench::{StopTurn, Workbench};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    px, AnyWindowHandle, App, AppContext as _, Bounds, Entity, Point, Size, TestAppContext,
    WindowBounds, WindowOptions,
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
        ] {
            assert!(
                window.find(entry).visible(),
                "{entry} must render in the settings panel"
            );
        }
        window.click("config-entry-Models", cx);
        assert!(sent(&runtime, |c| matches!(c, UiCommand::OpenModels)));

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
    })
    .unwrap();
}
