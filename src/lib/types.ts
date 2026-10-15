// Hub discovery API types (REMOTE-CLIENTS.md contract; fields are additive-only).

export interface HubSessionInfo {
  sessionId: string;
  title?: string;
  updatedAt?: number;
  // Coarse heartbeat indicator (bridge 0.8.0, ADR-0005): "running" | "idle",
  // up to ~10s stale. The live WS broadcast (sessionStates) wins when a
  // connection exists; this is the no-connection fallback for the list.
  status?: string;
}

export interface HubInstance {
  id: string;
  port?: number;
  pid?: number;
  startedAt?: number;
  workspace?: string;
  // How the instance was started (bridge 0.17.0, ADR-0014): "editor" = a
  // bridge an editor spawned over stdio; "serve" = a headless bridge created
  // via remote session-create. Absent on older bridges — treat as "editor".
  origin?: "editor" | "serve";
  sessions: HubSessionInfo[];
  [key: string]: unknown;
}

// GET /api/projects (bridge 0.17.0, ADR-0014): one known project workspace —
// every directory the machine's App tasks index has ever recorded a session
// for (temp trees and ~/.zcode filtered out server-side). This list is also
// the create whitelist: POST /api/instances refuses paths outside it.
export interface HubProject {
  workspacePath: string;
  sessions: number;
  lastActive: number;
}

// GET /api/fs/list (bridge 0.65.0, browse-and-create): one directory level
// of the hub machine's filesystem. `path` is the RESOLVED (realpath)
// spelling — navigate by it, not by what was sent. Only subdirectories are
// listed; hidden (`.`-prefixed) entries are included, filter client-side.
// `creatable` = a NEW session may start in the listed directory itself
// (known project, or under a configured remote.projectRoots root).
export interface FsBrowseResult {
  path: string;
  parent: string | null;
  creatable: boolean;
  entries: Array<{ name: string }>;
  truncated: boolean;
}

// POST /api/instances result: the (new or reused) serve instance id.
export interface HubCreateInstanceResult {
  id: string;
  reused: boolean;
}

// One row of GET /api/projects/sessions (bridge 0.19.0, server ADR-0015):
// the project's backend session store, closed conversations included.
export interface HubHistorySession {
  sessionId: string;
  title?: string;
  cwd?: string;
  // The store's ISO timestamp — NOT discovery's epoch-ms number.
  updatedAt?: string;
  // live = currently advertised by a bridge (discovery membership);
  // running = a turn is in flight. Both absent on the wire when false.
  live?: boolean;
  running?: boolean;
  // Journal workflow activity joined bridge-side (bridge 0.57.0): runs of
  // this session still pending/running in the journal's newest window, plus
  // the newest run. Absent = gate off / no runs / older bridge.
  workflowActivity?: {
    active: number;
    last?: { status: string; updatedAt?: number; name?: string };
  };
  [key: string]: unknown;
}

// Composite "load more" cursor. Pass BOTH fields back verbatim — `before`
// alone would drop rows whose timestamps tie across a page boundary.
export interface HubHistoryCursor {
  before: number;
  beforeId: string;
}

export interface HubHistoryPage {
  workspacePath: string;
  // The serve instance a follow-up resume attaches to.
  instance: { id: string; origin?: "editor" | "serve" };
  sessions: HubHistorySession[];
  // null = no older sessions (last page).
  nextCursor: HubHistoryCursor | null;
}

export interface ConnectionProfile {
  hubUrl: string;
  token: string;
}

// One entry of the persisted multi-server book: the connection pair plus a
// user-editable display name. Exactly one entry is active at a time;
// `profile` in the store mirrors that entry.
export interface SavedServer {
  id: string;
  name: string;
  hubUrl: string;
  token: string;
}

// POST /api/upgrade (bridge 0.11.7): the hub's own staleness-check verdict.
// `restarting` means IT decided the on-disk code is newer — the client only
// triggered the check; the decision was never ours to make.
export interface HubUpgradeResult {
  ok: boolean;
  restarting: boolean;
  reason: "version" | "mtime" | "up-to-date";
  runningVersion: string;
  diskVersion: string | null;
}

