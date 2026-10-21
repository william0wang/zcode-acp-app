import { create } from "zustand";
import { AcpConnection } from "../lib/acp";
import { HubApiError, HubClient } from "../lib/hub";
import { rememberLaunch } from "../lib/workflow-launches";
import {
  defaultServerName,
  loadFontSize,
  loadLang,
  loadPending,
  loadServerBook,
  loadUpdateChannel,
  newServerId,
  saveFontSize,
  saveLang,
  savePending,
  saveServerBook,
  type FontSize,
  type Lang,
} from "../lib/storage";
import {
  configSliceState,
  createConfigSlice,
  seedUpdateChannel,
  type ConfigSlice,
} from "./configSlice";
import {
  applyOne,
  appendTextPart,
  bridgeSupportsPageMarker,
  EARLIER_PAGE_LIMIT,
  findToolCallPart,
  isEarlierPageMeta,
  isLocalMessageId,
  newMessage,
  readReplayMeta,
  readSessionCache,
  removeSessionCache,
  REPLAY_TAIL_LIMIT,
  snapshotTailId,
  writeSessionCache,
  type PlanEntry,
} from "./replay";
import {
  type AccountUsageStats,
  type AttachmentDraft,
  type ChatMessage,
  type ChatPart,
  type CliproxyUsageStats,
  type ConfigOption,
  type ConnectionProfile,
  type ContextUsage,
  type CpWindowEntry,
  type FsListing,
  type GoUsageStats,
  type GoWindowEntry,
  type GlmUsageStats,
  type HubCreateInstanceResult,
  type HubInstance,
  type HubSessionInfo,
  type HubUpgradeResult,
  type OcWindowEntry,
  type OllamaUsageStats,
  type PromptDraft,
  type QuotaItem,
  type SavedServer,
  type SessionUpdate,
  type SlashCommand,
  type TurnStats,
  type ConversationRunSummary,
  type WorkflowHubGroup,
  type WorkflowRunRow,
  type WorkflowScope,
  type AmendRunSettingsResponse,
  type WorkflowStartResponse,
} from "../lib/types";

export type ConnState = "idle" | "connecting" | "open" | "reconnecting";

export type { PlanEntry } from "./replay";
export type { ConfigSection } from "./configSlice";

// Last-turn prompt-cache hit rate (percent) from a session/prompt response
// usage: cachedRead / (input + cachedRead + cachedWrite), the ACP de-facto
// convention. inputTokens is already cache-exclusive — the bridge normalizes
// the backend's OpenAI-style inclusive counts. Null when the turn reported
// no cache numbers at all (caller keeps the previous rate then).
function cacheHitRate(u: unknown): number | null {
  const v = u as {
    inputTokens?: unknown;
    cachedReadTokens?: unknown;
    cachedWriteTokens?: unknown;
  } | null;
  if (!v || typeof v !== "object") return null;
  if (typeof v.cachedReadTokens !== "number") return null;
  const input = typeof v.inputTokens === "number" ? v.inputTokens : 0;
  const write = typeof v.cachedWriteTokens === "number" ? v.cachedWriteTokens : 0;
  const denom = input + v.cachedReadTokens + write;
  return denom > 0 ? Math.round((v.cachedReadTokens / denom) * 100) : null;
}

// Validates an account/usage_stats result into the combined GLM + Opencode
// Go + Ollama Cloud shape; null when the payload doesn't fit (treated like a
// failed fetch). glm/opencode stay required, ollama is optional (older hubs).
function parseUsageStats(result: unknown): AccountUsageStats | null {
  if (typeof result !== "object" || result === null) return null;
  const r = result as {
    glm?: unknown;
    opencode?: unknown;
    ollama?: unknown;
    cliproxy?: unknown;
  };
  if (typeof r.glm !== "object" || r.glm === null) return null;
  if (typeof r.opencode !== "object" || r.opencode === null) return null;

  const glm = r.glm as Partial<GlmUsageStats>;
  const go = r.opencode as Partial<GoUsageStats>;
  const oc =
    typeof r.ollama === "object" && r.ollama !== null
      ? (r.ollama as Partial<OllamaUsageStats>)
      : undefined;
  const cp =
    typeof r.cliproxy === "object" && r.cliproxy !== null
      ? (r.cliproxy as Partial<CliproxyUsageStats>)
      : undefined;
  const items = Array.isArray(glm.items)
    ? glm.items.filter((it) => it && typeof it.usedPercent === "number")
    : undefined;
  const windows = Array.isArray(go.windows)
    ? go.windows.filter((w) => w && typeof w.usagePercent === "number")
    : undefined;

  return {
    glm: {
      kind: (["success", "auth_error", "rate_limited", "unavailable"] as const).includes(
        glm.kind as GlmUsageStats["kind"],
      )
        ? (glm.kind as GlmUsageStats["kind"])
        : "unavailable",
      ...(typeof glm.level === "string" ? { level: glm.level } : {}),
      ...(items ? { items: items as QuotaItem[] } : {}),
    },
    opencode: {
      kind: (["success", "not_configured", "auth_error", "unavailable"] as const).includes(
        go.kind as GoUsageStats["kind"],
      )
        ? (go.kind as GoUsageStats["kind"])
        : "unavailable",
      ...(windows ? { windows: windows as GoWindowEntry[] } : {}),
    },
    // Ollama section is optional — pre-0.41.0 bridges don't send it.
    ...(oc
      ? {
          ollama: {
            kind: (["success", "not_configured", "auth_error", "unavailable"] as const).includes(
              oc.kind as OllamaUsageStats["kind"],
            )
              ? (oc.kind as OllamaUsageStats["kind"])
              : "unavailable",
            ...(Array.isArray(oc.windows)
              ? {
                  windows: oc.windows.filter(
                    (w) => w && typeof w.usagePercent === "number",
                  ) as OcWindowEntry[],
                }
              : {}),
          },
        }
      : {}),
    // CLIProxy section is optional — pre-0.66.0 bridges don't send it. The
    // pool-wide `combined` rows are what the card renders; the per-account
    // breakdown stays on the CLI like GLM's per-model detail.
    ...(cp
      ? {
          cliproxy: {
            kind: (["success", "not_configured", "auth_error", "unavailable"] as const).includes(
              cp.kind as CliproxyUsageStats["kind"],
            )
              ? (cp.kind as CliproxyUsageStats["kind"])
              : "unavailable",
            ...(Array.isArray(cp.combined)
              ? {
                  combined: cp.combined.filter(
                    (w) => w && typeof w.usedPercent === "number",
                  ) as CpWindowEntry[],
                }
              : {}),
            ...(typeof cp.otherAccounts === "number"
              ? { otherAccounts: cp.otherAccounts }
              : {}),
          },
        }
      : {}),
  };
}

export interface PermissionOption {
  optionId: string;
  kind?: string;
  name?: string;
}

// Context resolved for the approval card. The request params alone carry
// almost nothing readable; the interesting text (plan, question) rides on the
// tool_call the bridge emits right before the request (ADR 0003).
export interface ApprovalContext {
  toolCallId?: string;
  toolName?: string;
  kind?: string;
  title?: string;
  // Plan text (ExitPlanMode) or question text (AskUserQuestion), from the
  // matched tool_call's content.
  detail?: string;
  plan?: string;
  rawInputText?: string;
}

export interface PendingPermission {
  requestId: number;
  sessionId: string;
  options: PermissionOption[];
  context?: ApprovalContext;
}

// One AskUserQuestion field parsed out of an elicitation/create form
// (bridge builds the schema: `q_<i>` enum/array + `q_<i>_other` free text;
// the plan-approval form is a single `approval` enum field).
export interface ElicitField {
  // Field key in requestedSchema (q_0, q_1, …) and its free-text companion.
  key: string;
  otherKey: string | null;
  question: string;
  // Long-form markdown under the question — the plan-approval form (bridge
  // 0.33+) carries the COMPLETE plan here, rendered as a markdown pane.
  description?: string;
  multi: boolean;
  options: { value: string; label: string }[];
}

export interface PendingElicitation {
  requestId: number;
  sessionId: string;
  message: string;
  fields: ElicitField[];
}

// Per-session display state for the session list. Bridge broadcasts fan out
// across ALL sessions of the instance, so non-active sessions are tracked
// too. Not persisted: after a reconnect other sessions read as idle until the
// next broadcast event.
export interface SessionActivity {
  running: boolean;
  awaitingPermission: boolean;
  finishedAt?: number;
}

export interface AppState extends ConfigSlice {
  profile: ConnectionProfile | null;
  // Multi-server book: every saved Hub URL + token pair and the active one.
  // `profile` mirrors the active entry (null when nothing is active).
  savedServers: SavedServer[];
  activeServerId: string | null;
  lang: Lang;
  fontSize: FontSize;
  instances: HubInstance[];
  instancesError: string | null;
  // Hub unreachable (editor exited — it re-spawns the hub on demand). Expected
  // state, rendered as a calm hint; polling continues until it returns.
  hubOffline: boolean;
  connState: ConnState;
  instanceId: string | null;
  activeSessionId: string | null;
  messages: ChatMessage[];
  planEntries: PlanEntry[] | null;
  isRunning: boolean;
  // Follow-ups typed while a turn is running or a replay is still loading;
  // flushed in order the moment the prompt settles / the session attaches.
  // Keyed by sessionId and persisted — drafts survive session switches and
  // app restarts.
  pendingPrompts: Record<string, PromptDraft[]>;
  // Pending permission/elicitation requests, keyed by the sessionId they
  // belong to (bridge 0.17.0 semantics: a request is only ever answered in
  // the session that raised it — dialogs render per-session, and a request
  // from session A must never appear in session B's view). One pending
  // request per session at a time: the bridge issues its interactions
  // sequentially within a turn.
  permissions: Record<string, PendingPermission>;
  // AskUserQuestion via elicitation/create (the bridge's preferred channel
  // once ANY client advertises elicitation.form — capabilities OR-merge).
  elicitations: Record<string, PendingElicitation>;
  notice: string | null;
  // Ephemeral one-shot feedback (downloads, copies, share results) with its
  // own auto-clear timer. Separate from `notice` because the banner only
  // renders on the picker/chat screens — toasts raised inside the z-50
  // full-screen overlays (FileBrowser/FileViewer) were invisible there,
  // which read as "download failed with no feedback at all".
  toast: { id: number; text: string } | null;
  // Pagination state from the attach response's replayMeta.
  replayCursor: string | null;
  hasMore: boolean;
  totalMessages: number | null;
  loadingEarlier: boolean;
  // Session config (model/mode/thought) + context usage, from the attach
  // response and config_option_update / current_mode_update / usage_update.
  configOptions: ConfigOption[];
  currentModeId: string | null;
  usage: ContextUsage | null;
  // Last turn's prompt-cache hit rate (percent), from the session/prompt
  // response usage. Null until a turn reports cache numbers.
  cacheHit: number | null;
  // Last turn's speed readout (desktop status bar): output tokens, wall
  // seconds, and their ratio. Null until a turn reports output tokens.
  turnStats: TurnStats | null;
  // Slash commands from available_commands_update (overwrite semantics),
  // driving the "/" completion menu in the composer.
  availableCommands: SlashCommand[];
  // Account-level quota (account/usage_stats): the combined GLM + Opencode
  // Go structure mirroring the zcode-quota CLI card, pulled after connect.
  usageStats: AccountUsageStats | null;
  // The hub's preformatted one-line quota dock (ADR-0021) — the exact string
  // the Martty TUI renders in its bottom dock. Null = no data; a failed
  // refresh keeps the last line (sticky, same contract as the TUI's).
  quotaDock: string | null;
  // Epoch ms of the last successful quota fetch — shown so the user can tell
  // whether a refresh actually landed.
  usageStatsAt: number | null;
  // Per-session running/permission state from bridge-wide broadcasts.
  sessionStates: Record<string, SessionActivity>;
  // Last quota fetch failed — keep the section header + Refresh visible so a
  // long-lived connection can still retry (REMOTE-CLIENTS: hide data, retry later).
  quotaUnavailable: boolean;
  // Bridge advertised session file access (initialize agentCapabilities
  // `_meta.zcode.fs`, server 0.7.0+); gates the file browser entry (ADR-0005).
  fsCapable: boolean;
  // Bridge marks load_earlier page updates `_meta.zcode.earlierPage`
  // (zcode-acp-server 0.44.1+); gates the marker-based page routing. Against
  // an older bridge the unmarked pages fall back to the pre-marker
  // buffer-everything collection.
  pageMarkerCapable: boolean;
  loadingSession: boolean;

