//! Pure protocol layer for the ZCode ACP desktop client ("Desktop
//! Workbench") — no UI dependencies by contract (see
//! `.zcode/docs/desktop-client-requirements.md` §5 and
//! `docs/adr/0009-desktop-client-as-gpui-product-line.md`).
//!
//! This crate is the Rust port of the TS app's `src/lib/types.ts` domain and
//! message types, plus the pure logic the TS side unit-tests (model-value
//! fallback, workflow-run event rendering). Where the TS type and the server
//! (`zcode-acp-server` src) disagree, the SERVER wins; every such divergence
//! is recorded in `tests/contract.rs` (the contract tests' fixture comments).
//!
//! Wire doctrine (inherited from `src/lib/types.ts`): ACP payloads are
//! deliberately permissive — unknown kinds and fields must be ignored, never
//! rejected. Optional TS fields round-trip as omitted keys (`skip_serializing_if`),
//! nullable TS fields (`T | null`) serialize back as JSON `null`.
//!
//! Module map:
//! - [`acp_client`] — the ACP-over-WebSocket connection: connect + bearer
//!   auth + initialize (Desktop Presence clientInfo), session/new &
//!   session/load wire shapes, streaming session/update dispatch
//!   (`src/lib/acp.ts`)
//! - [`reconnect`] — the connection supervisor: connState machine,
//!   exponential backoff, wake liveness probe, wedge watchdog (the
//!   connection slice of `src/store/appStore.ts`)
//! - [`clock`] — injected time (real + mock) for every connection timer
//! - [`http`] — the injectable HTTP transport + query helpers (`fetch`)
//! - [`hub_client`] — the hub REST client over [`http`] (`src/lib/hub.ts`)
//! - [`ws`] — the injectable WebSocket transport + tungstenite factory
//! - [`bootstrap`] — Hub Bootstrap: local config read, detached hub spawn,
//!   EADDRINUSE race adjudication, guided token setup (desktop-only logic)
//! - [`hub`] — hub discovery/history/fs REST payloads, connection profiles
//! - [`acp`] — ACP `session/update` wire types and content-block helpers
//! - [`approval`] — approval card semantics: the three card shapes (Plan /
//!   Tool / Question), `session/request_permission` / `elicitation/create`
//!   parsing, context matched by `toolCallId`, and the Permission Race state
//!   machine (the permission/elicitation slices of `src/store/appStore.ts`)
//! - [`toast`] — ephemeral toast semantics (`tests/notify-toast.test.ts`)
//! - [`chat`] — the internal chat model (messages, parts, drafts, turn stats)
//! - [`replay`] — replay projection helpers: session/update -> chat model,
//!   replay pagination metadata, the per-session replay cache
//!   (`src/store/replay.ts`)
//! - [`session`] — the session data-semantics state machine on top of
//!   [`replay`]: tail catch-up (session/load + cache reconcile),
//!   load_earlier pagination, the pending prompt queue, the cache snapshot
//!   race (the session slice of `src/store/appStore.ts`)
//! - [`settings`] — Settings API (models/mcp/hooks/agents + usage/quota pages)
//! - [`settings_client`] — the Settings API client over an injectable HTTP
//!   transport (reads/writes, effect classes, backend restart)
//! - [`config_decode`] — the config screens' pure decode/encode helpers
//! - [`config_store`] — configuration screen state (reset-card session,
//!   pending-restart flag, hub-switch reset)
//! - [`quota`] — `account/usage_stats` / `/api/quota` combined sections
//! - [`workflow`] — dynamic-workflow management payloads
//! - [`system_stats`] — `/api/system-stats` machine status
//! - [`model_value`] — configOption model-id fallback (`src/lib/modelValue.ts`)
//! - [`workflow_args`] — launch-args declaration parsing + the typed form's
//!   report computation (`src/lib/workflow-args.ts`, `WorkflowArgsForm.tsx`)
//! - [`workflow_run_event_text`] — journal event rendering + slice folding
//!   (`src/components/config/workflowRunEventText.ts`)
//! - [`workflow_run_detail`] — the run-detail view's pure halves: phase fold
//!   + slicing, workspace row text, node-result shape, preset artifact
//!   models (`WorkflowRunDetail.tsx`, `WorkflowArtifactViews.tsx`)
//! - [`image`] — client-side attachment downsampling/re-encoding
//!   (`src/lib/image.ts`, ADR 0007): pick/paste/drop → ACP image block

pub mod acp;
pub mod acp_client;
pub mod approval;
pub mod bootstrap;
pub mod chat;
pub mod clock;
pub mod config_decode;
pub mod config_store;
pub mod hub;
pub mod hub_client;
pub mod http;
pub mod image;
pub mod model_value;
pub mod profiles;
pub mod quota;
pub mod reconnect;
pub mod replay;
pub mod session;
pub mod settings;
pub mod settings_client;
pub mod system_stats;
pub mod terminal;
pub mod toast;
pub mod workflow;
pub mod workflow_args;
pub mod workflow_run_detail;
pub mod workflow_run_event_text;
pub mod ws;

pub use acp::{
    content_diff_blocks, content_text, AcpDiffContent, AcpTextContent, ConfigOption, ContextUsage,
    SessionUpdate, SlashCommand,
};
pub use approval::{
    permission_shape, ApprovalContext, ApprovalCore, ApprovalShape, ElicitAnswers, ElicitField,
    ElicitOption, ElicitValue, IncomingAsk, PendingAsk, PendingElicitation, PendingPermission,
    PermissionOption,
};
pub use model_value::bare_model_id_from_config_value;
pub use replay::{
    bridge_supports_page_marker, read_replay_meta, snapshot_tail_id, EARLIER_PAGE_LIMIT,
    REPLAY_TAIL_LIMIT,
};
pub use session::{SessionCore, WireRequest};