// Session Files (bridge 0.7.0, ADR-0005): one directory level per call;
// entries sort dirs-first in byte order server-side.
export interface FsEntry {
  name: string;
  kind: "file" | "dir" | "symlink";
  size: number;
  mtime?: number;
}

export interface FsListing {
  root: string;
  entries: FsEntry[];
  truncated: boolean;
}

// Session config (session/load result + config_option_update). Shape matches
// the bridge's buildConfigOptions output (id: model | mode | thought).
export interface ConfigOption {
  id: string;
  name?: string;
  category?: string;
  currentValue?: string;
  options?: { value: string; name: string }[];
}

// usage_update session/update payload — the editor's context bar data.
export interface ContextUsage {
  used: number;
  size: number;
}

// Last completed turn's speed readout (title bar): output tokens, wall
// seconds, and their ratio. Seeded only by a locally-run turn's prompt
// response; persisted in the session snapshot so switches/reconnects keep it.
export interface TurnStats {
  tps: number;
  outputTokens: number;
  seconds: number;
}

// account/usage_stats (Proposal 0002) — the combined dual-provider quota
// behind the zcode-quota CLI: one GLM section plus one Opencode Go section.
// GLM items pass through verbatim (counts, reset timestamps, per-model
// details); Go windows carry the countdown resolved to absolute epoch ms.
export interface QuotaItem {
  key: string;
  label: string;
  usedPercent: number;
  usedCount?: number;
  totalCount?: number;
  nextResetTime?: number;
  detail?: { modelCode: string; usage: number }[];
}
export interface GlmUsageStats {
  kind: "success" | "auth_error" | "rate_limited" | "unavailable";
  level?: string;
  items?: QuotaItem[];
}

export interface GoWindowEntry {
  key: string;
  label: string;
  usagePercent: number;
  resetsAt: number;
}

export interface GoUsageStats {
  kind: "success" | "not_configured" | "auth_error" | "unavailable";
  windows?: GoWindowEntry[];
}

// Ollama Cloud section (bridge 0.41.0): whichever windows the account's plan
// exposes (legacy: 5h + Week; credit plans: Month). ollama.com returns
// fractions only — the reset moments are derived hub-side (bridge 0.42.0)
// and ride along when available (monthly drops out when its lookup fails).
export interface OcWindowEntry {
  key: string;
  label: string;
  usagePercent: number;
  resetsAt?: number;
}

export interface OllamaUsageStats {
  kind: "success" | "not_configured" | "auth_error" | "unavailable";
  windows?: OcWindowEntry[];
}

export interface AccountUsageStats {
  glm: GlmUsageStats;
  opencode: GoUsageStats;
  // Optional: hub payloads before bridge 0.41.0 carry no ollama section.
  ollama?: OllamaUsageStats;
}

// available_commands_update entry (ACP AvailableCommand shape): the bridge's
// slash commands, advertised per session after load (overwrite semantics).
export interface SlashCommand {
  name: string;
  description?: string;
  input?: { hint?: string };
}

// ACP wire types. Deliberately permissive: unknown kinds/fields must be
// ignored. The SessionUpdate union is discriminated by `sessionUpdate`
// (per @agentclientprotocol schema), NOT by `kind`.

export interface SessionUpdate {
  sessionUpdate: string;
  [key: string]: unknown;
}

export interface AcpTextContent {
  type: "text";
  text: string;
}

// Edit/Write results ship structured diffs (old/new line sets reconstructed
// from the backend's patch — NOT a unified-diff string).
export interface AcpDiffContent {
  type: "diff";
  path: string;
  oldText: string | null;
  newText: string;
}

function isTextContent(c: unknown): c is AcpTextContent {
  return (
    typeof c === "object" &&
    c !== null &&
    (c as { type?: string }).type === "text"
  );
}

// The bridge wraps plain results as {type:"content", content:{type:"text"}}
// (Bash output, plan text, question text); unwrap those too.
function unwrapText(c: unknown): string | null {
  if (isTextContent(c)) return c.text;
  if (
    typeof c === "object" &&
    c !== null &&
    (c as { type?: string }).type === "content"
  ) {
    const inner = (c as { content?: unknown }).content;
    if (isTextContent(inner)) return inner.text;
  }
  return null;
}