  // Machine-level saved-workflow groups for the management page, served
  // pre-aggregated by the hub (/api/workflow-overview, bridge 0.60.0): one
  // global group + one per WORKSPACE (a project with two bridges renders
  // once — the hub dedupes), each with per-name last-run rows for badges.
  // A workspace whose every bridge failed carries `error` and shows as
  // unavailable instead of hiding the project.
  workflowHub: WorkflowHubGroup[] | null;
  // The same overview's machine-wide active-run list (journal rows with
  // status pending/running, owner-instance annotated by the hub). The
  // page's live zone renders exactly this — no per-instance queries.
  workflowActiveRuns: WorkflowRunRow[] | null;
  // The same overview's finished complement (terminal rows, newest first,
  // hub-capped) — the page's recently-finished list renders exactly this.
  workflowRecentRuns: WorkflowRunRow[] | null;
  // The overview load's own error (a hub too old to serve the route, or a
  // transport failure on the first load). Kept SEPARATE from configError:
  // the list-changed notification can refresh the hub while another section
  // is open, and that refresh must never stomp the other section's error.
  workflowHubError: string | null;
  workflowHubLoading: boolean;
  loadWorkflowHub: () => Promise<void>;
  // Bridge-side directory watch (bridge 0.57.0): the connected instance's
  // workflow lists changed by any means. Reloads only when the hub list is
  // already loaded — nobody is watching otherwise, and the workflows page's
  // own poll still covers active runs.
  refreshWorkflowHubIfLoaded: () => void;
  // Deep link: open the management page straight at one run's detail (a
  // session-panel run row tap). Consumed exactly once by the page's mount.
  workflowRunTarget: {
    sessionId: string;
    runId: string;
    name: string;
    instanceId: string;
  } | null;
  openWorkflowRun: (target: {
    sessionId: string;
    runId: string;
    name: string;
    instanceId: string;
  }) => void;
  clearWorkflowRunTarget: () => void;
  // Text (plus staged images) the next mounted composer adopts as its draft
  // (the workflow "create via conversation" entry, and queued drafts returned
  // for editing via editPending). Nonce-keyed so the same text twice still
  // reads as a new request; consumed exactly once.
  composerPrefill: { text: string; images?: AttachmentDraft[]; nonce: number } | null;
  setComposerPrefill: (text: string) => void;
  clearComposerPrefill: () => void;

  init: () => void;
  // Upserts by normalized hubUrl: an existing entry gets the new token and
  // becomes active; otherwise a new entry is appended (name defaults to the
  // URL host). Connecting NEVER drops the other saved servers.
  connectToHub: (input: { hubUrl: string; token: string; name?: string }) => void;
  // Tears the connection down and clears the active selection; the saved
  // server list itself is kept (the manager screen lists them).
  disconnectHub: () => void;
  // Switches the active server: full connection teardown, then fresh polling
  // against the target entry. No health gate — the hubOffline banner covers
  // a down hub and polling heals when it comes back.
  switchServer: (id: string) => void;
  // In-place edit of one saved entry; editing the ACTIVE one reconnects.
  saveServer: (entry: SavedServer) => void;
  // Removes one entry; deleting the ACTIVE one disconnects to the manager.
  deleteServer: (id: string) => void;
  // Full-screen server manager overlay (opened from settings). Browsing and
  // editing happens WITHOUT dropping the current connection; switching or
  // adding a server closes it (the chosen hub's session list takes over).
  manageOpen: boolean;
  openServerManager: () => void;
  closeServerManager: () => void;
  setLang: (lang: Lang) => void;
  setFontSize: (size: FontSize) => void;
  refreshInstances: (opts?: { probe?: boolean }) => Promise<void>;
  upgradeHub: () => Promise<HubUpgradeResult>;
  connectInstance: (
    instanceId: string,
    attachSessionId?: string,
    opts?: { noAttach?: boolean },
  ) => Promise<void>;
  // Remote session-create (bridge 0.17.0, ADR-0014): create/reuse a headless
  // serve bridge for a known project and open a FRESH session in it.
  // Resolves true only when the session is actually the active one — callers
  // cannot infer it from a route swap (the sheet may open from inside a
  // session too, where nothing unmounts).
  createProjectSession: (workspacePath: string) => Promise<boolean>;
  // Resume a CLOSED session from the project history listing (bridge 0.19.0,
  // ADR-0015): create/reuse the workspace's serve bridge (usually reused:true
  // with the listing's instance), then attach and session/load the store id —
  // the normal connect path replays the history. Resolves true only on a
  // successful attach (see createProjectSession).
  resumeProjectSession: (workspacePath: string, sessionId: string) => Promise<boolean>;
  openSession: (instanceId: string, sessionId: string) => Promise<void>;
  // Foreground wake hook: proves the WS pipe is actually alive (Android deep
  // sleep leaves zombie sockets that never fire onclose) and reconnects if
  // not. Registered at module level via visibilitychange.
  wakeProbe: () => void;
  // Returns to the session list. The instance connection stays open so
  // bridge broadcasts keep the list's activity badges live (running state
  // is broadcast-only — the hub's REST discovery carries no such field);
  // re-entering a session on the same instance reuses the socket.
  closeSession: () => void;
  loadSession: (sessionId: string) => Promise<void>;
  // Retires a session from remote discovery via the hub's HTTP close
  // endpoint (ADR-0006). Close, not delete — editor-side-still-open
  // conversations self-heal back on their next use. Refused (409 notice)
  // while a turn runs.
  closeRemoteSession: (instanceId: string, sessionId: string) => Promise<void>;
  // Soft-deletes (tombstones) a session from every listing (bridge ≥0.53,
  // server ADR-0031) — the history surface's row removal. Live/running
  // sessions are refused server-side (409 notice). Resolves true when the
  // tombstone landed; callers own their local row state.
  deleteSession: (instanceId: string, sessionId: string) => Promise<boolean>;
  // Soft-deletes (tombstones) a whole workspace (hub ≥0.53, server ADR-0031):
  // the project and all its sessions vanish from every listing; a new session
  // in it records a fresh row and brings it back. Resolves true on success;
  // callers own their local list state.
  deleteProject: (workspacePath: string) => Promise<boolean>;
  // Renames a session via the hub's HTTP rename endpoint (bridge 0.11.9).
  // The one-shot auto-title never revises a title; this is the manual path.
  // Updates the local list optimistically; the bridge broadcasts the change
  // to attached clients and the next discovery poll reconciles.
  renameSession: (instanceId: string, sessionId: string, title: string) => Promise<void>;
  // Terminates a remote-incubated bridge via the hub's instance shutdown
  // endpoint — the "close the session window" counterpart for app-created
  // instances. Editor-origin bridges are refused server-side (403).
  shutdownInstance: (instanceId: string) => Promise<void>;
  loadEarlier: () => Promise<boolean>;
  setConfigOption: (configId: string, value: string) => Promise<void>;
  refreshUsageStats: () => Promise<void>;
  // Refresh the title-bar quota dock line (hub /api/quota/dock). Sticky on
  // failure: a fetch error keeps the last line instead of blanking it.
  refreshQuotaDock: () => Promise<void>;
  // Session Files (ADR-0005): read-only browse/view over hub REST, scoped to
  // the active instance + session. Null = not connected / request failed.
  fsList: (path: string) => Promise<FsListing | null>;
  fsFileText: (
    path: string,
    line: number,
    limit: number,
  ) => Promise<{ firstLine: number; text: string } | null>;
  fsFileUrl: (path: string) => string | null;
  sendPrompt: (text: string, images?: AttachmentDraft[]) => Promise<void>;
  runPrompt: (draft: PromptDraft) => Promise<void>;
  // Interrupts the running turn so a queued follow-up goes out immediately.
  // With an index, the targeted draft moves to the front first — that
  // bubble's button sends THAT message, not the queue head.
  forceSendPending: (index?: number) => void;
  discardPending: (index: number) => void;
  // Returns a queued draft to the composer for editing: it leaves the queue
  // and re-enters the input as a prefill (text + staged images).
  editPending: (index: number) => void;
  cancelTurn: () => void;
  answerPermission: (requestId: number, optionId: string) => void;
  // Resolves a pending elicitation form: content answers it (accept), null
  // declines. Same first-response-wins race as permissions.
  answerElicitation: (requestId: number, content: Record<string, string | string[]> | null) => void;
  dismissNotice: () => void;
  // Ephemeral UI feedback (download progress, copy confirmations); renders
  // above every overlay and auto-clears.
  notify: (text: string) => void;
  dismissToast: () => void;

  // Runs one hub call with the connected client and the connected instance
  // bound in; notifies on failure and resolves null (no client, no instance,
  // or the call refused). Every workflow read/write the screens make goes
  // through here so the error posture lives in one place. `silent` serves
  // the run-detail poller: a dead session must not toast every 3s tick.
  // `onError` hands silent callers the bare failure message ("not connected"
  // or the thrown error's text) so a screen can show WHY alongside its own
  // empty/failed state instead of a toast.
  workflowAction: <T>(
    label: string,
    fn: (client: HubClient, instanceId: string) => Promise<T>,
    silent?: boolean,
    onError?: (message: string) => void,
    /** Target instance for cross-group actions (defaults to the connected one). */
    iidOverride?: string,
  ) => Promise<T | null>;
  // Launches a saved workflow; records the run in the local launch memory so
  // its rows stay actionable (open session, run detail, resume) later.
  // `sessionId` pins the run to an existing session (in-chat start) instead
  // of letting the bridge mint a fresh one.
  startWorkflow: (input: {
    scope: WorkflowScope;
    name: string;
    args?: Record<string, unknown>;
    sessionId?: string;
    /** Owning instance for a launch started from another instance's group. */
    instanceId?: string;
  }) => Promise<WorkflowStartResponse | null>;
  resumeWorkflowRun: (input: {
    runId: string;
    sessionId: string;
    name?: string;
    instanceId?: string;
  }) => Promise<boolean>;
  // Amend a flying run's settings (bridge 0.49.0). Returns the run that
  // continues the work — SAME id for in-place changes, a NEW id (plus
  // supersededRunId) when the amendment stopped and replaced the run; the
  // caller re-points its view on a differing id.
  amendWorkflowRun: (input: {
    runId: string;
    sessionId: string;
    body: { subagentModel?: string | null; maxConcurrency?: number | null };
    /** Owning instance for a run opened from another instance's group. */
    instanceId?: string;
  }) => Promise<AmendRunSettingsResponse | null>;
  // Stop a flying run (`session/cancelBackgroundTask`, taskId ≡ runId — the
  // same identity equation as resume/amend). With `instanceId` it rides the
  // hub route and reaches ANY instance's run; the ACP fallback below only
  // reaches the instance the socket is bound to.
  stopWorkflowRun: (runId: string, sessionId: string, instanceId?: string) => Promise<boolean>;
  // Best-effort per-session run summaries for the runs list (the `resumable`
  // verdict lives there). Silent on failure: a session the bridge no longer
  // knows simply contributes no summaries and its rows stay read-only.
  loadRunSummaries: (
    sessionIds: string[],
    /** Owning instance for a detail page opened from another group. */
    instanceId?: string,
  ) => Promise<Record<string, ConversationRunSummary[]>>;
}

// Module singletons: connection + timers live outside React state.
let acp: AcpConnection | null = null;
// Single-flight respawn of a bridge that died under a still-open session
// (serve bridge idle-exited during phone sleep — the socket never delivered
// a close, so nothing else noticed). The first session-bound call to hit
// the dead instance revives it; concurrent failures share the one attempt.
let reviveInFlight: Promise<string | null> | null = null;
// Toast auto-clear; the id guards against a stale timer clearing a newer toast.
const TOAST_MS = 4000;
let toastSeq = 0;
let toastTimer: ReturnType<typeof setTimeout> | null = null;
// Composer-prefill request counter; see setComposerPrefill.
let prefillSeq = 0;
// Events from superseded connections must be ignored (their async onclose
// can fire after a new connection to the SAME instance was started).
let connSeq = 0;
let pollTimer: ReturnType<typeof setInterval> | null = null;
let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
let reconnectAttempt = 0;
// When the in-flight openConnection() attempt started (0 = none). wakeProbe
// uses it to recognize a connect attempt wedged past its 15s deadline.
let connectingSince = 0;
// >0 while a tryReconnect() call is in flight (stamped on entry, cleared in
// its finally). A fired reconnect timer leaves a dead id behind, so
// "!reconnectTimer" alone can't see the wedge where the timer callback
// itself hangs — wakeProbe also treats an attempt that outlived every
// legitimate budget (10s discovery fetch + 15s WS deadline) as stuck.
let reconnectRunSince = 0;
const RECONNECT_RUN_WEDGE_MS = 45_000;
// Last time the ACP socket delivered a session/update (0 = never). Fresh
// traffic proves liveness better than any probe — wakeProbe skips its ping
// while a turn is streaming, so a loaded-but-slow tunnel never gets a
// healthy connection torn down for answering slowly.
let lastWireActivityAt = 0;
const WIRE_ACTIVITY_FRESH_MS = 30_000;
// Consecutive network-level discovery failures. A phone's network stack
// takes a moment after launch — the first failed poll means "not yet",
// not "hub down"; only DISCOVERY_FAIL_THRESHOLD in a row earn the banner.
let discoveryFailures = 0;
const DISCOVERY_FAIL_THRESHOLD = 3;

// Connection-level failures are transient by design: the reconnect loop
// (and its banner) own that story, and a successful reconnect replays the
// session back to freshness. Only settled failures deserve a notice.
function isTransientConnError(e: unknown): boolean {
  if (e instanceof HubApiError) return e.network;
  const msg = e instanceof Error ? e.message : String(e);
  return msg.includes("connection closed") || msg.includes("connection not open");
}

function stopReconnect(): void {
  if (reconnectTimer) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
}

// Optimistically merge a just-created session into the discovery list: add
// it to its instance (creating the instance row if discovery hasn't seen it
// yet) so it renders before the first prompt promotes it.
function mergeCreatedSession(
  instances: HubInstance[],
  created: { instanceId: string; sessionId: string; workspace: string },
): HubInstance[] {
  const session: HubSessionInfo = {
    sessionId: created.sessionId,
    updatedAt: Date.now(),
  };
  const idx = instances.findIndex((i) => i.id === created.instanceId);
  if (idx === -1)
    return [
      ...instances,
      {
        id: created.instanceId,
        workspace: created.workspace,
        origin: "serve",
        sessions: [session],
      },
    ];
  const inst = instances[idx];
  if (inst.sessions.some((x) => x.sessionId === session.sessionId)) return instances;
  const next = [...instances];
  next[idx] = {
    ...inst,
    workspace: inst.workspace ?? created.workspace,
    sessions: [...inst.sessions, session],
  };
  return next;
}

