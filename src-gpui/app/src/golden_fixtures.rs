//! Canned snapshots for the OS-level golden-screenshot stage
//! (`scripts/acceptance.sh`): `--golden-fixture <name>` renders one of
//! these through the REAL Workbench with an inert runtime — no driver, no
//! network, fixed window frame, deterministic strings — so the capture
//! script can diff the pixels against `scripts/golden/<name>.png`.
//!
//! Every fixture is a SETTLED state (no running badges, no streaming
//! tails): animated bits would make the captures non-deterministic.

use crate::runtime::{
    Phase, TabPhase, TabSummary, TabView, WorkbenchSnapshot, WorkflowsPageView,
};
use gpui_kit::{px, Bounds, Point, Size};

/// The pinned capture frame (screen points): the capture script's
/// `screencapture -R` region must match exactly.
pub const GOLDEN_WINDOW: Bounds<gpui_kit::Pixels> = Bounds {
    origin: Point::new(px(120.), px(120.)),
    size: Size {
        width: px(1200.),
        height: px(800.),
    },
};

/// Every fixture the capture script walks — keep in lockstep with
/// `scripts/acceptance.sh`'s golden stage.
pub const NAMES: &[&str] = &["chat", "workflows", "settings"];

/// The snapshot for `name`, or `None` for an unknown name (the caller
/// exits with the list).
pub fn snapshot(name: &str) -> Option<WorkbenchSnapshot> {
    let mut snap = WorkbenchSnapshot::initial();
    snap.phase = Phase::Connected;
    snap.hub_addr = Some("http://127.0.0.1:8377".to_string());
    snap.tabs = vec![TabSummary {
        id: 7,
        session_id: Some("sess_1".to_string()),
        phase: TabPhase::Connected,
        running: false,
        pending: 0,
        awaiting: false,
    }];
    snap.active_tab = Some(7);
    match name {
        "chat" => snap.active = Some(chat_tab()),
        "workflows" => snap.workflows_page = Some(workflows_page()),
        "settings" => {
            snap.active = Some(chat_tab());
            snap.servers = vec![protocol::hub::SavedServer {
                id: "srv-1".to_string(),
                name: "Remote dev box".to_string(),
                hub_url: "https://hub.example:8377".to_string(),
                token: "tok".to_string(),
            }];
            snap.active_server_id = Some("srv-1".to_string());
            snap.workflow_gate = Some(protocol::settings::WorkflowGateBlock {
                enabled: true,
                mode: "env".to_string(),
                source: "remote".to_string(),
                r#override: None,
            });
        }
        _ => return None,
    }
    Some(snap)
}

/// Whether the fixture wants the config dock panel open at paint time
/// (main.rs drives the same toggle the rail button does).
pub fn wants_config_dock(name: &str) -> bool {
    name == "settings"
}

fn chat_tab() -> TabView {
    TabView {
        session_id: Some("sess_1".to_string()),
        agent_version: Some("1.2.3".to_string()),
        phase: TabPhase::Connected,
        running: false,
        loading_session: false,
        messages: vec![
            protocol::chat::ChatMessage {
                id: "u1".to_string(),
                role: protocol::chat::ChatRole::User,
                parts: vec![protocol::chat::ChatPart::Text {
                    text: "golden fixture: does the desktop paint?".to_string(),
                }],
                created_at: 1,
                collapsed: None,
            },
            protocol::chat::ChatMessage {
                id: "a1".to_string(),
                role: protocol::chat::ChatRole::Assistant,
                parts: vec![protocol::chat::ChatPart::Text {
                    text: "It does — this frame is a canned snapshot rendered \
                           by the real workbench, captured and diffed by the \
                           acceptance script."
                        .to_string(),
                }],
                created_at: 2,
                collapsed: None,
            },
        ],
        pending: Vec::new(),
        has_more: false,
        total_messages: Some(2),
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

fn workflows_page() -> WorkflowsPageView {
    use protocol::workflow::{
        WorkflowEntry, WorkflowHubGroup, WorkflowOverviewResponse, WorkflowRunRow, WorkflowScope,
    };
    let row = |run_id: &str, name: &str, status: &str, session: Option<&str>| WorkflowRunRow {
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
    };
    let mut last_runs = std::collections::BTreeMap::new();
    last_runs.insert(
        "deploy".to_string(),
        row("run-r1", "deploy", "completed", Some("s3")),
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
            active_runs: vec![row("run-a1", "deploy", "running", Some("s1"))],
            recent_runs: vec![
                row("run-r1", "deploy", "completed", Some("s3")),
                row("run-r2", "audit", "cancelled", None),
            ],
        }),
        detail: None,
        run: None,
        busy: false,
        action_instance: Some("inst-a".to_string()),
        launches: std::collections::HashMap::new(),
        session_runs: std::collections::HashMap::new(),
    }
}