function isDiffContent(c: unknown): c is AcpDiffContent {
  if (typeof c !== "object" || c === null) return false;
  const d = c as Partial<AcpDiffContent>;
  return (
    d.type === "diff" &&
    typeof d.newText === "string" &&
    typeof d.path === "string"
  );
}

// Chunk content is a SINGLE ContentBlock on the wire; tool_call_update
// content is an ARRAY of blocks. Accept both.
export function contentText(content: unknown): string {
  if (!content) return "";
  const blocks: unknown[] = Array.isArray(content) ? content : [content];
  return blocks
    .map(unwrapText)
    .filter((t): t is string => t !== null)
    .join("");
}

export function contentDiffBlocks(content: unknown): AcpDiffContent[] {
  if (!content) return [];
  const blocks: unknown[] = Array.isArray(content) ? content : [content];
  return blocks.filter(isDiffContent);
}

// Internal chat model, converted to assistant-ui ThreadMessageLike at render time.

export interface ToolCallPart {
  type: "tool-call";
  toolCallId: string;
  toolName: string;
  detail: string;
  status: string;
  // ACP tool kind (execute/edit/read/search/fetch/switch_mode/other…) for
  // icon + colour mapping.
  kind?: string;
  // Raw backend tool name from `_meta.claudeCode.toolName` (e.g. "Bash"),
  // distinct from `toolName` which holds the wire title "Bash: npm test".
  rawName?: string;
  // Replay-only harness fold (`_meta.zcode.kind` on `histfold_` tool_calls,
  // server 0.6.0): "context-handoff" | "tool-transcript". The full plumbing
  // text rides `detail` behind the card's expand.
  foldKind?: string;
  diffs?: AcpDiffContent[];
}

export type ChatPart =
  | { type: "text"; text: string }
  | { type: "thought"; text: string }
  // Prompt attachment echo: `image` holds a display data URL.
  | { type: "image"; image: string }
  | ToolCallPart;

// A compressed prompt attachment ready for the wire (ADR 0007): base64
// without the data-url prefix — exactly the ACP image block payload.
export interface AttachmentDraft {
  data: string;
  mimeType: string;
}

// A queued prompt: text plus staged attachments (persisted across restarts).
export interface PromptDraft {
  text: string;
  images: AttachmentDraft[];
}

export interface ChatMessage {
  id: string;
  role: "user" | "assistant";
  parts: ChatPart[];
  createdAt: number;
  // Replay-only: harness-injected context handoff rendered collapsed instead
  // of a wall of user text (`_meta.zcode.collapsed` on the update).
  collapsed?: boolean;
}

// ---- ZCode configuration (bridge 0.47.0, server ADR-0025) ----
//
// The payloads are deliberately loose where the bridge is free to add fields
// (models, skills, hooks, agents): unknown keys must survive a round trip, and
// a strict client type would silently drop them on the way back out. Only the
// fields the app reads or writes are named.

/** What a configuration write costs to take effect. */
export type Effect = "immediate" | "needs-restart";

/** Every write answers with its effect class. */
export interface WriteEffect {
  ok: boolean;
  effect?: Effect;
  error?: string;
}

/**
 * Body of `POST /settings/models` — an upsert: it adds a model to a provider
 * or edits its personal rule. Only the fields present change; the route maps
 * `contextWindow` into `properties` and `reasoningLevels` into `optionSpecs`.
 */
export interface ModelUpsert {
  providerId: string;
  modelId: string;
  enabled?: boolean;
  contextWindow?: number;
  reasoningLevels?: string[];
}

/**
 * Body of `PUT /settings/mcp/{name}`. The route MERGES this into the existing
 * entry, so only the fields the form actually edits are sent. `type` decides
 * which transport fields the runtime reads: stdio uses command/args/env,
 * http and sse use url/headers.
 */
export interface McpServerUpsert {
  type?: string;
  command?: string;
  args?: string[];
  url?: string;
  env?: Record<string, string>;
  headers?: Record<string, string>;
  enabled?: boolean;
}