// The profile matching the book's active entry (first entry as fallback).
function activeProfile(servers: SavedServer[], activeId: string | null): ConnectionProfile | null {
  const s = servers.find((x) => x.id === activeId) ?? servers[0];
  return s ? { hubUrl: s.hubUrl, token: s.token } : null;
}

// Session/connection-scoped reset shared by disconnect, server switch and
// active-server delete. Quota data clears too — it belongs to the hub being
// left, not to the instance connection.
function connectionResetPatch(): Partial<AppState> {
  return {
    connState: "idle",
    instanceId: null,
    activeSessionId: null,
    messages: [],
    pendingPrompts: {},
    planEntries: null,
    permissions: {},
    elicitations: {},
    notice: null,
    replayCursor: null,
    hasMore: false,
    totalMessages: null,
    loadingEarlier: false,
    configOptions: [],
    currentModeId: null,
    usage: null,
    cacheHit: null, turnStats: null,
    availableCommands: [],
    usageStats: null,
    quotaDock: null,
    usageStatsAt: null,
    sessionStates: {},
    quotaUnavailable: false,
    fsCapable: false,
    pageMarkerCapable: false,
    loadingSession: false,
    isRunning: false,
    // Configuration state describes ONE machine, so it is re-probed on the
    // next hub rather than carried across. Two things make that mandatory:
    // the in-flight install polling and the reset-card session name a provider
    // whose card may have changed underneath us, and `configSupported` only
    // re-probes while it is still null — a stale false from an older hub would
    // permanently report "no settings API" against one that has it.
    //
    // `pendingRestart` is the one that can do damage rather than mislead: it is
    // "THIS bridge owes a restart". Left set across a hub switch, the next
    // machine's screens offer a restart whose backend never had the write —
    // and accepting it cancels that machine's in-flight turns for nothing.
    appUpdateInstall: null,
    resetCards: null,
    resetProviderId: null,
    resetNonce: null,
    resetIdempotencyKey: null,
    resetEligible: [],
    resetCredentials: true,
    resetBusy: false,
    resetNextTryAt: null,
    pendingRestart: false,
    // The capability verdict and every payload are re-read from the NEXT hub,
    // not carried over from this one. `configSupported` matters most: a false
    // left behind by an older hub would make the entry screen report "this hub
    // has no settings API" against a hub that does, and the probe only runs
    // when the value is still null — so a stale false is permanent.
    configSupported: null,
    configLoading: false,
    configError: null,
    configAll: null,
    configModels: null,
    configSkills: null,
    configMcp: null,
    configHooks: null,
    configAgents: null,
    configUsage: null,
    configUsageRange: "7d",
    configPlatformUsage: null,
    configPlatformUsageRange: "7d",
    configPlatformUsageSupported: true,
    configBackups: null,
    configAppUpdate: null,
    configWorkflows: null,
    configWorkflowScope: "project",
    configWorkflowGate: null,
    workflowHub: null,
    workflowActiveRuns: null,
    workflowRecentRuns: null,
    workflowHubError: null,
    workflowHubLoading: false,
    workflowRunTarget: null,
    composerPrefill: null,
  };
}