/**
 * Body of `PUT /settings/hooks/{event}/{matcherIndex}` (`hookIndex` rides in
 * the body — the path takes only two segments). The route edits ONE existing
 * entry and merges the patch: `enabled: true` removes the pin rather than
 * storing it, `timeoutMs` (ms) wins over `timeout` (seconds) when both exist.
 */
export interface HookEntryPatch {
  command?: string;
  enabled?: boolean;
  timeoutMs?: number;
  timeout?: number;
}

/**
 * Body of `PUT /settings/agents/{name}`. A personal agent patches frontmatter
 * fields (`model`/`thoughtLevel` accept null to clear the key); a built-in
 * agent instead reads a model override, and `providerId`/`modelId` must be
 * null TOGETHER to clear it. A PUT on a missing personal agent creates it —
 * then `description` is required.
 */
export interface AgentUpsert {
  description?: string;
  color?: string;
  model?: string | null;
  thoughtLevel?: string | null;
  providerId?: string | null;
  modelId?: string | null;
  reasoningLevel?: string;
}

/**
 * One-shot snapshot of every section, for a config page's first paint.
 *
 * The two optional sections degrade independently: a machine that never ran an
 * agent has no local database, and one that never linked a coding plan has no
 * decryptable credential store — both are normal, not errors.
 */
export interface SettingsAll {
  ok: boolean;
  models: unknown;
  skills: unknown;
  mcp: unknown;
  hooks: unknown;
  agents: unknown;
  usage: SettingsUsage | null;
  resetCards: { providers: string[]; credentials: boolean; reason?: string };
  /**
   * Dynamic-workflow gate verdict (bridge 0.48.0, ADR-0029). Absent on an
   * older bridge — read as "off", which hides the workflows section.
   * `source:"override"` + `override` (bridge ≥0.53) mark a local override;
   * `mode:"unknown"` means the verdict could not be resolved (fail-closed).
   */
  workflow?: WorkflowGateBlock;
}

/** The gate verdict block (GET /settings/all and PUT /settings/workflow-gate). */
export interface WorkflowGateBlock {
  enabled: boolean;
  mode: string;
  source: string;
  /** Present only under a local override: which position produced this. */
  override?: "disabled" | "onDemand" | "alwaysOn";
}

/** The switch's write vocabulary (bridge ≥0.53): "auto" follows the remote verdict. */
export type WorkflowGateModeSetting =
  "auto" | "disabled" | "onDemand" | "alwaysOn";

export interface WorkflowGateSetResponse {
  ok: boolean;
  /** The effective verdict the next gate read sees. */
  gate: WorkflowGateBlock;
}

export interface SettingsUsage {
  available: boolean;
  range: "7d" | "30d" | "all";
  summary: { totalTokens: number; requestCount: number; models: number };
  models: Array<{
    modelId: string;
    totalTokens: number;
    inputTokens?: number;
    outputTokens?: number;
    reasoningTokens?: number;
    cacheReadTokens?: number;
    requestCount?: number;
    share?: number;
  }>;
  daily: Array<{
    date: string;
    models: Array<{ modelId: string | null; totalTokens: number }>;
  }>;
}

// ---- platform usage (bridge GET /settings/usage-platform) ----
// The account-level monitor data behind the desktop app's Coding Plan usage
// tab: a fixed 365-day activity window plus range-scoped credit/model/tool
// detail. See .zcode/docs/platform-usage-backend-requirements.md for the
// frozen contract; only `kind === "success"` carries the optional blocks.

/** One heatmap square. `level` is 0-4 (0 = no usage), precomputed by the bridge. */
export interface PlatformUsageHeatmapCell {
  date: string;
  level: number;
  tokens: number;
}

export interface PlatformUsageDetailSummary {
  /** Percent, e.g. 72.4; null when the upstream did not report it. */
  cacheHitRate: number | null;
  /** Period-over-period change of the metric; sign carries direction. */
  cacheHitRateTrend: number | null;
  totalCredits: number;
  totalCreditsTrend: number | null;
  averageDailyCredits: number;
  averageDailyCreditsTrend: number | null;
}

export interface SettingsPlatformUsage {
  kind: "success" | "auth_error" | "rate_limited" | "unavailable";
  range: "today" | "7d" | "30d";
  generatedAt: number;
  activity?: {
    summary: {
      totalTokens: number;
      peakDailyTokens: number;
      peakDailyTokensDate: string | null;
      totalUsageDurationMs: number;
      currentStreakDays: number;
      longestStreakDays: number;
    };
    heatmap: {
      startDate: string | null;
      endDate: string | null;
      maxTokens: number;
      /** Sunday-start weeks; missing days inside a week are null cells. */
      weeks: Array<{ days: Array<PlatformUsageHeatmapCell | null> }>;
    };
  };
  detail?: {
    model: PlatformUsageDetailSummary | null;
    tool: PlatformUsageDetailSummary | null;
  };
  models?: Array<{
    name: string;
    totalTokens: number;
    totalCredits?: number;
    cachedInputTokens?: number;
    uncachedInputTokens?: number;
    outputTokens?: number;
    sortOrder: number;
  }>;
  tools?: Array<{
    name: string;
    totalUsageCount: number;
    totalCredits?: number;
  }>;
  /** Aggregate tokens per day (or per hour today) over the selected range. */
  series?: { granularity: "hour" | "day"; xTime: string[]; totals: number[] };
}

/**
 * The reset-card inventory. A card carries an expiry and nothing else — the
 * API exposes no card number or denomination, so a list can only be counted
 * and dated.
 */
export interface ResetCardStatus {
  ok: boolean;
  resetCards: {
    availableFiveHour: Array<{ expireAt: number }>;
    availableWeek: Array<{ expireAt: number }>;
    latestFiveHour: { usedAt: number } | null;
    latestWeek: { usedAt: number } | null;
    hasUnreadHistory: boolean;
    /** Send back verbatim with a spend; ties it to this status read. */
    nonce: string;
  };
}

// ---- dynamic-workflow management (bridge 0.48.0, server ADR-0029) ----
//
// Shapes of the per-instance settings routes under
// /api/instances/{id}/settings/workflows* and /settings/workflow-runs*.
// Read-heavy and append-only: the backend is the authority, the app renders
// what arrives and never infers fields the route did not send.

/** Workflow scope vocabulary (upstream workflows/* `scope`). */
export type WorkflowScope = "project" | "global";

export interface WorkflowEntry {
  name: string;
  description?: string;
  scope?: string;
}

export interface WorkflowListResponse {
  ok: boolean;
  workflows: WorkflowEntry[];
  invalid: Array<{ path?: string; reason?: string }>;
  /** The scanned directory, returned even when it does not exist yet. */
  dir?: string;
}

export interface WorkflowDetailResponse {
  ok: boolean;
  name: string;
  path?: string;
  scope: WorkflowScope;
  meta?: Record<string, unknown>;
  script?: string;
}

/**
 * One history row from the journal (`workflows/runs` — cross-restart, not
 * session-bound). `parentSessionId` is the BACKEND session id; `acpSessionId`
 * is the attachable alias the bridge joined onto it (bridge 0.57.0) — present
 * means an editor-launched run is actionable (open session / resume) without
 * this app's own launch memory.
 * Lineage fields ride the upstream row extension (bridge passthrough already
 * pinned); absent until the upstream journal projection lands.
 */
export interface WorkflowRunRow {
  runId: string;
  name?: string;
  status: string;
  stopReason?: string;
  createdAt?: number;
  updatedAt?: number;
  spentTokens?: number;
  parentSessionId?: string;
  acpSessionId?: string;
  toolCallId?: string;
  args?: Record<string, unknown>;
  cwd?: string;
  artifacts?: Array<{ id?: string; kind?: string; title?: string }>;
  resumedFrom?: string;
  supersededBy?: string;
  /**
   * Hub annotation on the overview's active runs (bridge 0.60.0): the
   * instance whose live session list currently holds this run's session —
   * the address for stop/resume from a client attached elsewhere. Absent
   * when no instance lists the session (fall back to launch memory).
   */
  ownerInstanceId?: string;
}

export interface WorkflowRunsHistoryResponse {
  ok: boolean;
  runs: WorkflowRunRow[];
  truncated?: boolean;
}