export const useAppStore = create<AppState>((set, get) => {
  function hub(): HubClient | null {
    const p = get().profile;
    return p ? new HubClient(p.hubUrl, p.token) : null;
  }

  // True while a session/prompt WE sent is in flight. Turn-end notifications
  // ($/zcode/turnState) arrive before the prompt response; those turns flush
  // their queue in runPrompt's finally, so the notification path must not
  // double-flush (it only handles restored/foreign turns).
  let localPromptActive = false;

  // Fold a turn's usage block (prompt response, or the turn-end broadcast a
  // NEW bridge attaches for attach-only clients) into the title-bar readouts.
  // Turns without cache numbers keep the previous rate; tps needs output
  // tokens plus a wall time (absent on foreign turns from an old bridge).
  function applyTurnUsage(sessionId: string, usage: unknown, seconds?: number): void {
    const hit = cacheHitRate(usage);
    const v = usage as { outputTokens?: unknown } | null;
    const outTok = typeof v?.outputTokens === "number" ? v.outputTokens : 0;
    const patch: Partial<AppState> = {};
    if (hit !== null) patch.cacheHit = hit;
    if (outTok > 0 && seconds != null && seconds > 0) {
      patch.turnStats = {
        tps: Math.round(outTok / seconds),
        outputTokens: outTok,
        seconds: Math.round(seconds * 10) / 10,
      };
    }
    if (Object.keys(patch).length === 0) return;
    set((s) => (s.activeSessionId === sessionId ? patch : {}));
  }

  // Single flush point for the pending queue: after a local turn settles,
  // after a restored (bridged) turn ends, and when an attach lands. The
  // check-then-run is synchronous, so concurrent callers stay safe.
  function flushPending(): void {
    // The optimistic bubble inserted below must see every ARRIVED update:
    // a turn's trailing chunks precede its settle notification on the wire
    // but may still sit in applyUpdate's 120ms batch — inserting first
    // would land the queued message in the middle of the finished turn.
    flushUpdateBatch();
    const s = get();
    if (s.connState !== "open" || !s.activeSessionId) return;
    if (s.loadingSession || s.isRunning) return;
    const next = s.pendingPrompts[s.activeSessionId]?.[0];
    if (next == null) return;
    setQueue(s.activeSessionId, s.pendingPrompts[s.activeSessionId].slice(1));
    void get().runPrompt(next);
  }

  // Every queue mutation goes through here so state and localStorage stay in
  // sync (drafts must survive switches/restarts). Empty queues drop the key.
  function setQueue(sid: string, next: PromptDraft[]): void {
    set((state) => {
      const map = { ...state.pendingPrompts };
      if (next.length > 0) map[sid] = next;
      else delete map[sid];
      savePending(map);
      return { pendingPrompts: map };
    });
  }

  /**
   * The instance's workspace label from the hub list, or undefined when the
   * list is stale/missing this instance. Callers that must send a cwd
   * (session/load's SDK schema requires one) fall back to "/" — safe because
   * the bridge ignores the load cwd entirely: session roots are
   * backend-authoritative, never taken from the client.
   */
  function instanceWorkspace(): string | undefined {
    const s = get();
    const inst = s.instances.find((i) => i.id === s.instanceId);
    const ws = inst?.workspace;
    return ws && ws !== "/" ? ws : undefined;
  }

  function startPolling(): void {
    if (pollTimer) return;
    void get().refreshInstances();
    pollTimer = setInterval(() => void get().refreshInstances(), 4000);
  }

  function stopPolling(): void {
    if (pollTimer) {
      clearInterval(pollTimer);
      pollTimer = null;
    }
  }

  // Shared connection teardown for the hub-level transitions (disconnect /
  // switch server / delete active server): everything those paths do besides
  // their own state reset.
  function teardownConnection(): void {
    stopPolling();
    stopReconnect();
    connSeq++; // invalidate any in-flight connection events
    acp?.close();
    acp = null;
    pendingResponds.clear();
    pendingElicitResponds.clear();
    reconnectAttempt = 0;
    dropQueuedUpdates();
  }

  // Replay bursts hundreds of updates in one go; batching them into a single
  // store write keeps render cost O(bursts) instead of O(chunks).
  let updateQueue: { sessionId: string; u: SessionUpdate; meta?: unknown }[] = [];
  let flushTimer: ReturnType<typeof setTimeout> | null = null;
  // While a session/load_earlier request is in flight its updates must be
  // PREPENDED, not appended — buffer them until the request resolves.
  let collectingEarlier = false;
  let earlierBuffer: { sessionId: string; u: SessionUpdate; meta?: unknown }[] = [];
  // Page updates (marked earlierPage) arriving OUTSIDE the collection window
  // — the load_earlier response already landed. They are replay content by
  // construction, never live, so they must not fall into applyUpdate (that
  // lands them at the tail: older messages appended after the newest ones).
  // Hold them briefly so a burst coalesces into ONE prepended segment.
  let latePageBuffer: {
    sessionId: string;
    u: SessionUpdate;
    meta?: unknown;
  }[] = [];
  let latePageTimer: ReturnType<typeof setTimeout> | null = null;

  // Legacy bridges (< PAGE_MARKER_BRIDGE_VERSION) cannot mark page updates,
  // and any bridge's load_earlier response may overtake its own page
  // notifications on the wire (the bridge dispatches them fire-and-forget).
  // On a marker-capable bridge the marker alone routes the stragglers into
  // the late-page buffer (prepended). Without a marker the collection window
  // must survive the response and stay open until the page's burst goes
  // quiet — closing it on the response appends the rest of the page after the
  // newest message, which reads as a middle chunk of an old session landing
  // at the end.
  const EARLIER_WINDOW_QUIET_MS = 600;
  const EARLIER_WINDOW_CAP_MS = 20_000;
  let earlierStreamLastAt = 0;

  function dropQueuedUpdates(): void {
    updateQueue = [];
    collectingEarlier = false;
    earlierBuffer = [];
    latePageBuffer = [];
    if (flushTimer) {
      clearTimeout(flushTimer);
      flushTimer = null;
    }
    if (latePageTimer) {
      clearTimeout(latePageTimer);
      latePageTimer = null;
    }
  }

  // Prepends whatever late page updates accumulated as its own oldest-first
  // segment, mirroring loadEarlier's assembly (messages only; same seam
  // dedupe against the current head). Stale sessions are dropped.
  function flushLatePage(): void {
    latePageTimer = null;
    const page = latePageBuffer;
    latePageBuffer = [];
    if (page.length === 0) return;
    set((state) => {
      const sid = state.activeSessionId;
      if (!sid) return {};
      let segment: ChatMessage[] = [];
      let mine = false;
      for (const { sessionId, u, meta } of page) {
        if (sessionId !== sid) continue;
        mine = true;
        const r = applyOne(segment, u, meta);
        if (r?.messages) segment = r.messages;
      }
      if (!mine || segment.length === 0) return {};
      let rest = state.messages;
      if (rest.length && segment[segment.length - 1].id === rest[0].id) {
        // Seam dedupe: the same message split across pages.
        const seam = segment[segment.length - 1];
        segment = segment.slice(0, -1);
        if (segment.length === 0) return {};
        rest = [{ ...seam, parts: [...seam.parts, ...rest[0].parts] }, ...rest.slice(1)];
      }
      return { messages: [...segment, ...rest] };
    });
  }

  // Closes the legacy (unmarked-page) collection window once the update stream
  // has gone quiet, the hard cap fires, or our own prompt starts — the page is
  // over and anything still arriving is live content that must append. The
  // caller assembles the buffered page after this resolves.
  function settleEarlierWindow(): Promise<void> {
    return new Promise((resolve) => {
      const capAt = Date.now() + EARLIER_WINDOW_CAP_MS;
      const step = () => {
        const now = Date.now();
        if (
          now - earlierStreamLastAt >= EARLIER_WINDOW_QUIET_MS ||
          now >= capAt ||
          localPromptActive
        ) {
          collectingEarlier = false;
          resolve();
          return;
        }
        setTimeout(step, 200);
      };
      setTimeout(step, 200);
    });
  }

  // Snapshots the active session into the replay cache. Skipped while a turn
  // is in flight (ours or foreign): mid-turn messages are partial, and the
  // optimistic prompt bubble may never reach the bridge. Also skipped while
  // loadingSession — a replay/reconcile is mid-flight and the store holds a
  // half-applied history. Both writers that settle a turn (onTurnState end,
  // runPrompt's finally) re-snapshot through their own flush paths.
  function cacheActiveSnapshot(): void {
    const s = get();
    if (!s.activeSessionId || s.loadingSession) return;
    if (s.isRunning || localPromptActive) return;
    writeSessionCache(s.activeSessionId, {
      messages: s.messages,
      planEntries: s.planEntries,
      replayCursor: s.replayCursor,
      hasMore: s.hasMore,
      totalMessages: s.totalMessages,
      lastMessageId: snapshotTailId(s.messages),
      configOptions: s.configOptions,
      currentModeId: s.currentModeId,
      usage: s.usage,
      cacheHit: s.cacheHit,
      turnStats: s.turnStats,
    });
  }

  // Applies (or re-applies) whatever arrived since the last batch. Called by
  // the 120ms coalescing timer and synchronously by flushPending, which must
  // not let the optimistic bubble overtake already-received turn content.
  function flushUpdateBatch(): void {
    if (flushTimer) {
      clearTimeout(flushTimer);
      flushTimer = null;
    }
    const batch = updateQueue;
    if (batch.length === 0) return;
    updateQueue = [];
    set((state) => {
      let messages = state.messages;
      let planEntries = state.planEntries;
      let configOptions = state.configOptions;
      let currentModeId = state.currentModeId;
      let usage = state.usage;
      let availableCommands = state.availableCommands;
      for (const { sessionId: sid, u: upd, meta } of batch) {
        if (state.activeSessionId !== sid) continue;
        const r = applyOne(messages, upd, meta);
        if (!r) continue;
        if (r.messages) messages = r.messages;
        if (r.planEntries !== undefined) planEntries = r.planEntries;
        if (r.configOptions) configOptions = r.configOptions;
        if (r.currentModeId !== undefined) currentModeId = r.currentModeId;
        if (r.usage) usage = r.usage;
        if (r.availableCommands) availableCommands = r.availableCommands;
      }
      return {
        messages,
        planEntries,
        configOptions,
        currentModeId,
        usage,
        availableCommands,
      };
    });
    cacheActiveSnapshot();
  }

  function applyUpdate(sessionId: string, u: SessionUpdate, meta?: unknown): void {
    updateQueue.push({ sessionId, u, meta });
    if (flushTimer) return;
    flushTimer = setTimeout(() => {
      flushTimer = null;
      flushUpdateBatch();
    }, 120);
  }

  // ---- connection lifecycle ----

  // Bridge-wide broadcasts keep per-session activity for the session list
  // (awaiting confirmation > running > just finished > idle).
  function setActivity(
    sessionId: string,
    patch: Partial<SessionActivity> | ((prev: SessionActivity) => Partial<SessionActivity>),
  ): void {
    set((state) => {
      const prev: SessionActivity = state.sessionStates[sessionId] ?? {
        running: false,
        awaitingPermission: false,
      };
      const next = {
        ...prev,
        ...(typeof patch === "function" ? patch(prev) : patch),
      };
      if (
        next.running === prev.running &&
        next.awaitingPermission === prev.awaitingPermission &&
        next.finishedAt === prev.finishedAt
      ) {
        return {};
      }
      return { sessionStates: { ...state.sessionStates, [sessionId]: next } };
    });
  }

  function buildApprovalContext(params: Record<string, unknown>): ApprovalContext | undefined {
    const tc = (params.toolCall ?? null) as {
      toolCallId?: unknown;
      title?: unknown;
      content?: unknown;
      rawInput?: unknown;
    } | null;
    if (!tc || typeof tc !== "object") return undefined;
    const toolCallId =
      typeof tc.toolCallId === "string" && tc.toolCallId ? tc.toolCallId : undefined;
    const part = toolCallId ? findToolCallPart(get().messages, toolCallId) : null;
    const rawInput = tc.rawInput;
    const plan =
      rawInput &&
      typeof rawInput === "object" &&
      typeof (rawInput as { plan?: unknown }).plan === "string"
        ? (rawInput as { plan: string }).plan
        : undefined;
    // Wire-provided popup fields (bridge 0.33+): title and content text
    // blocks carry the readable form of the input — prefer them over the
    // matched tool_call part and the INPUT_KEYS heuristic.
    const title = typeof tc.title === "string" && tc.title ? tc.title : undefined;
    let contentText: string | undefined;
    if (Array.isArray(tc.content)) {
      const texts = tc.content
        .map((b) => (b as { content?: { text?: unknown } } | null)?.content?.text ?? null)
        .filter((t): t is string => typeof t === "string" && t.length > 0);
      if (texts.length > 0) contentText = texts.join("\n\n");
    }
    let rawInputText: string | undefined;
    if (rawInput != null) {
      try {
        rawInputText = typeof rawInput === "string" ? rawInput : JSON.stringify(rawInput, null, 2);
      } catch {
        rawInputText = String(rawInput);
      }
    }
    if (!toolCallId && rawInputText == null && title == null && contentText == null)
      return undefined;
    return {
      toolCallId,
      // Part fields first (richer); title = the wire title stored in toolName.
      toolName: part?.rawName ?? part?.toolName,
      kind: part?.kind,
      title: title ?? part?.toolName,
      detail: contentText ?? part?.detail,
      plan,
      rawInputText,
    };
  }

  // Parses an elicitation/create form into renderable fields: the bridge's
  // buildAskUserElicitationForm shape (`q_<i>` enum/array + `q_<i>_other`
  // free-text companion) and the plan-approval form (single `approval` enum
  // field whose `description` carries the full plan markdown). The skip
  // sentinel option is dropped — an unanswered field IS the skip (the
  // bridge's parser treats absent as skipped).
  function parseElicitationForm(params: Record<string, unknown>): {
    sessionId: string;
    message: string;
    fields: ElicitField[];
  } | null {
    const sessionId = String(params.sessionId ?? "");
    const message = typeof params.message === "string" ? params.message : "";
    const schema = params.requestedSchema as { properties?: Record<string, unknown> } | undefined;
    const props = schema?.properties;
    if (!sessionId || typeof props !== "object" || props === null) return null;
    const fields: ElicitField[] = [];
    for (const key of Object.keys(props)) {
      if (key.endsWith("_other")) continue;
      const prop = props[key] as
        | {
            type?: unknown;
            title?: unknown;
            description?: unknown;
            oneOf?: Array<{ const?: unknown; title?: unknown }>;
            items?: { anyOf?: Array<{ const?: unknown; title?: unknown }> };
          }
        | undefined;
      if (!prop || typeof prop !== "object") continue;
      const variants = Array.isArray(prop.oneOf)
        ? prop.oneOf
        : Array.isArray(prop.items?.anyOf)
          ? (prop.items!.anyOf as Array<{ const?: unknown; title?: unknown }>)
          : null;
      if (!variants) continue; // not a question field (additive schema)
      const options = variants
        .filter((v) => typeof v.const === "string" && v.const !== "__skip__")
        .map((v) => ({
          value: v.const as string,
          label: typeof v.title === "string" ? v.title : (v.const as string),
        }));
      fields.push({
        key,
        otherKey: typeof props[`${key}_other`] === "object" ? `${key}_other` : null,
        question: typeof prop.title === "string" ? prop.title : key,
        description:
          typeof prop.description === "string" && prop.description.trim()
            ? prop.description
            : undefined,
        multi: prop.type === "array",
        options,
      });
    }
    if (fields.length === 0) return null;
    return { sessionId, message, fields };
  }

  function scheduleReconnect(): void {
    const s = get();
    if (!s.profile || !s.instanceId) return;
    stopReconnect();
    const delay = Math.min(30000, 1000 * 2 ** reconnectAttempt++);
    set({ connState: "reconnecting" });
    reconnectTimer = setTimeout(() => void tryReconnect(), delay);
  }

  // Liveness probe for wake: the bridge's SDK answers unknown methods with an
  // immediate -32601, so ANY settle (even a rejection) within the timeout
  // proves the round-trip works. Only silence — or a locally-refused send —
  // means the socket is a zombie.
  function probeAlive(timeoutMs: number): Promise<boolean> {
    const conn = acp;
    if (!conn) return Promise.resolve(false);
    return new Promise((resolve) => {
      let settled = false;
      const done = (alive: boolean) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        resolve(alive);
      };
      const timer = setTimeout(() => done(false), timeoutMs);
      conn.request("$/zcode/ping").then(
        () => done(true),
        (e: Error) => done(e.message !== "connection not open"),
      );
    });
  }

  // Deep sleep on Android routinely kills the socket without ever delivering
  // onclose in the WebView: connState stays "open", every request silently
  // vanishes, and nothing schedules a reconnect. On foreground, probe the
  // pipe; a zombie is torn down exactly like an unexpected close so the
  // reconnect loop (and its replay) recover the session automatically.
  // Belt-and-braces for the same freeze in mid-(re)connect: an attempt wedged
  // past its deadline, or a "reconnecting" with no timer left, gets the same
  // teardown — without this the connect() timeout is the only way out.
  function teardownZombieConnection(): void {
    connSeq++;
    acp?.close();
    acp = null;
    connectingSince = 0;
    reconnectRunSince = 0;
    pendingResponds.clear();
    pendingElicitResponds.clear();
    set({ permissions: {}, elicitations: {}, sessionStates: {} });
    // Whatever was torn down has already kept the user waiting; restart the
    // backoff from the bottom so recovery is a 1s retry, not another 30s.
    reconnectAttempt = 0;
    scheduleReconnect();
  }

  async function wakeProbe(): Promise<void> {
    const s = get();
    if (!s.profile || !s.instanceId) return;
    if (s.connState === "open") {
      if (Date.now() - lastWireActivityAt < WIRE_ACTIVITY_FRESH_MS) return;
      if (await probeAlive(5000)) return;
      if (get().connState !== "open") return; // closed meanwhile — loop has it
      teardownZombieConnection();
      return;
    }
    if (
      (s.connState === "connecting" &&
        connectingSince > 0 &&
        Date.now() - connectingSince > 30_000) ||
      (s.connState === "reconnecting" &&
        (!reconnectTimer ||
          (reconnectRunSince > 0 &&
            Date.now() - reconnectRunSince > RECONNECT_RUN_WEDGE_MS)))
    ) {
      teardownZombieConnection();
    }
  }

  // Stamps reconnectRunSince around the body so wakeProbe can recognize a
  // wedged attempt (see the comment on the variable).
  async function tryReconnect(): Promise<void> {
    reconnectRunSince = Date.now();
    try {
      await tryReconnectInner();
    } finally {
      reconnectRunSince = 0;
    }
  }

  async function tryReconnectInner(): Promise<void> {
    const s = get();
    if (!s.profile || !s.instanceId) return;
    // Probe: a hard-killed bridge lingers in the heartbeat view for up to 30s,
    // which would keep the reconnect loop retrying a dead instance.
    await s.refreshInstances({ probe: true });
    // Re-read after the await: the hub/instance may have changed meanwhile.
    const cur = get();
    if (!cur.profile || cur.profile !== s.profile || cur.instanceId !== s.instanceId) return;
    // Hub unreachable or discovery failed (phone just woke, network not yet
    // ready): instance liveness is UNKNOWN, not "gone" — retry in place
    // instead of bouncing the user out of the session. Only a successful
    // listing that lacks the instance proves it gone.
    if (cur.hubOffline || cur.instancesError) {
      scheduleReconnect();
      return;
    }
    const still = cur.instances.some((i) => i.id === s.instanceId);
    if (!still) {
      // Instance gone: its sessions are gone too (contract).
      connSeq++;
      acp?.close();
      acp = null;
      dropQueuedUpdates();
      set({
        connState: "idle",
        instanceId: null,
        activeSessionId: null,
        messages: [],
        planEntries: null,
        permissions: {},
        elicitations: {},
        notice: "notice.instanceGone",
        fsCapable: false,
        pageMarkerCapable: false,
        replayCursor: null,
        hasMore: false,
        totalMessages: null,
        loadingEarlier: false,
        configOptions: [],
        currentModeId: null,
        usage: null,
        cacheHit: null, turnStats: null,
        availableCommands: [],
        // usageStats stays (hub-level quota, not instance-scoped) — nothing
        // re-fetches it here, so clearing would blank the card until the
        // next app start.
        sessionStates: {},
        loadingSession: false,
        isRunning: false,
      });
      reconnectAttempt = 0;
      return;
    }
    await openConnection(cur.profile, cur.instanceId!);
  }

  /**
   * Respawn the bridge behind the current session after the hub proved it
   * gone ("unknown instance" / "bridge unreachable" on a session-bound
   * call). Same path as manually re-opening the session: createInstance
   * incubates a serve bridge for the workspace, which reconnects AND
   * re-attaches (replay included), so the caller just retries on the NEW
   * instanceId. The stale local instance list still carries the dead
   * instance's workspace — the hub no longer lists it, so refreshing first
   * would erase the one fact the respawn needs. Single-flight: concurrent
   * failures (e.g. a listing + a file window) share one respawn.
   */
  async function reviveDeadInstance(): Promise<string | null> {
    if (reviveInFlight) return reviveInFlight;
    const s = get();
    if (!s.profile || !s.activeSessionId) return null;
    const workspace = s.instances.find((i) => i.id === s.instanceId)?.workspace;
    if (!workspace) return null;
    const sessionId = s.activeSessionId;
    reviveInFlight = get()
      .resumeProjectSession(workspace, sessionId)
      .then((ok) => (ok ? get().instanceId : null))
      .catch(() => null);
    try {
      return await reviveInFlight;
    } finally {
      reviveInFlight = null;
    }
  }

  async function openConnection(profile: ConnectionProfile, instanceId: string): Promise<void> {
    stopReconnect();
    const mySeq = ++connSeq;
    acp?.close();
    const stale = () => mySeq !== connSeq;
    const conn = new AcpConnection(profile.hubUrl, profile.token, instanceId, {
      onState: (state) => {
        if (stale()) return;
        if (state === "open") {
          reconnectAttempt = 0;
          connectingSince = 0;
          lastWireActivityAt = Date.now();
          // Activity is broadcast-only: a fresh connection knows nothing
          // until events arrive (active session's turnActive is restored by
          // loadSession's replay). Stale notices (e.g. from the dropped
          // connection's failed requests) die with it — the replay has
          // already superseded whatever they complained about.
          set({ connState: "open", sessionStates: {}, notice: null });
        } else if (state === "closed") {
          if (get().instanceId === instanceId) {
            // The pending permission/elicitation requests die with the
            // socket; don't leave dead dialogs that silently swallow taps.
            // Broadcast activity is equally untrustworthy now.
            pendingResponds.clear();
            pendingElicitResponds.clear();
            set({
              permissions: {},
              elicitations: {},
              sessionStates: {},
            });
            scheduleReconnect();
          }
        }
      },
      onUpdate: (sessionId, update, meta) => {
        if (stale()) return;
        lastWireActivityAt = Date.now();
        // Page updates ride the earlier buffer; live-turn updates streaming
        // during the page request append normally instead of being prepended
        // to the top with the page. On a marker-capable bridge (0.44.1+) the
        // bridge marks pages on the update's own _meta (same placement as the
        // tool-fold flags; notification-level meta as a fallback); an older
        // bridge sends unmarked pages, so everything buffers as before.
        // Marked updates arriving OUTSIDE the window are still replay, never
        // live — they take the late-page buffer (prepended), never the tail.
        const marked =
          isEarlierPageMeta((update as { _meta?: unknown })._meta) || isEarlierPageMeta(meta);
        if (
          collectingEarlier &&
          sessionId === get().activeSessionId &&
          (!get().pageMarkerCapable || marked)
        ) {
          earlierBuffer.push({ sessionId, u: update, meta });
          earlierStreamLastAt = Date.now();
          return;
        }
        if (marked) {
          latePageBuffer.push({ sessionId, u: update, meta });
          if (!latePageTimer) {
            latePageTimer = setTimeout(() => {
              latePageTimer = null;
              flushLatePage();
            }, 120);
          }
          return;
        }
        applyUpdate(sessionId, update, meta);
      },
      onTurnState: (sessionId, running, turn) => {
        if (stale()) return;
        setActivity(
          sessionId,
          running
            ? {
                running: true,
                awaitingPermission: false,
                finishedAt: undefined,
              }
            : {
                running: false,
                awaitingPermission: false,
                finishedAt: Date.now(),
              },
        );
        if (sessionId !== get().activeSessionId) return;
        if (running) {
          set({ isRunning: true });
          return;
        }
        // Turn-end broadcast carries the turn's usage (new bridges): an
        // attach-only client learns cache/tps for turns driven from another
        // client (TUI, editor, phone) — the response path only reaches the
        // prompt's sender.
        if (turn?.usage != null) {
          applyTurnUsage(
            sessionId,
            turn.usage,
            turn.turnMs != null ? Math.max(turn.turnMs / 1000, 0.1) : undefined,
          );
        }
        // End-of-turn arrives BEFORE the prompt response for locally sent
        // turns (the bridge notifies in its finally, responds on return) —
        // those own their state in runPrompt's finally. Restored/foreign
        // turns have no local prompt in flight: settle here and flush.
        if (localPromptActive) return;
        set({ isRunning: false });
        cacheActiveSnapshot();
        flushPending();
      },
      onWorkflowListChanged: () => {
        if (stale()) return;
        get().refreshWorkflowHubIfLoaded();
      },
      onServerRequest: (req, respond) => {
        if (stale()) return;
        if (req.method === "session/request_permission") {
          const sessionId = String(req.params.sessionId ?? "");
          const options = Array.isArray(req.params.options)
            ? (req.params.options as PermissionOption[])
            : [];
          setActivity(sessionId, { awaitingPermission: true });
          set((s) => ({
            permissions: {
              ...s.permissions,
              [sessionId]: {
                requestId: req.id,
                sessionId,
                options: options.filter((o) => typeof o.optionId === "string"),
                context: buildApprovalContext(req.params),
              },
            },
          }));
          // respond is captured by answerPermission through the stored request id.
          pendingResponds.set(req.id, respond);
          // Foreground visibility: the approval sheet renders only inside its
          // own session — when the user is elsewhere in the app, announce the
          // ask once so a blocked approval never waits in silence (the
          // backgrounded/locked-phone case is the bridge's push, not ours).
          if (get().activeSessionId !== sessionId) {
            const raw = req.params as { toolCall?: { title?: unknown }; toolName?: unknown };
            const title =
              (typeof raw.toolCall?.title === "string" && raw.toolCall.title) ||
              (typeof raw.toolName === "string" && raw.toolName) ||
              "tool";
            get().notify(`approval requested: ${title.slice(0, 60)}`);
          }
        } else if (req.method === "elicitation/create") {
          // AskUserQuestion / plan-approval form — the bridge's preferred
          // channel once any client advertises elicitation.form, which we
          // now declare ourselves in initialize (see AcpConnection).
          const parsed = parseElicitationForm(req.params);
          if (parsed) {
            setActivity(parsed.sessionId, { awaitingPermission: true });
            set((s) => ({
              elicitations: {
                ...s.elicitations,
                [parsed.sessionId]: { requestId: req.id, ...parsed },
              },
            }));
            pendingElicitResponds.set(req.id, respond);
            if (get().activeSessionId !== parsed.sessionId) {
              get().notify(
                `agent question: ${parsed.message.slice(0, 60) || "waiting for your answer"}`,
              );
            }
          } else {
            // Unparseable schema: decline rather than leave the request
            // dangling (first-response-wins; Zed may still answer).
            respond({ action: "decline" });
          }
        } else {
          // Other unsupported interaction. Deliberately NOT answered:
          // first-response-wins, and the primary editor client is always
          // attached (bridge lifetime = editor lifetime).
          set({ notice: "notice.unsupported" });
        }
      },
      onCancelRequest: (id) => {
        // We lost the Permission Race; drop the dialog(s) for this request.
        const cleared: string[] = [];
        set((s) => {
          const permissions = { ...s.permissions };
          const elicitations = { ...s.elicitations };
          for (const [sid, p] of Object.entries(permissions)) {
            if (p.requestId === id) {
              delete permissions[sid];
              cleared.push(sid);
            }
          }
          for (const [sid, e] of Object.entries(elicitations)) {
            if (e.requestId === id) {
              delete elicitations[sid];
              cleared.push(sid);
            }
          }
          return { permissions, elicitations };
        });
        for (const sid of cleared) setActivity(sid, { awaitingPermission: false });
        pendingResponds.delete(id);
        pendingElicitResponds.delete(id);
      },
    });
    acp = conn;
    connectingSince = Date.now();
    try {
      const init = await conn.connect();
      if (stale()) return;
      connectingSince = 0;
      set({
        fsCapable: init.agentCapabilities?._meta?.zcode?.fs === true,
        pageMarkerCapable: bridgeSupportsPageMarker(
          (init.agentInfo as { version?: unknown } | undefined)?.version,
        ),
      });
      const active = get().activeSessionId;
      if (active) {
        // Replay is the catch-up mechanism after any disconnect.
        await get().loadSession(active);
      }
    } catch {
      connectingSince = 0;
      if (get().instanceId === instanceId) scheduleReconnect();
    }
  }

  const pendingResponds = new Map<number, (result: unknown) => void>();
  const pendingElicitResponds = new Map<number, (result: unknown) => void>();

  // Scales the whole UI: every Tailwind text/size class is rem-based, so
  // overriding the root font size is enough. "small" keeps the browser
  // default (16px) — i.e. the pre-setting look.
  const FONT_SIZE_PX = { small: "16px", medium: "17.5px", large: "19px" };
  function applyFontSize(size: FontSize) {
    document.documentElement.style.fontSize = FONT_SIZE_PX[size];
  }

  return {
    profile: null,
    savedServers: [],
    activeServerId: null,
    manageOpen: false,
    lang: "en",
    fontSize: "small" as FontSize,
    instances: [],
    instancesError: null,
    hubOffline: false,
    connState: "idle",
    instanceId: null,
    activeSessionId: null,
    messages: [],
    pendingPrompts: loadPending(),
    planEntries: null,
    isRunning: false,
    permissions: {},
    elicitations: {},
    notice: null,
    toast: null,
    replayCursor: null,
    hasMore: false,
    totalMessages: null,
    loadingEarlier: false,
    configOptions: [],
    currentModeId: null,
    usage: null,
    cacheHit: null, turnStats: null,
    availableCommands: [],
    usageStats: null,
    quotaDock: null,
    usageStatsAt: null,
    sessionStates: {},
    quotaUnavailable: false,
    fsCapable: false,
    pageMarkerCapable: false,
    loadingSession: false,
    workflowHub: null,
    workflowActiveRuns: null,
    workflowRecentRuns: null,
    workflowHubError: null,
    workflowHubLoading: false,
    workflowRunTarget: null,
    composerPrefill: null,
    ...configSliceState,

    init: () => {
      const book = loadServerBook();
      const fontSize = loadFontSize();
      applyFontSize(fontSize);
      // Seed the channel mirror and the state together, so a stored "preview"
      // is in effect before the first app-update read.
      const channel = loadUpdateChannel();
      seedUpdateChannel(channel);
      set({
        savedServers: book?.servers ?? [],
        activeServerId: book?.activeId ?? null,
        profile: book ? activeProfile(book.servers, book.activeId) : null,
        lang: loadLang(),
        fontSize,
        updateChannel: channel,
      });
      if (book) {
        startPolling();
        // Plain HTTP (no instance connection needed); slow-moving data.
        void get().refreshUsageStats();
      }
    },

    connectToHub: (input) => {
      // Normalize before the upsert so "http://hub" and "http://hub/" land
      // on the same entry instead of duplicating it.
      const hubUrl = input.hubUrl.trim().replace(/\/+$/, "");
      const token = input.token.trim();
      const label = input.name?.trim();
      const servers = [...get().savedServers];
      const idx = servers.findIndex((x) => x.hubUrl === hubUrl);
      let activeId: string;
      if (idx >= 0) {
        // An empty name keeps the entry's own — reconnecting to a saved URL
        // must not clobber a custom display name with the host default.
        servers[idx] = {
          ...servers[idx],
          hubUrl,
          token,
          ...(label ? { name: label } : {}),
        };
        activeId = servers[idx].id;
      } else {
        const server: SavedServer = {
          id: newServerId(),
          name: label || defaultServerName(hubUrl),
          hubUrl,
          token,
        };
        servers.push(server);
        activeId = server.id;
      }
      saveServerBook({ servers, activeId });
      discoveryFailures = 0;
      set({
        savedServers: servers,
        activeServerId: activeId,
        profile: { hubUrl, token },
        instances: [],
        instancesError: null,
        hubOffline: false,
        manageOpen: false,
      });
      startPolling();
      void get().refreshUsageStats();
    },

    disconnectHub: () => {
      teardownConnection();
      saveServerBook({ servers: get().savedServers, activeId: null });
      savePending({});
      set({
        activeServerId: null,
        profile: null,
        instances: [],
        instancesError: null,
        hubOffline: false,
        manageOpen: false,
        ...connectionResetPatch(),
      });
    },

    switchServer: (id) => {
      const target = get().savedServers.find((x) => x.id === id);
      if (!target) return;
      teardownConnection();
      saveServerBook({ servers: get().savedServers, activeId: id });
      savePending({});
      discoveryFailures = 0;
      set({
        activeServerId: id,
        profile: { hubUrl: target.hubUrl, token: target.token },
        instances: [],
        instancesError: null,
        hubOffline: false,
        manageOpen: false,
        ...connectionResetPatch(),
      });
      startPolling();
      void get().refreshUsageStats();
    },

    saveServer: (entry) => {
      const s = get();
      const idx = s.savedServers.findIndex((x) => x.id === entry.id);
      if (idx === -1) return;
      const hubUrl = entry.hubUrl.trim().replace(/\/+$/, "");
      const servers = [...s.savedServers];
      servers[idx] = {
        ...entry,
        hubUrl,
        name: entry.name.trim() || defaultServerName(hubUrl),
      };
      saveServerBook({ servers, activeId: s.activeServerId });
      set({ savedServers: servers });
      if (entry.id === s.activeServerId) {
        // The live connection still points at the old URL/token — re-run the
        // switch path so the edit takes effect immediately.
        get().switchServer(entry.id);
      }
    },

    deleteServer: (id) => {
      const s = get();
      const servers = s.savedServers.filter((x) => x.id !== id);
      if (servers.length === s.savedServers.length) return;
      if (s.activeServerId !== id) {
        saveServerBook({ servers, activeId: s.activeServerId });
        set({ savedServers: servers });
        return;
      }
      // Deleting the active server: disconnect back to the manager screen.
      teardownConnection();
      saveServerBook({ servers, activeId: null });
      savePending({});
      set({
        savedServers: servers,
        activeServerId: null,
        profile: null,
        instances: [],
        instancesError: null,
        hubOffline: false,
        manageOpen: false,
        ...connectionResetPatch(),
      });
    },

    openServerManager: () => set({ manageOpen: true }),

    closeServerManager: () => set({ manageOpen: false }),

    setLang: (lang) => {
      saveLang(lang);
      set({ lang });
    },

    setFontSize: (size) => {
      saveFontSize(size);
      applyFontSize(size);
      set({ fontSize: size });
    },

    refreshInstances: async (opts?: { probe?: boolean }) => {
      const client = hub();
      if (!client) return;
      const profile = get().profile;
      try {
        const instances = await client.instances(opts?.probe === true);
        if (get().profile !== profile) return; // hub changed mid-flight
        discoveryFailures = 0;
        set({ instances, instancesError: null, hubOffline: false });
      } catch (e) {
        if (get().profile !== profile) return;
        if (e instanceof HubApiError && e.network) {
          // Hub unreachable is expected while no editor runs (bridges
          // re-spawn it on demand) — but only say so once it has STAYED
          // unreachable: early polls race the phone's network stack, and a
          // transient blip must not flash the banner (or blank the list).
          discoveryFailures++;
          if (discoveryFailures >= DISCOVERY_FAIL_THRESHOLD) {
            set({ instances: [], instancesError: null, hubOffline: true });
          }
          return;
        }
        const msg =
          e instanceof HubApiError ? e.message : `discovery failed: ${(e as Error).message}`;
        set({ instancesError: msg, hubOffline: false });
      }
    },

    // Trigger the hub's SELF-decided upgrade check (bridge 0.11.7): POST
    // /api/upgrade, and if the hub judged the on-disk code newer (it exits
    // ~500ms after replying, re-spawning onto the new dist), poll health
    // until the respawn answers, then refresh discovery. The restart
    // decision is entirely the hub's — this only triggers the check.
    upgradeHub: async () => {
      const client = hub();
      if (!client) throw new Error("not connected");
      const result = await client.upgrade();
      if (!result.restarting) return result;
      const deadline = Date.now() + 30_000;
      for (;;) {
        await new Promise((r) => setTimeout(r, 1000));
        try {
          await client.health();
          break;
        } catch {
          // Still down mid-respawn — keep polling until the deadline; the
          // 4s discovery poll heals the list even if we give up here.
          if (Date.now() > deadline) break;
        }
      }
      await get().refreshInstances({ probe: true });
      return result;
    },

    connectInstance: async (instanceId, attachSessionId, opts) => {
      const s = get();
      if (!s.profile) return;
      stopReconnect();
      reconnectAttempt = 0;
      pendingResponds.clear();
      pendingElicitResponds.clear();
      dropQueuedUpdates();
      set({
        instanceId,
        connState: "connecting",
        activeSessionId: null,
        messages: [],
        planEntries: null,
        permissions: {},
        elicitations: {},
        notice: null,
        replayCursor: null,
        hasMore: false,
        totalMessages: null,
        loadingEarlier: false,
        configOptions: [],
        currentModeId: null,
        usage: null,
        cacheHit: null, turnStats: null,
        availableCommands: [],
        fsCapable: false,
        pageMarkerCapable: false,
        // usageStats stays: quota is hub-level (/api/quota), not tied to the
        // instance connection — switching instances must not blank the card.
        sessionStates: {},
        loadingSession: false,
      });
      await openConnection(s.profile, instanceId);
      // Attach to the requested session, else the most recently updated one.
      // noAttach (remote session-create): the caller opens a FRESH session
      // itself — never auto-attach a recycled instance's history.
      if (!opts?.noAttach && get().connState === "open" && !get().activeSessionId) {
        const inst = get().instances.find((i) => i.id === instanceId);
        let target = attachSessionId ?? null;
        if (!target && inst?.sessions?.length) {
          target = [...inst.sessions].sort((a, b) => (b.updatedAt ?? 0) - (a.updatedAt ?? 0))[0]
            .sessionId;
        }
        if (target) await get().loadSession(target);
      }
    },

    // Remote session-create (bridge 0.17.0, ADR-0014): start a NEW CLI
    // session in one of the machine's known projects. Creates (or reuses) a
    // headless serve bridge for the workspace, connects it with noAttach,
    // and opens a fresh session whose cwd is the project directory. The new
    // conversation counts as "no activity" on the bridge until its first
    // prompt lands it in discovery — normal, not an error.
    createProjectSession: async (workspacePath) => {
      const client = hub();
      if (!client) return false;
      let created: HubCreateInstanceResult;
      try {
        created = await client.createInstance(workspacePath);
      } catch (e) {
        set({
          notice:
            e instanceof HubApiError && e.status === 403
              ? "notice.projectUnknown"
              : `create session failed: ${e instanceof Error ? e.message : String(e)}`,
        });
        return false;
      }
      // Discovery refresh (best-effort) so the new instance shows in the
      // list; the connection itself does not depend on it.
      void get()
        .refreshInstances()
        .catch(() => undefined);
      await get().connectInstance(created.id, undefined, { noAttach: true });
      if (get().connState !== "open" || !acp) {
        // POST succeeded but the WS never opened (e.g. serve bridge died
        // between registration and connect); scheduleReconnect owns the
        // retry. Surface it or pick() silently restores its buttons.
        set({ notice: "create session failed: instance connection failed" });
        return false;
      }
      try {
        // cwd is REQUIRED by the ACP schema — a request without it is
        // rejected -32602 before the bridge's serve-mode handler (which
        // ignores the value in favor of the process cwd) ever runs.
        const result = (await acp.request("session/new", {
          cwd: workspacePath,
          mcpServers: [],
        })) as {
          sessionId: string;
          configOptions?: ConfigOption[];
          modes?: { currentModeId?: string };
        };
        set((s) => ({
          activeSessionId: result.sessionId,
          configOptions: result.configOptions ?? [],
          currentModeId: result.modes?.currentModeId ?? null,
          loadingSession: false,
          // Discovery skips the conversation until its first prompt — seed
          // it locally so the session list shows it right away.
          instances: mergeCreatedSession(s.instances, {
            instanceId: created.id,
            sessionId: result.sessionId,
            workspace: workspacePath,
          }),
        }));
        return true;
      } catch (e) {
        set({
          notice: `create session failed: ${e instanceof Error ? e.message : String(e)}`,
        });
        return false;
      }
    },

    // Resume a closed session from the project history listing (bridge
    // 0.19.0, ADR-0015): the store id (`sess_…`) rides the POST body so the
    // hub incubates a resume TUI that boots straight into the session
    // (ADR-0017) — a NEW instance, never the listing's live serve bridge,
    // unless the desktop app is present (app ADR-0010): then the hub answers
    // with the live bridge (`reused: true`) and no window opens. The attach
    // below session/loads the same id on whichever instance came back.
    resumeProjectSession: async (workspacePath, sessionId) => {
      const client = hub();
      if (!client) return false;
      let created: HubCreateInstanceResult;
      try {
        created = await client.createInstance(workspacePath, sessionId);
      } catch (e) {
        set({
          notice:
            e instanceof HubApiError && e.status === 403
              ? "notice.projectUnknown"
              : `resume session failed: ${e instanceof Error ? e.message : String(e)}`,
        });
        return false;
      }
      // Discovery refresh (best-effort) so the instance shows in the lists.
      void get()
        .refreshInstances()
        .catch(() => undefined);
      await get().connectInstance(created.id, sessionId);
      // The attach (session/load) sets activeSessionId synchronously before it
      // issues the request, so a non-null id is the same signal the old caller
      // used — kept explicit here rather than inferred from the route swap.
      if (get().connState !== "open" || !get().activeSessionId) {
        // POST succeeded but the WS never opened; scheduleReconnect owns the
        // retry. Surface it or pick() silently restores its buttons.
        set({ notice: "resume session failed: instance connection failed" });
        return false;
      }
      return true;
    },

    // One tap from the flat session list: may need to switch bridge instance
    // (new WS connection) before the session/load.
    openSession: async (instanceId, sessionId) => {
      const s = get();
      if (!s.profile) return;
      if (s.instanceId === instanceId && s.connState === "open") {
        if (s.activeSessionId !== sessionId) await get().loadSession(sessionId);
        return;
      }
      await get().connectInstance(instanceId, sessionId);
    },

    wakeProbe: () => {
      void wakeProbe();
    },

    closeRemoteSession: async (instanceId, sessionId) => {
      const client = hub();
      if (!client) return;
      try {
        await client.closeSession(instanceId, sessionId);
      } catch (e) {
        if (e instanceof HubApiError && e.status === 409) {
          set({ notice: "notice.sessionRunning" });
        } else {
          set({
            notice: `close session failed: ${e instanceof Error ? e.message : String(e)}`,
          });
        }
        return;
      }
      // Drop the row locally; the heartbeat list aligns within ~10s.
      set((state) => ({
        instances: state.instances.map((i) =>
          i.id === instanceId
            ? {
                ...i,
                sessions: (i.sessions ?? []).filter((s) => s.sessionId !== sessionId),
              }
            : i,
        ),
      }));
      // Closing the conversation we're looking at returns to the list
      // (connection stays, closeSession handles the session-scoped reset).
      if (get().instanceId === instanceId && get().activeSessionId === sessionId) {
        get().closeSession();
      }
      set({ notice: "notice.sessionClosed" });
      removeSessionCache(sessionId);
    },

    shutdownInstance: async (instanceId) => {
      const client = hub();
      if (!client) return;
      try {
        await client.shutdownInstance(instanceId);
      } catch (e) {
        set({
          notice: `shutdown instance failed: ${e instanceof Error ? e.message : String(e)}`,
        });
        return;
      }
      // Drop the instance locally; the heartbeat list aligns within ~10s.
      set((state) => ({
        instances: state.instances.filter((i) => i.id !== instanceId),
      }));
      if (get().instanceId !== instanceId) {
        set({ notice: "notice.instanceShutdown" });
        return;
      }
      // We were connected to it: same teardown as tryReconnect's "instance
      // gone" branch — the sessions died with the bridge (contract).
      connSeq++;
      acp?.close();
      acp = null;
      dropQueuedUpdates();
      set({
        connState: "idle",
        instanceId: null,
        activeSessionId: null,
        messages: [],
        planEntries: null,
        permissions: {},
        elicitations: {},
        notice: "notice.instanceShutdown",
        fsCapable: false,
        pageMarkerCapable: false,
        replayCursor: null,
        hasMore: false,
        totalMessages: null,
        loadingEarlier: false,
        configOptions: [],
        currentModeId: null,
        usage: null,
        cacheHit: null, turnStats: null,
        availableCommands: [],
        // usageStats stays (hub-level quota, not instance-scoped).
        sessionStates: {},
        loadingSession: false,
        isRunning: false,
      });
      reconnectAttempt = 0;
    },

    deleteSession: async (instanceId, sessionId) => {
      const client = hub();
      if (!client) return false;
      try {
        await client.deleteSession(instanceId, sessionId);
      } catch (e) {
        // 409 = the conversation is still live somewhere (an editor or
        // another client holds it) or a turn is in flight; stopping it is
        // stop's job, this route only deletes history.
        if (e instanceof HubApiError && e.status === 409) {
          set({ notice: "notice.sessionDeleteLive" });
        } else {
          set({
            notice: `delete session failed: ${e instanceof Error ? e.message : String(e)}`,
          });
        }
        return false;
      }
      set({ notice: "notice.sessionDeleted" });
      removeSessionCache(sessionId);
      return true;
    },

    deleteProject: async (workspacePath) => {
      const client = hub();
      if (!client) return false;
      try {
        await client.deleteProject(workspacePath);
      } catch (e) {
        set({
          notice: `delete project failed: ${e instanceof Error ? e.message : String(e)}`,
        });
        return false;
      }
      set({ notice: "notice.projectDeleted" });
      return true;
    },

    renameSession: async (instanceId, sessionId, title) => {
      const trimmed = title.trim().slice(0, 80);
      if (!trimmed) return;
      const client = hub();
      if (!client) return;
      try {
        await client.renameSession(instanceId, sessionId, trimmed);
      } catch {
        set({ notice: "notice.sessionRenameFailed" });
        return;
      }
      set((state) => ({
        instances: state.instances.map((i) =>
          i.id === instanceId
            ? {
                ...i,
                sessions: (i.sessions ?? []).map((s) =>
                  s.sessionId === sessionId ? { ...s, title: trimmed } : s,
                ),
              }
            : i,
        ),
      }));
      set({ notice: "notice.sessionRenamed" });
    },

    closeSession: () => {
      // Leave the open session but KEEP the instance connection: the list
      // screen renders live activity badges from sessionStates, which only
      // flow over the instance WS (broadcast-only, not in hub discovery).
      // Connection-level teardown (instance gone, forget hub) lives
      // elsewhere; this only resets session-scoped state. openSession's
      // fast path reuses the still-open socket.
      dropQueuedUpdates();
      const leaving = get().activeSessionId;
      // The leaving session's dialogs are gone — drop their respond handles
      // too (the bridge's race resolves elsewhere or dies with the turn).
      const leavingPerm = leaving ? get().permissions[leaving] : undefined;
      const leavingElicit = leaving ? get().elicitations[leaving] : undefined;
      if (leavingPerm) pendingResponds.delete(leavingPerm.requestId);
      if (leavingElicit) pendingElicitResponds.delete(leavingElicit.requestId);
      set((s) => {
        // Other sessions' pending dialogs stay: a request raised in session
        // A keeps waiting in the bridge's first-response-wins race, and the
        // awaitingPermission badge points the user back into A to answer.
        const permissions = { ...s.permissions };
        const elicitations = { ...s.elicitations };
        if (s.activeSessionId) {
          delete permissions[s.activeSessionId];
          delete elicitations[s.activeSessionId];
        }
        return {
          activeSessionId: null,
          messages: [],
          planEntries: null,
          permissions,
          elicitations,
          notice: null,
          replayCursor: null,
          hasMore: false,
          totalMessages: null,
          loadingEarlier: false,
          configOptions: [],
          currentModeId: null,
          usage: null,
          cacheHit: null, turnStats: null,
          availableCommands: [],
          loadingSession: false,
          isRunning: false,
        };
      });
    },

    loadSession: async (sessionId) => {
      if (!acp || get().connState !== "open") return;
      // Replay arrives as session/update notifications; reset first. A stale
      // local dialog for THIS session dies here (a still-pending request is
      // re-sent by the bridge after the load and lands as a fresh entry);
      // other sessions' dialogs are untouched.
      dropQueuedUpdates();
      // Cache hit: paint the snapshot right away (the composer stays locked
      // via loadingSession) and reconcile with a metadata-only attach below
      // instead of replaying the tail from scratch.
      const cached = readSessionCache(sessionId);
      set((s) => {
        const permissions = { ...s.permissions };
        const elicitations = { ...s.elicitations };
        const oldPerm = permissions[sessionId];
        const oldElicit = elicitations[sessionId];
        if (oldPerm) pendingResponds.delete(oldPerm.requestId);
        if (oldElicit) pendingElicitResponds.delete(oldElicit.requestId);
        delete permissions[sessionId];
        delete elicitations[sessionId];
        return {
          activeSessionId: sessionId,
          // A cached snapshot paints instantly and carries the last turn's
          // readouts (cacheHit/turnStats) with it — without them a switch or
          // wake-reconnect blanks the title-bar segments until the next
          // locally-run turn.
          ...(cached ?? {
            messages: [],
            planEntries: null,
            replayCursor: null,
            hasMore: false,
            totalMessages: null,
            configOptions: [],
            currentModeId: null,
            usage: null,
            cacheHit: null,
            turnStats: null,
          }),
          permissions,
          elicitations,
          availableCommands: [],
          loadingEarlier: false,
          loadingSession: true,
          isRunning: false,
        };
      });
      // True once the tail replay request is sent: the wire may have applied
      // part of its notification stream when a failure lands.
      let replaying = false;
      try {
        const ws = instanceWorkspace();
        const conn = acp;
        const load = (limit: number) =>
          conn.request("session/load", {
            sessionId,
            // cwd is REQUIRED by the SDK's session/load schema (no default) —
            // omitting it fails with -32602 Invalid params, e.g. when the hub
            // list is still stale right after a wake reconnect. The bridge
            // ignores the value on load (roots are backend-authoritative), so
            // an unknown workspace sends a "/" placeholder.
            cwd: ws ?? "/",
            mcpServers: [],
            // Tail replay: the limit rides in _meta (top-level unknown keys
            // are stripped by the SDK schema). Counts messages, turn-aligned.
            // limit 0 is the bridge's metadata-only attach: response metadata
            // only, no replay notifications.
            _meta: { zcode: { limit } },
          });

        if (cached) {
          // Reconcile the painted snapshot: "did anything happen while we
          // were gone". The tail id (bridge additive) is the primary key —
          // counts only refresh on replay, so a turn watched live leaves the
          // cached totalMessages stale and a count-only check would force a
          // full replay of an unchanged tail on every re-entry. Ids compare
          // verbatim: a thought-tailed turn has `thought_x` on both sides.
          // Bridges without the field, an id-less tail, or a LOCAL optimistic
          // tail (ensureMessage's m-counter never matches a backend id) fall
          // back to the count check: snapshots are written between turns, so
          // an equal count with an idle bridge turn means the history is what
          // we show.
          const result = await load(0);
          const meta = readReplayMeta(result);
          const serverTail = meta?.lastMessageId ?? null;
          const cachedTail = isLocalMessageId(cached.lastMessageId)
            ? null
            : cached.lastMessageId;
          const unchanged =
            serverTail != null && cachedTail != null
              ? serverTail === cachedTail
              : typeof meta?.totalMessages === "number" &&
                meta.totalMessages === cached.totalMessages;
          if (unchanged) {
            const res = result as {
              modes?: { currentModeId?: string };
              configOptions?: ConfigOption[];
            } | null;
            const configOptions = Array.isArray(res?.configOptions) ? res!.configOptions! : [];
            const currentModeId = res?.modes?.currentModeId ?? null;
            // Unchanged: keep the painted messages/cursor/hasMore — they
            // stay valid for load_earlier. The metadata-only cursor anchors
            // at history's END (a different convention than the tail slice's
            // START), so it must not be stored.
            set((s) =>
              s.activeSessionId === sessionId
                ? {
                    configOptions,
                    currentModeId,
                    loadingSession: false,
                    // A turn that survived the reconnect is still running on
                    // the bridge — restore the running UI, not the composer.
                    isRunning: meta?.turnActive === true,
                  }
                : {},
            );
            setActivity(sessionId, { running: meta?.turnActive === true });
            writeSessionCache(sessionId, {
              ...cached,
              // Keep the count from rotting even though the id carried the
              // verdict — an old-bridge fallback reconcile reads it next.
              totalMessages:
                typeof meta?.totalMessages === "number"
                  ? meta.totalMessages
                  : cached.totalMessages,
              configOptions,
              currentModeId,
            });
            flushPending();
            return;
          }
          // History moved while away: drop the painted snapshot and replay
          // the tail wholesale — merging the replay stream into the cached
          // messages would double the chunk parts of every shared message.
          set((s) =>
            s.activeSessionId === sessionId
              ? {
                  messages: [],
                  planEntries: null,
                  replayCursor: null,
                  hasMore: false,
                  totalMessages: null,
                }
              : {},
          );
        }

        replaying = true;
        const result = await load(REPLAY_TAIL_LIMIT);
        const meta = readReplayMeta(result);
        const res = result as {
          modes?: { currentModeId?: string };
          configOptions?: ConfigOption[];
        } | null;
        set({
          replayCursor: meta?.cursor ?? null,
          hasMore: meta?.hasMore ?? false,
          totalMessages: typeof meta?.totalMessages === "number" ? meta.totalMessages : null,
          configOptions: Array.isArray(res?.configOptions) ? res!.configOptions! : [],
          currentModeId: res?.modes?.currentModeId ?? null,
          loadingSession: false,
          // A turn that survived the reconnect is still running on the bridge
          // (replayMeta.turnActive): restore the running UI instead of the
          // idle composer, which would let a prompt collide with the turn.
          isRunning: meta?.turnActive === true,
        });
        setActivity(sessionId, { running: meta?.turnActive === true });
        cacheActiveSnapshot();
        // History is live: send whatever queued while the replay streamed
        // (flushPending no-ops while a restored turn is running — its
        // turnState end event does the flushing then).
        flushPending();
      } catch (e) {
        // Drop the in-flight batch AND the partially-applied replay: the
        // wire may have delivered half the tail notifications before the
        // failure, and flushing (or keeping) them paints a mid-conversation
        // stub that reads as reordered history until the next reload. The
        // reconnect replay is the only catch-up — until it lands, show
        // nothing rather than a half. A failed metadata-only reconcile
        // (replaying=false) keeps the painted snapshot: it is a complete,
        // self-consistent view, and the next reconnect retries the load.
        dropQueuedUpdates();
        set((s) => ({
          // Transient connection failures stay quiet: the reconnect banner
          // owns them, and the post-reconnect replay supersedes this error.
          ...(isTransientConnError(e)
            ? null
            : { notice: `session/load failed: ${(e as Error).message}` }),
          loadingSession: false,
          ...(s.activeSessionId === sessionId && replaying ? { messages: [] } : {}),
        }));
      }
    },

    // Fetches one page of older history and prepends it. Returns true when a
    // page was applied (the UI re-anchors its scroll position on this).
    loadEarlier: async () => {
      const s = get();
      if (!acp || s.connState !== "open") return false;
      if (
        !s.activeSessionId ||
        !s.replayCursor ||
        !s.hasMore ||
        // Mid-reconcile/replay: the cursor belongs to the about-to-be
        // replaced view; the page would land on the wrong history.
        s.loadingSession ||
        s.loadingEarlier
      )
        return false;
      const sessionId = s.activeSessionId;
      set({ loadingEarlier: true });
      collectingEarlier = true;
      earlierBuffer = [];
      earlierStreamLastAt = Date.now();
      try {
        const result = await acp.request("session/load_earlier", {
          sessionId,
          before: s.replayCursor,
          limit: EARLIER_PAGE_LIMIT,
        });
        if (get().pageMarkerCapable) {
          // Marked bridges close the window on the response: stragglers carry
          // the marker and take the late-page buffer (still a prepend).
          collectingEarlier = false;
        } else {
          // Legacy bridge: unmarked page updates and a response that can
          // overtake them — hold the window until the burst settles so the
          // whole page prepends instead of its tail appending.
          await settleEarlierWindow();
        }
        const page = earlierBuffer;
        earlierBuffer = [];
        // Build the page as its own segment (oldest -> newest), then prepend.
        set((state) => {
          if (state.activeSessionId !== sessionId) return state;
          let segment: ChatMessage[] = [];
          for (const { sessionId: sid, u, meta } of page) {
            if (sid !== sessionId) continue;
            const r = applyOne(segment, u, meta);
            if (r?.messages) segment = r.messages;
            // plan/config/usage in old pages are stale: take messages only
          }
          let rest = state.messages;
          if (segment.length && rest.length && segment[segment.length - 1].id === rest[0].id) {
            // Seam dedupe: the same message split across pages.
            const seam = segment[segment.length - 1];
            segment = segment.slice(0, -1);
            rest = [{ ...seam, parts: [...seam.parts, ...rest[0].parts] }, ...rest.slice(1)];
          }
          const meta = readReplayMeta(result);
          return {
            messages: [...segment, ...rest],
            replayCursor: meta?.cursor ?? null,
            hasMore: meta?.hasMore ?? false,
            loadingEarlier: false,
          };
        });
        cacheActiveSnapshot();
        return true;
      } catch (e) {
        collectingEarlier = false;
        earlierBuffer = [];
        set({ loadingEarlier: false });
        const msg = (e as Error).message ?? "";
        if (msg.includes("cursor expired")) {
          // History shrank (compaction): rebuild from a fresh tail attach.
          await get().loadSession(sessionId);
        } else if (!isTransientConnError(e)) {
          set({ notice: `load_earlier failed: ${msg}` });
        }
        return false;
      }
    },

    // Model / mode / thought switching via the bridge's configOptions.
    setConfigOption: async (configId, value) => {
      const s = get();
      if (!acp || s.connState !== "open" || !s.activeSessionId) return;
      try {
        const result = await acp.request("session/set_config_option", {
          sessionId: s.activeSessionId,
          configId,
          value,
        });
        const opts = (result as { configOptions?: ConfigOption[] } | null)?.configOptions;
        if (Array.isArray(opts)) set({ configOptions: opts });
        // The bridge also broadcasts config_option_update / current_mode_update
        // to every client (editor included) — the store picks those up too.
      } catch (e) {
        if (!isTransientConnError(e))
          set({ notice: `config change failed: ${(e as Error).message}` });
      }
    },

    // Combined quota via the hub's /api/quota (bridge 0.8.0, ADR-0005) — the
    // same payload the ACP account/usage_stats method returns, but plain
    // HTTP: no instance connection needed. Pull-only; fetched on hub connect
    // and on demand from the panels. 502/network failures land in the catch
    // (hide data, retry later). Per-provider failures arrive as section
    // `kind` strings (rendered like the CLI's status lines), not rejections.
    refreshUsageStats: async () => {
      const client = hub();
      const profile = get().profile;
      if (!client || !profile) return;
      void get().refreshQuotaDock();
      try {
        const result = await client.quota();
        if (get().profile !== profile) return; // hub changed mid-flight
        set({
          usageStats: parseUsageStats(result),
          quotaUnavailable: false,
          usageStatsAt: Date.now(),
        });
      } catch {
        if (get().profile === profile) set({ usageStats: null, quotaUnavailable: true });
      }
    },

    // Title-bar quota dock line (ADR-0021). Sticky last-known on failure —
    // a transient hub hiccup must not blank the resident line (the TUI's
    // refresher keeps it the same way); only a successful `null` clears it.
    refreshQuotaDock: async () => {
      const client = hub();
      const profile = get().profile;
      if (!client || !profile) return;
      try {
        const formatted = await client.quotaDock();
        if (get().profile === profile) set({ quotaDock: formatted });
      } catch {
        // keep the last line
      }
    },

    // ---- Session Files (ADR-0005) ----

    fsList: async (path) => {
      const client = hub();
      const s = get();
      if (!client || !s.instanceId || !s.activeSessionId) return null;
      try {
        return await client.fsList(s.instanceId, s.activeSessionId, path);
      } catch (e) {
        if (e instanceof HubApiError && e.instanceDead) {
          const revived = await reviveDeadInstance();
          const cur = get();
          if (revived && cur.instanceId && cur.activeSessionId) {
            try {
              return await client.fsList(cur.instanceId, cur.activeSessionId, path);
            } catch {
              return null;
            }
          }
        }
        return null;
      }
    },

    fsFileText: async (path, line, limit) => {
      const client = hub();
      const s = get();
      if (!client || !s.instanceId || !s.activeSessionId) return null;
      try {
        return await client.fsFileText(s.instanceId, s.activeSessionId, path, line, limit);
      } catch (e) {
        if (e instanceof HubApiError && e.instanceDead) {
          const revived = await reviveDeadInstance();
          const cur = get();
          if (revived && cur.instanceId && cur.activeSessionId) {
            try {
              return await client.fsFileText(
                cur.instanceId,
                cur.activeSessionId,
                path,
                line,
                limit,
              );
            } catch {
              return null;
            }
          }
        }
        return null;
      }
    },

    fsFileUrl: (path) => {
      const client = hub();
      const s = get();
      if (!client || !s.instanceId || !s.activeSessionId) return null;
      return client.fsFileUrl(s.instanceId, s.activeSessionId, path);
    },

    // Attach-only client: sessions are created in the editor, never here.
    // ACP allows one prompt at a time: while a turn is running, history is
    // still replaying, or the connection is down (phone slept mid-session),
    // new text queues as pending instead — the post-reconnect attach's
    // loadSession flushes it. The composer is cleared before this runs, so
    // dropping instead of queueing would lose the draft for good.
    sendPrompt: async (text, images) => {
      const s = get();
      if (!s.activeSessionId) {
        set({ notice: "notice.noSession" });
        return;
      }
      const draft: PromptDraft = { text, images: images ?? [] };
      if (!acp || s.connState !== "open" || s.isRunning || s.loadingSession) {
        setQueue(s.activeSessionId, [...(s.pendingPrompts[s.activeSessionId] ?? []), draft]);
        return;
      }
      await get().runPrompt(draft);
    },

    runPrompt: async (draft) => {
      const s = get();
      if (!acp || s.connState !== "open" || !s.activeSessionId || s.loadingSession) return;
      // Live turns never echo the user's message back (only replay does), so
      // insert it optimistically; replay replaces the whole history anyway.
      // Always a NEW message: two prompts sent back-to-back are two turns
      // even when the first produced no assistant output before the queue
      // flushed (response racing the trailing chunks, or a failed turn) —
      // appending to the previous user bubble merged them into one and
      // garbled the transcript.
      let msg = newMessage("user");
      if (draft.text) msg = appendTextPart(msg, draft.text, "text");
      for (const img of draft.images) {
        // Display form is the data URL; the wire block stays base64.
        msg = {
          ...msg,
          parts: [
            ...msg.parts,
            {
              type: "image",
              image: `data:${img.mimeType};base64,${img.data}`,
            } as ChatPart,
          ],
        };
      }
      set((state) => ({ messages: [...state.messages, msg] }));
      const sessionId = s.activeSessionId;
      const startedAt = Date.now();
      set({ isRunning: true });
      localPromptActive = true;
      // A server-rejected prompt (compaction busy window, "a prompt is
      // already running") never ran: the bubble is retracted and the draft
      // goes back to the HEAD of the queue — the next settle or a
      // force-send flushes it. No auto-flush below: a still-rejecting
      // bridge would livelock the queue into a reject-resend loop.
      let rejected = false;
      try {
        const result = (await acp.request("session/prompt", {
          sessionId,
          prompt: [
            ...(draft.text ? [{ type: "text", text: draft.text }] : []),
            ...draft.images.map((img) => ({
              type: "image",
              data: img.data,
              mimeType: img.mimeType,
            })),
          ],
        })) as { usage?: unknown } | null;
        // Per-turn usage rides the prompt response (UNSTABLE ACP field) AND,
        // on new bridges, the turn-end broadcast — both fold through the
        // same helper. Turns that report no cache numbers keep the previous
        // rate. Wall time includes tool calls (there is no LLM-only clock).
        applyTurnUsage(
          sessionId,
          result?.usage,
          Math.max((Date.now() - startedAt) / 1000, 0.1),
        );
      } catch (e) {
        if (isTransientConnError(e)) {
          // A dropped connection kills the in-flight prompt, but the
          // reconnect replay rebuilds the truth — stay quiet and let the
          // banner speak. The turn may have run, so the draft is NOT
          // re-queued (a re-send could duplicate it).
        } else {
          rejected = true;
          set((state) => ({
            messages: state.messages.filter((m) => m.id !== msg.id),
          }));
          setQueue(sessionId, [draft, ...(get().pendingPrompts[sessionId] ?? [])]);
          set({ notice: `prompt failed: ${(e as Error).message}` });
        }
      } finally {
        localPromptActive = false;
        set({ isRunning: false });
        cacheActiveSnapshot();
        // Quota moved with the turn — refresh the dock line (TUI does the
        // same at every turn end; hub caches 15s so retries stay cheap).
        void get().refreshQuotaDock();
        // Auto-flush: the queue is FIFO, and each settled turn sends the next.
        // A concurrent re-attach defers to loadSession's own flush instead.
        // A rejected prompt skips it — its draft just went back to the head.
        if (!rejected) flushPending();
      }
    },

    forceSendPending: (index) => {
      const s = get();
      const sid = s.activeSessionId;
      if (!sid) return;
      const queue = s.pendingPrompts[sid] ?? [];
      if (queue.length === 0) return;
      // During replay there is no turn to interrupt; the queue fires on load.
      if (s.loadingSession) return;
      // Disconnected: runPrompt would silently swallow the draft — the
      // post-reconnect flush owns the queue until the socket is back.
      if (s.connState !== "open") return;
      // Any bubble's button sends THAT draft, not the head: move it to the
      // front (order of the rest is untouched). Own running turn: cancel and
      // let the settle-flush pick it up. Foreign/restored turn: cancel AND
      // flush now — its settle goes to the connection that prompted it.
      const i = Math.min(Math.max(index ?? 0, 0), queue.length - 1);
      const reordered = [
        queue[i],
        ...queue.filter((_, j) => j !== i),
      ];
      if (s.isRunning) {
        setQueue(sid, reordered);
        get().cancelTurn();
        if (!localPromptActive) {
          // Foreign/restored turn: the prompt response goes to the prompting
          // connection and a turn-end broadcast may never arrive here either
          // — waiting for the settle-flush dead-ends the queue. The cancel
          // stops the turn; anything still draining is preempted by the
          // prompt itself, so flush the head immediately.
          set({ isRunning: false });
          cacheActiveSnapshot();
          flushPending();
        }
      } else {
        setQueue(sid, reordered.slice(1));
        void get().runPrompt(reordered[0]);
      }
    },

    discardPending: (index) => {
      const s = get();
      const sid = s.activeSessionId;
      if (!sid) return;
      const queue = s.pendingPrompts[sid] ?? [];
      if (index < 0 || index >= queue.length) return;
      setQueue(
        sid,
        queue.filter((_, i) => i !== index),
      );
    },

    editPending: (index) => {
      const s = get();
      const sid = s.activeSessionId;
      if (!sid) return;
      const queue = s.pendingPrompts[sid] ?? [];
      if (index < 0 || index >= queue.length) return;
      const draft = queue[index];
      setQueue(
        sid,
        queue.filter((_, i) => i !== index),
      );
      prefillSeq += 1;
      set({
        composerPrefill: { text: draft.text, images: draft.images, nonce: prefillSeq },
      });
    },

    cancelTurn: () => {
      const s = get();
      if (!acp || !s.activeSessionId) return;
      // session/cancel is a NOTIFICATION in ACP (no id, no response).
      acp.notify("session/cancel", { sessionId: s.activeSessionId });
    },

    answerPermission: (requestId, optionId) => {
      const respond = pendingResponds.get(requestId);
      if (!respond) return;
      pendingResponds.delete(requestId);
      const cleared: string[] = [];
      set((s) => {
        const permissions = { ...s.permissions };
        for (const [sid, p] of Object.entries(permissions)) {
          if (p.requestId === requestId) {
            delete permissions[sid];
            cleared.push(sid);
          }
        }
        return { permissions };
      });
      for (const sid of cleared) setActivity(sid, { awaitingPermission: false });
      respond({ outcome: { outcome: "selected", optionId } });
    },

    answerElicitation: (requestId, content) => {
      const respond = pendingElicitResponds.get(requestId);
      if (!respond) return;
      pendingElicitResponds.delete(requestId);
      const cleared: string[] = [];
      set((s) => {
        const elicitations = { ...s.elicitations };
        for (const [sid, e] of Object.entries(elicitations)) {
          if (e.requestId === requestId) {
            delete elicitations[sid];
            cleared.push(sid);
          }
        }
        return { elicitations };
      });
      for (const sid of cleared) setActivity(sid, { awaitingPermission: false });
      // Wire shape (elicitation/create): accept carries the form content,
      // anything else declines. Absent fields = skipped questions.
      respond(content ? { action: "accept", content } : { action: "decline" });
    },

    dismissNotice: () => set({ notice: null }),

    notify: (text) => {
      const id = ++toastSeq;
      set({ toast: { id, text } });
      if (toastTimer) clearTimeout(toastTimer);
      // A newer toast (higher id) resets the timer; the stale one is a no-op.
      toastTimer = setTimeout(() => {
        toastTimer = null;
        set((s) => (s.toast?.id === id ? { toast: null } : s));
      }, TOAST_MS);
    },

    dismissToast: () => {
      if (toastTimer) {
        clearTimeout(toastTimer);
        toastTimer = null;
      }
      set({ toast: null });
    },

    // ---- ZCode configuration (ADR-0009) ----
    // Config-screen state and actions live in ./configSlice.
    ...createConfigSlice(set, get, hub),

    // ---- dynamic workflows (bridge 0.48.0, server ADR-0029) ----

    setComposerPrefill: (text) => {
      // Monotonic nonce: the same prompt text requested twice must still be
      // distinguishable, or the second request would not re-trigger the
      // composer's consumption effect.
      prefillSeq += 1;
      set({ composerPrefill: { text, nonce: prefillSeq } });
    },

    clearComposerPrefill: () => set({ composerPrefill: null }),

    workflowAction: async (label, fn, silent = false, onError, iidOverride) => {
      const client = hub();
      const instanceId = iidOverride ?? get().instanceId;
      if (!client || !instanceId) {
        if (!silent) get().notify(`${label} failed: not connected`);
        onError?.("not connected");
        return null;
      }
      try {
        return await fn(client, instanceId);
      } catch (e) {
        const message = e instanceof Error ? e.message : String(e);
        if (!silent) get().notify(`${label} failed: ${message}`);
        onError?.(message);
        return null;
      }
    },

    startWorkflow: async (input) => {
      const res = await get().workflowAction(
        "start workflow",
        (client, iid) =>
          client.workflowStart(iid, input.scope, input.name, {
            ...(input.args ? { args: input.args } : {}),
            ...(input.sessionId ? { sessionId: input.sessionId } : {}),
          }),
        false,
        undefined,
        input.instanceId,
      );
      if (!res) return null;
      // Remember the join the response just minted: `runId → acpSessionId`
      // exists nowhere else the app can read it (history rows carry the
      // BACKEND session id), and without it the run's rows could never be
      // opened, inspected, or resumed from this app later.
      if (res.runId) {
        rememberLaunch({
          runId: res.runId,
          acpSessionId: res.acpSessionId,
          instanceId: input.instanceId ?? get().instanceId ?? "",
          name: input.name,
          scope: input.scope,
          at: Date.now(),
        });
      }
      return res;
    },

    resumeWorkflowRun: async (input) => {
      const res = await get().workflowAction(
        "resume run",
        (client, iid) =>
          client.workflowResume(iid, input.runId, {
            sessionId: input.sessionId,
            ...(input.name ? { name: input.name } : {}),
          }),
        false,
        undefined,
        input.instanceId,
      );
      return res !== null;
    },

    amendWorkflowRun: async (input) => {
      return get().workflowAction(
        "amend run settings",
        (client, iid) => client.amendRunSettings(iid, input.runId, input.sessionId, input.body),
        false,
        undefined,
        input.instanceId,
      );
    },

    stopWorkflowRun: async (runId, sessionId, instanceId) => {
      // The hub route reaches a run on ANY instance; the ACP socket below is
      // bound to the connected one only.
      if (instanceId) {
        const res = await get().workflowAction(
          "stop run",
          (client, iid) => client.workflowStop(iid, runId, { sessionId }),
          false,
          undefined,
          instanceId,
        );
        return res !== null;
      }
      const conn = acp;
      if (!conn) {
        get().notify("stop run failed: not connected");
        return false;
      }
      try {
        const result = (await conn.request("session/cancelBackgroundTask", {
          sessionId,
          taskId: runId,
        })) as { cancelled?: boolean } | null;
        if (result?.cancelled !== true) {
          get().notify("stop run failed: run not found or already settled");
          return false;
        }
        return true;
      } catch (e) {
        get().notify(`stop run failed: ${e instanceof Error ? e.message : String(e)}`);
        return false;
      }
    },

    loadRunSummaries: async (sessionIds, iidOverride) => {
      const client = hub();
      const instanceId = iidOverride ?? get().instanceId;
      if (!client || !instanceId) return {};
      const out: Record<string, ConversationRunSummary[]> = {};
      await Promise.allSettled(
        sessionIds.map(async (sessionId) => {
          const res = await client.conversationRuns(instanceId, sessionId);
          if (res?.runs) out[sessionId] = res.runs;
        }),
      );
      return out;
    },

    openWorkflowRun: (target) => {
      set({ workflowRunTarget: target });
      get().openConfig("workflows");
    },

    clearWorkflowRunTarget: () => set({ workflowRunTarget: null }),

    refreshWorkflowHubIfLoaded: () => {
      if (get().workflowHub !== null) void get().loadWorkflowHub();
    },

    // The management page's machine-level list, served PRE-AGGREGATED by the
    // hub (/api/workflow-overview — bridge 0.60.0): one global group + one
    // project group per WORKSPACE (the hub dedupes the editor/serve bridge
    // pair and races the reads), each joined hub-side with its per-name
    // newest journal row, plus the machine-wide active-run list. The app
    // renders the answer as-is — no cross-instance merging here.
    loadWorkflowHub: async () => {
      const client = hub();
      if (!client) return;
      set({ workflowHubLoading: true });
      try {
        const res = await client.workflowOverview();
        set({
          workflowHub: res.groups ?? [],
          workflowActiveRuns: res.activeRuns ?? [],
          workflowRecentRuns: res.recentRuns ?? [],
          workflowHubError: null,
          workflowHubLoading: false,
        });
      } catch (e) {
        // 404 = the hub PREDATES the overview route: a version gap, not a
        // transient failure — surface the actionable "restart the hub" story.
        // Anything else is a blip: keep the last groups, no error paint over
        // working content (the FIRST load of a broken connection still needs
        // a story, so an empty list with nothing yet loaded takes the error).
        const message =
          e instanceof HubApiError && e.status === 404
            ? "the hub does not serve the workflow overview yet — restart the hub to update it"
            : e instanceof Error
              ? e.message
              : String(e);
        const hadData = get().workflowHub !== null;
        set({
          ...(hadData ? {} : { workflowHub: [], workflowActiveRuns: [], workflowRecentRuns: [] }),
          workflowHubError: hadData ? get().workflowHubError : message,
          workflowHubLoading: false,
        });
      }
    },
  };
});

// Zombie-socket guard: deep-sleep wake is the one moment connState can lie
// ("open" socket that died mid-sleep without an onclose). Validate on every
// foreground return — the probe is one tiny round-trip on a healthy pipe.
// The interval covers the wedge that lands AFTER the wake probe already ran
// (e.g. a reconnect attempt stuck on an unbounded await): without it the
// banner can stick on "reconnecting" until the user exits and re-enters,
// because nothing else re-examines the loop while the screen stays on.
if (typeof document !== "undefined") {
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") useAppStore.getState().wakeProbe();
  });
  setInterval(() => {
    if (document.visibilityState === "visible")
      useAppStore.getState().wakeProbe();
  }, 20_000);
}