/**
 * The hub's machine-level workflow page payload (`/api/workflow-overview`,
 * bridge 0.60.0): one group per WORKSPACE (a project with an editor + serve
 * bridge renders once — the hub dedupes), each pre-joined with its per-name
 * newest journal row, plus the machine-wide active-run list (the journal is
 * shared across projects; a client attached to one instance must still see
 * another project's flying runs). The app renders this as-is — no client-side
 * merging or dedupe.
 */
export interface WorkflowOverviewResponse {
  ok: boolean;
  groups: WorkflowHubGroup[];
  activeRuns: WorkflowRunRow[];
  /**
   * The same journal read's finished complement (terminal statuses, newest
   * first, hub-capped): the top-level "recently finished" list's feed — a
   * settled run otherwise only resurfaces inside its workflow's history.
   */
  recentRuns: WorkflowRunRow[];
}

/**
 * The hub's machine-level system status (`/api/system-stats`, bridge
 * 0.62.0): host identity, CPU/memory/storage, battery + power assertions,
 * network, and the hub's own + every registered bridge's process resources.
 * Every field is independently best-effort — a probe the hub could not run
 * (denied inside a sandbox, absent on this hardware) arrives as null and the
 * page shows a placeholder, never an error. Rate fields (CPU %, network B/s,
 * hub CPU %) are deltas the hub computes across reads: the FIRST read after
 * a hub start reports null for them.
 */
export interface SystemStatsResponse {
  collectedAt: number;
  host: {
    hostname: string;
    /** e.g. "15.8 (24H23)" — null when `sw_vers` was unavailable. */
    osVersion: string | null;
    model: string | null;
    chip: string | null;
  };
  uptimeS: number;
  cpu: {
    cores: number;
    usagePct: number | null;
    loadAvg: [number, number, number];
  };
  memory: {
    totalBytes: number;
    availableBytes: number | null;
    usedBytes: number | null;
    swapTotalBytes: number | null;
    swapUsedBytes: number | null;
  };
  storage: {
    /** null when statfs failed; `home` is null again when it is the same volume as root. */
    root: SystemStatsVolume | null;
    home: SystemStatsVolume | null;
  };
  battery: {
    /** False on a desktop machine (no battery). */
    present: boolean;
    percent: number | null;
    powerSource: "ac" | "battery" | null;
    /** Raw pmset wording: charging / discharging / charged / … */
    status: string | null;
    remainingMin: number | null;
  };
  power: {
    /** True while something holds PreventUserIdleSystemSleep — the machine will not idle-sleep. */
    preventSleep: boolean | null;
    sleepHolders: string[];
    /** Thermal CPU speed limit (100 = full speed; null = no reading recorded). */
    cpuSpeedLimitPct: number | null;
  };
  network: {
    addresses: string[];
    ssid: string | null;
    rxBytesPerS: number | null;
    txBytesPerS: number | null;
  };
  processes: {
    hub: { pid: number; rssBytes: number; cpuPct: number | null };
    bridges: Array<{
      id: string;
      workspace: string | null;
      pid: number | null;
      rssBytes: number | null;
      cpuPct: number | null;
    }>;
  };
  hub: { version: string; uptimeS: number; instances: number };
}

/** One filesystem volume of the system-status page. */
export interface SystemStatsVolume {
  totalBytes: number;
  usedBytes: number;
  availableBytes: number;
}

/**
 * One group of the management page's cross-instance list (desktop hub
 * parity): the GLOBAL workflows (one machine-wide group — `instanceId` is
 * whichever instance answered) or one hub instance's PROJECT workflows.
 * `lastRuns` is the per-name newest journal row (the list badge source,
 * desktop `lastRunByWorkflowName` semantics — model-renamed runs that match
 * no saved name are simply not joined). `error` marks an instance that
 * refused (gate off, offline, old bridge) — the group still renders.
 */
export interface WorkflowHubGroup {
  instanceId: string;
  scope: WorkflowScope;
  /** Display label: the instance's workspace basename (global group uses the scope label). */
  workspace: string;
  workflows: WorkflowEntry[];
  invalid: Array<{ path?: string; reason?: string }>;
  lastRuns: Record<string, WorkflowRunRow>;
  error: string | null;
}

/**
 * One declared arg on a saved workflow's meta (`args` map, upstream
 * `zcodeSavedWorkflowArgsDeclarationSchema`): drives the typed launch form.
 */
export interface WorkflowArgDeclaration {
  type?: string;
  description?: string;
  required?: boolean;
  default?: unknown;
}

/** `POST /settings/workflow-runs/{runId}/settings` (bridge 0.49.0, PR #265). */
export interface AmendRunSettingsResponse {
  ok: boolean;
  /** The run that continues the work — same id in place, NEW id after supersede. */
  runId: string;
  toolCallId: string;
  /** Present only when the amendment stopped and superseded a flying run. */
  supersededRunId?: string;
}

/** A session-scoped run summary (`v4 workflowRuns` via the bridge). */
export interface ConversationRunSummary {
  runId: string;
  toolCallId?: string;
  label?: string;
  updatedAt?: number;
  status: string;
  stopReason?: string;
  resumedFrom?: string;
  supersededBy?: string;
  failureCode?: string;
  failureMessage?: string;
  /** cancelled ∪ failed-Interrupted — the resume button's source of truth. */
  resumable: boolean;
}

export interface ConversationRunsResponse {
  ok: boolean;
  runs: ConversationRunSummary[];
}

/** One journal event; `sequence` is the append-only cursor. */
export interface WorkflowRunEvent {
  sequence: number;
  type: string;
  payload: Record<string, unknown>;
  truncated?: boolean;
}

export interface WorkflowRunEventsResponse {
  ok: boolean;
  events: WorkflowRunEvent[];
  hasMore?: boolean;
}

/**
 * One artifact. Kind-specific latest fields sit at the top level (kept open
 * via the index signature); `versions` carries history (≤16).
 */
export interface WorkflowArtifact {
  id: string;
  kind: "file" | "markdown" | "chart" | "table" | "metrics" | "board" | string;
  title?: string;
  versions?: Array<Record<string, unknown>>;
  itemCount?: number;
  primary?: boolean;
  [key: string]: unknown;
}

export interface WorkflowArtifactItemsResponse {
  ok: boolean;
  items: Array<{
    sequence?: number;
    siteId?: string;
    ordinal?: number;
    item: unknown;
  }>;
  hasMore?: boolean;
}

export interface WorkflowArtifactReadResponse {
  ok: boolean;
  dataBase64: string;
  mediaType?: string;
  totalBytes?: number;
  nextOffset?: number;
}

/**
 * A workspace node's summary is a strict OBJECT upstream (v4
 * workflow-workspace schema), never a string — which fields are set depends
 * on the op (arrays carry resultCount, world.run carries exitCode and the
 * two output streams, string bodies only resultBytes).
 */
export interface WorkflowNodeSummary {
  resultBytes: number;
  resultCount?: number;
  exitCode?: number;
  stdoutBytes?: number;
  stderrBytes?: number;
}

export interface WorkflowWorkspaceNode {
  siteId?: string;
  ordinal?: number;
  op?: string;
  args?: unknown;
  status?: string;
  summary?: WorkflowNodeSummary;
  createdAt?: number;
  updatedAt?: number;
}

export interface WorkflowWorkspaceResponse {
  ok: boolean;
  nodes: WorkflowWorkspaceNode[];
  truncated?: boolean;
}

export interface WorkflowNodeResultResponse {
  ok: boolean;
  status?: string;
  result?: unknown;
  error?: unknown;
  truncated?: boolean;
  totalBytes?: number;
}

/** `POST …/start` — `acpSessionId` is what openSession() consumes. */
export interface WorkflowStartResponse {
  ok: boolean;
  acpSessionId: string;
  runId?: string;
  toolCallId?: string;
}

export interface WorkflowCreatePromptResponse {
  ok: boolean;
  prompt: string;
}

/** The install outcome of an app update, as the bridge reports it. */
export type AppUpdateStage =
  | "idle"
  | "downloading"
  | "installing"
  | "done"
  | "needs-user-install"
  | "failed";

export interface AppUpdateState {
  stage: AppUpdateStage;
  version: string | null;
  receivedBytes: number;
  totalBytes: number | null;
  restartRequired?: boolean;
  artifactPath?: string;
  error?: string;
}
