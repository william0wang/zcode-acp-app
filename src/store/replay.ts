// Pure replay helpers extracted from appStore: message projection
// (session/update -> chat model), replay pagination metadata, and the
// per-session replay cache. No store access lives here — everything operates
// on the arrays/values it is handed.
import {
  contentDiffBlocks,
  contentText,
  rawInputText,
  type ChatMessage,
  type ChatPart,
  type ConfigOption,
  type ContextUsage,
  type SessionUpdate,
  type SlashCommand,
  type ToolCallPart,
  type TurnStats,
} from "../lib/types";

// Server-side tail replay (REMOTE-CLIENTS.md "Tail replay and history
// pagination"): attach ships only the last REPLAY_TAIL_LIMIT messages;
// older pages arrive via session/load_earlier.
export const REPLAY_TAIL_LIMIT = 30;
export const EARLIER_PAGE_LIMIT = 50;

// Per-session replay cache: a wake-reconnect or a switch back paints the
// snapshot instantly and reconciles with a metadata-only attach (limit 0)
// instead of a full tail replay. Module-level on purpose — it never drives a
// render, so it stays out of React state.
interface SessionSnapshot {
  messages: ChatMessage[];
  planEntries: PlanEntry[] | null;
  replayCursor: string | null;
  hasMore: boolean;
  totalMessages: number | null;
  // Tail message id (see snapshotTailId) — the primary reconcile key.
  // Counts only refresh on replay, so a turn watched live leaves
  // totalMessages stale while the tail id is already the new one.
  lastMessageId: string | null;
  configOptions: ConfigOption[];
  currentModeId: string | null;
  usage: ContextUsage | null;
  // Last turn's readouts ride the snapshot too: without them a switch or
  // wake-reconnect blanks the title-bar cache/tps segments until the next
  // locally-run turn, while context (usage above) paints instantly.
  cacheHit: number | null;
  turnStats: TurnStats | null;
  // Taken between turns. Only a settled snapshot may skip the reconcile on an
  // equal tail: mid-turn the last message can still be growing under the
  // same id, and an optimistic prompt may never have reached the bridge.
  settled: boolean;
}

const SESSION_CACHE_LIMIT = 6;
const sessionCache = new Map<string, SessionSnapshot>();

export function readSessionCache(sessionId: string): SessionSnapshot | null {
  const hit = sessionCache.get(sessionId);
  if (!hit) return null;
  // LRU refresh: re-insert so eviction order reflects recency.
  sessionCache.delete(sessionId);
  sessionCache.set(sessionId, hit);
  return hit;
}

export function writeSessionCache(
  sessionId: string,
  snap: SessionSnapshot,
): void {
  sessionCache.delete(sessionId);
  sessionCache.set(sessionId, snap);
  while (sessionCache.size > SESSION_CACHE_LIMIT) {
    const oldest = sessionCache.keys().next().value;
    if (oldest === undefined) break;
    sessionCache.delete(oldest);
  }
}

export function removeSessionCache(sessionId: string): void {
  sessionCache.delete(sessionId);
}

/**
 * Id of the newest chat message — the snapshot-side reconcile key. Compared
 * VERBATIM against the bridge's `replayMeta.lastMessageId`: that field is the
 * newest journal entry's id as-is, and a thought IS its own journal entry
 * with a `thought_`-prefixed id — so a turn ending on a thought has
 * `thought_x` on both sides. Stripping the prefix here (the old behavior)
 * mismatched exactly that case and forced a redundant full replay on every
 * re-entry of the session.
 */
export function snapshotTailId(messages: ChatMessage[]): string | null {
  return messages[messages.length - 1]?.id ?? null;
}

/**
 * Local optimistic message ids (ensureMessage's `m1`, `m2`, … counter). They
 * can never equal a backend/bridge id (bridge-generated ids are UUIDs), so a
 * local tail id carries no reconcile signal — the caller falls back to the
 * count check. A backend id of this exact shape would only demote itself to
 * that same conservative fallback, never skip a needed replay.
 */
export function isLocalMessageId(id: string | null): boolean {
  return id !== null && /^m\d+$/.test(id);
}

interface ReplayMeta {
  cursor?: string;
  hasMore?: boolean;
  totalMessages?: number;
  // Bridge additive: id of the newest message at the slice's end anchor.
  // Preferred reconcile key over totalMessages (which drifts stale after
  // turns watched live); absent on older bridges.
  lastMessageId?: string;
  // Bridge flag: a turn is still in flight for this session (someone else's
  // prompt, or one that survived our reconnect) — restore the running UI.
  turnActive?: boolean;
}

export function readReplayMeta(result: unknown): ReplayMeta | null {
  if (typeof result !== "object" || result === null) return null;
  const meta = (result as { replayMeta?: unknown }).replayMeta;
  return typeof meta === "object" && meta !== null
    ? (meta as ReplayMeta)
    : null;
}

// zcode-acp-server version that started marking load_earlier page updates
// `_meta.zcode.earlierPage` — the floor for the marker-based page routing.
const PAGE_MARKER_BRIDGE_VERSION = "0.44.1";

// Semver-ish floor check on the bridge's advertised agentInfo.version.
// Unparseable versions read as NOT capable: the fallback (pre-marker
// buffer-everything collection) is the safe behavior on any bridge.
export function bridgeSupportsPageMarker(version: unknown): boolean {
  if (typeof version !== "string") return false;
  const parts = version.split(".");
  if (parts.length < 3) return false;
  const nums = parts.slice(0, 3).map((p) => Number.parseInt(p, 10));
  if (nums.some((n) => Number.isNaN(n))) return false;
  const [major, minor, patch] = nums;
  const floor = PAGE_MARKER_BRIDGE_VERSION.split(".").map((p) =>
    Number.parseInt(p, 10),
  ) as [number, number, number];
  if (major !== floor[0]) return major > floor[0];
  if (minor !== floor[1]) return minor > floor[1];
  return patch >= floor[2];
}

// One todo/plan entry; `status` is "pending" | "active" | "completed".
export interface PlanEntry {
  content: string;
  status?: string;
}

// Optimistic message ids for locally inserted bubbles (runPrompt's optimistic
// user message). Module-level counter, exactly as it was in appStore.
let msgCounter = 0;

// ---- session/update -> chat model ----

// Fresh bottom message sharing msgCounter with replay, so live and replayed
// ids never collide.
export function newMessage(role: "user" | "assistant"): ChatMessage {
  return {
    id: `m${++msgCounter}`,
    role,
    parts: [],
    createdAt: Date.now(),
  };
}

export function ensureMessage(
  msgs: ChatMessage[],
  role: "user" | "assistant",
): { messages: ChatMessage[]; message: ChatMessage } {
  const last = msgs[msgs.length - 1];
  if (last && last.role === role) return { messages: msgs, message: last };
  const message = newMessage(role);
  return { messages: [...msgs, message], message };
}

export function appendTextPart(
  message: ChatMessage,
  text: string,
  partType: "text" | "thought",
): ChatMessage {
  const lastPart = message.parts[message.parts.length - 1];
  if (lastPart && lastPart.type === partType) {
    const parts = [
      ...message.parts.slice(0, -1),
      { type: partType, text: lastPart.text + text } as ChatPart,
    ];
    return { ...message, parts };
  }
  return {
    ...message,
    parts: [...message.parts, { type: partType, text } as ChatPart],
  };
}

// With a messageId, chunks group into that exact message (backend ids are
// stable and dedupe across replay pages; thought ids carry a `thought_`
// prefix and form their own stream). Without one, fall back to merging
// into the trailing same-role message.
export function appendChunkMessage(
  msgs: ChatMessage[],
  u: SessionUpdate,
  role: "user" | "assistant",
  text: string,
  partType: "text" | "thought",
): ChatMessage[] {
  const mid =
    typeof u.messageId === "string" && u.messageId ? u.messageId : null;
  if (mid) {
    const i = msgs.findIndex((m) => m.id === mid);
    if (i >= 0)
      return patchMessage(msgs, i, appendTextPart(msgs[i], text, partType));
    return [
      ...msgs,
      {
        id: mid,
        role,
        parts: [{ type: partType, text } as ChatPart],
        createdAt: Date.now(),
      },
    ];
  }
  const ensured = ensureMessage(msgs, role);
  return ensured.messages.map((m) =>
    m.id === ensured.message.id ? appendTextPart(m, text, partType) : m,
  );
}

export function patchMessage(
  msgs: ChatMessage[],
  i: number,
  m: ChatMessage,
): ChatMessage[] {
  return msgs.slice(0, i).concat(m, msgs.slice(i + 1));
}

// `_meta.zcode.collapsed` on a replayed user chunk (context handoff and
// similar harness blocks) — the UI renders those behind an expand control.
export function isCollapsedMeta(meta: unknown): boolean {
  if (typeof meta !== "object" || meta === null) return false;
  const zcode = (meta as { zcode?: { collapsed?: unknown } }).zcode;
  return (
    typeof zcode === "object" && zcode !== null && zcode.collapsed === true
  );
}

// `_meta.zcode.earlierPage` on session/load_earlier paged updates — the
// bridge marks page replays so live-turn updates streaming while the page
// request is in flight can be told apart (they must append, not ride the
// prepend). Only consulted while pageMarkerCapable is true; older bridges
// fall back to buffering everything during the collection window.
export function isEarlierPageMeta(meta: unknown): boolean {
  if (typeof meta !== "object" || meta === null) return false;
  const zcode = (meta as { zcode?: { earlierPage?: unknown } }).zcode;
  return (
    typeof zcode === "object" && zcode !== null && zcode.earlierPage === true
  );
}

// `_meta.zcode.mid` on a replayed chunk (rewind spec §4.3): the backend's
// stable message id, the handle session/edit_user_message and
// session/retry_turn address. Live echoes carry none — absence is the
// "not addressable" spelling. Both placements are read (notification-level
// meta param first, then the update's own _meta), mirroring the
// collapsed/earlierPage flags.
export function zcodeMid(u: SessionUpdate, meta?: unknown): string | null {
  const read = (v: unknown): string | null => {
    if (typeof v !== "object" || v === null) return null;
    const zcode = (v as { zcode?: { mid?: unknown } }).zcode;
    return typeof zcode === "object" &&
      zcode !== null &&
      typeof zcode.mid === "string" &&
      zcode.mid
      ? zcode.mid
      : null;
  };
  return read(meta) ?? read(u._meta);
}

// Locates the tool_call part the bridge emits right before an interaction
// request; its content holds the plan/question text (ADR 0003).
export function findToolCallPart(
  msgs: ChatMessage[],
  toolCallId: string,
): ToolCallPart | null {
  for (let i = msgs.length - 1; i >= 0; i--) {
    const m = msgs[i];
    if (m.role !== "assistant") continue;
    for (let j = m.parts.length - 1; j >= 0; j--) {
      const p = m.parts[j];
      if (p.type === "tool-call" && p.toolCallId === toolCallId) return p;
    }
  }
  return null;
}

// Same lookup, but returns message/part indices so the caller can replace
// the part in place (tool_call dedup on insert).
export function findToolCallPartIndex(
  msgs: ChatMessage[],
  toolCallId: string,
): { mi: number; pi: number } | null {
  if (!toolCallId) return null;
  for (let i = msgs.length - 1; i >= 0; i--) {
    const m = msgs[i];
    if (m.role !== "assistant") continue;
    for (let j = m.parts.length - 1; j >= 0; j--) {
      const p = m.parts[j];
      if (p.type === "tool-call" && p.toolCallId === toolCallId)
        return { mi: i, pi: j };
    }
  }
  return null;
}

// Applies one update; returns a partial state patch, or null when nothing
// changed. Message kinds operate on the given array; session-level kinds
// (config/usage) carry their own values.
export function applyOne(
  msgs: ChatMessage[],
  u: SessionUpdate,
  meta?: unknown,
): {
  messages?: ChatMessage[];
  planEntries?: PlanEntry[] | null;
  configOptions?: ConfigOption[];
  currentModeId?: string;
  usage?: ContextUsage;
  availableCommands?: SlashCommand[];
} | null {
  const kind = u.sessionUpdate;

  if (kind === "user_message_chunk" || kind === "agent_message_chunk") {
    const role = kind === "user_message_chunk" ? "user" : "assistant";
    const text = contentText(u.content);
    if (!text) return null;
    let next = appendChunkMessage(msgs, u, role, text, "text");
    // Stamp the rewind handle onto the chunk's message (replay only — see
    // zcodeMid). The mid names the SAME message the chunk's own messageId
    // groups by, so the lookup cannot miss on the create path.
    const wireMid =
      typeof u.messageId === "string" && u.messageId ? u.messageId : null;
    const rewindMid = zcodeMid(u, meta);
    if (wireMid && rewindMid) {
      const i = next.findIndex((m) => m.id === wireMid && !m.mid);
      if (i >= 0) next = patchMessage(next, i, { ...next[i], mid: rewindMid });
    }
    if (role === "user" && isCollapsedMeta(meta)) {
      const mid = typeof u.messageId === "string" ? u.messageId : "";
      const i = next.findIndex((m) => m.id === mid && !m.collapsed);
      if (i >= 0)
        next = patchMessage(next, i, { ...next[i], collapsed: true });
    }
    return { messages: next };
  }
  if (kind === "agent_thought_chunk") {
    const text = contentText(u.content);
    if (!text) return null;
    return {
      messages: appendChunkMessage(msgs, u, "assistant", text, "thought"),
    };
  }
  if (kind === "tool_call") {
    const updateMeta = u._meta as
      | {
          claudeCode?: { toolName?: unknown };
          zcode?: { collapsed?: unknown; kind?: unknown };
        }
      | undefined;
    const meta = updateMeta?.claudeCode;
    // Replay harness folds (server 0.6.0): handoff summaries and rewritten
    // tool transcripts arrive as completed tool_calls flagged in _meta.zcode.
    const fold =
      updateMeta?.zcode?.collapsed === true &&
      typeof updateMeta.zcode.kind === "string"
        ? updateMeta.zcode.kind
        : null;
    // The bridge ships the plan/question text as the tool_call's initial
    // content — keep it in detail so approval cards can show it.
    const part = {
      type: "tool-call" as const,
      toolCallId: String(u.toolCallId ?? ""),
      toolName: String(u.title ?? "tool"),
      detail: contentText(u.content),
      status: String(u.status ?? "pending"),
      ...(typeof u.kind === "string" ? { kind: u.kind } : {}),
      ...(fold ? { foldKind: fold } : {}),
      ...(typeof meta?.toolName === "string"
        ? { rawName: meta.toolName }
        : {}),
      ...(() => {
        // Filled at creation only: an update never carries rawInput.
        const ri = rawInputText(u.rawInput);
        return ri ? { rawInput: ri } : {};
      })(),
      ...(() => {
        const diffs = contentDiffBlocks(u.content);
        return diffs.length ? { diffs } : {};
      })(),
    };
    const mid =
      typeof u.messageId === "string" && u.messageId ? u.messageId : null;
    // Dedupe on every insert (REPLAY-GUIDE contract): a reannounced
    // interaction (e.g. a still-pending plan approval) reuses its
    // toolCallId, and replay can deliver the same historical call again —
    // assistant-ui keys parts by toolCallId and throws on duplicates.
    // Replace the existing part in place instead of appending a second.
    const dupAt = findToolCallPartIndex(msgs, part.toolCallId);
    if (dupAt) {
      const { mi, pi } = dupAt;
      return {
        messages: patchMessage(msgs, mi, {
          ...msgs[mi],
          parts: msgs[mi].parts.map((p, j) => (j === pi ? part : p)),
        }),
      };
    }
    if (mid) {
      const i = msgs.findIndex((m) => m.id === mid);
      if (i >= 0) {
        return {
          messages: patchMessage(msgs, i, {
            ...msgs[i],
            parts: [...msgs[i].parts, part],
          }),
        };
      }
      return {
        messages: [
          ...msgs,
          {
            id: mid,
            role: "assistant",
            parts: [part],
            createdAt: Date.now(),
          },
        ],
      };
    }
    // A message-id-less tool_call is a live-stream card: the model's own
    // call, or sub-agent/background activity rolled up into this session's
    // event stream (arbitrary timing vs the host's streaming text). Splicing
    // a trailing message that is mid-text splits the sentence in two — the
    // next chunk of that message opens a new part AFTER the card. A dedicated
    // card message keeps the text whole (id'd chunks keep appending to their
    // own message, above the cards).
    const last = msgs[msgs.length - 1];
    const lastPart = last?.parts[last.parts.length - 1];
    const endsText =
      last?.role === "assistant" &&
      (lastPart?.type === "text" || lastPart?.type === "thought");
    let messages = msgs;
    let targetId: string;
    if (endsText) {
      const fresh = newMessage("assistant");
      targetId = fresh.id;
      messages = [...msgs, fresh];
    } else {
      const ensured = ensureMessage(msgs, "assistant");
      targetId = ensured.message.id;
      messages = ensured.messages;
    }
    return {
      messages: messages.map((m) =>
        m.id === targetId ? { ...m, parts: [...m.parts, part] } : m,
      ),
    };
  }
  if (kind === "tool_call_update") {
    const toolCallId = String(u.toolCallId ?? "");
    const chunk = contentText(u.content);
    const diffBlocks = contentDiffBlocks(u.content);
    // Terminal-channel Bash (enabled while an editor client is attached):
    // output streams as _meta.terminal_output.data deltas — append them so
    // the card keeps its output; the terminal_exit update then only closes.
    const termData = (
      u._meta as { terminal_output?: { data?: unknown } } | undefined
    )?.terminal_output?.data;
    const termText = typeof termData === "string" ? termData : null;
    // Non-terminal tools may carry a rawOutput string without content blocks.
    const rawOut = typeof u.rawOutput === "string" ? u.rawOutput : null;
    // An update carrying content blocks REPLACES the whole collection
    // (text and diffs); a status-only update leaves them untouched.
    const hasContent =
      u.content != null &&
      (!Array.isArray(u.content) || u.content.length > 0);
    const status = typeof u.status === "string" ? u.status : null;
    // Search backwards: the tool call may live in an earlier assistant
    // message when other clients' turns interleaved.
    for (let i = msgs.length - 1; i >= 0; i--) {
      const m = msgs[i];
      if (m.role !== "assistant") continue;
      if (
        !m.parts.some(
          (p) => p.type === "tool-call" && p.toolCallId === toolCallId,
        )
      ) {
        continue;
      }
      const patched = {
        ...m,
        parts: m.parts.map((p) =>
          p.type === "tool-call" && p.toolCallId === toolCallId
            ? {
                ...p,
                detail: termText
                  ? p.detail + termText
                  : chunk || rawOut || p.detail,
                status: status ?? p.status,
                ...(hasContent
                  ? { diffs: diffBlocks.length ? diffBlocks : undefined }
                  : {}),
              }
            : p,
        ),
      };
      return {
        messages: msgs.slice(0, i).concat(patched, msgs.slice(i + 1)),
      };
    }
    return null;
  }
  if (kind === "plan") {
    // Plan updates are full snapshots; render the latest one as a live
    // status area outside the message stream.
    const raw = Array.isArray(u.entries) ? u.entries : [];
    const entries = raw
      .map((e) => {
        const entry = e as { content?: string; status?: string };
        return {
          content: entry.content ?? "",
          ...(typeof entry.status === "string"
            ? { status: entry.status }
            : {}),
        } satisfies PlanEntry;
      })
      .filter((e) => e.content);
    return { planEntries: entries.length ? entries : null };
  }
  if (kind === "config_option_update") {
    const opts = Array.isArray(u.configOptions)
      ? (u.configOptions as ConfigOption[])
      : null;
    return opts ? { configOptions: opts } : null;
  }
  if (kind === "current_mode_update") {
    return typeof u.currentModeId === "string"
      ? { currentModeId: u.currentModeId }
      : null;
  }
  if (kind === "usage_update") {
    const used = typeof u.used === "number" ? u.used : null;
    const size = typeof u.size === "number" ? u.size : null;
    return used != null && size != null ? { usage: { used, size } } : null;
  }
  if (kind === "available_commands_update") {
    // Full snapshot, overwrite semantics — the bridge re-sends it after
    // each session/load, so keep the latest list.
    const list = Array.isArray(u.availableCommands)
      ? u.availableCommands
      : null;
    if (!list) return null;
    const commands = list.filter(
      (c): c is SlashCommand =>
        typeof c === "object" &&
        c !== null &&
        typeof (c as SlashCommand).name === "string",
    );
    return { availableCommands: commands };
  }
  // Unknown kinds (additive-only contract): ignore.
  return null;
}

// ---- session re-entry: merging a tail replay into the painted view ----

// One session/update notification held back from the store while a tail
// replay is captured (see loadSession).
export interface CapturedUpdate {
  u: SessionUpdate;
  meta?: unknown;
}

/**
 * Folds a captured run the way flushUpdateBatch applies a live batch, but
 * builds messages only from `from` on. Session-level kinds are snapshots, so
 * every item's counts; message kinds before `from` are not part of the batch.
 */
export function foldCapturedUpdates(
  items: CapturedUpdate[],
  from: number,
): {
  messages: ChatMessage[];
  planEntries?: PlanEntry[] | null;
  usage?: ContextUsage;
  availableCommands?: SlashCommand[];
} {
  let messages: ChatMessage[] = [];
  let planEntries: PlanEntry[] | null | undefined;
  let usage: ContextUsage | undefined;
  let availableCommands: SlashCommand[] | undefined;
  items.forEach(({ u, meta }, i) => {
    const r = applyOne(i >= from ? messages : [], u, meta);
    if (!r) return;
    if (i >= from && r.messages) messages = r.messages;
    if (r.planEntries !== undefined) planEntries = r.planEntries;
    if (r.usage) usage = r.usage;
    if (r.availableCommands) availableCommands = r.availableCommands;
  });
  return {
    messages,
    ...(planEntries !== undefined ? { planEntries } : {}),
    ...(usage ? { usage } : {}),
    ...(availableCommands ? { availableCommands } : {}),
  };
}

// Whether a message update's target is already among `messages`: the chunk's
// message, or the tool call's card. Session-level kinds carry no target.
export function updateOnScreen(
  u: SessionUpdate,
  messages: ChatMessage[],
): boolean {
  const kind = u.sessionUpdate;
  if (kind === "tool_call" || kind === "tool_call_update") {
    const id = String(u.toolCallId ?? "");
    return id !== "" && findToolCallPart(messages, id) !== null;
  }
  const mid =
    typeof u.messageId === "string" && u.messageId ? u.messageId : null;
  return mid !== null && messages.some((m) => m.id === mid);
}

// The painted object when both spellings are the same message, else null.
// createdAt is capture time, so it never decides. The rewind handle rides
// replay only and is borrowed onto the painted object — which is kept, not
// replaced: assistant-ui caches conversions per object, so a message the
// replay confirms never re-renders.
function keepPainted(a: ChatMessage, b: ChatMessage): ChatMessage | null {
  if (a.id !== b.id || a.role !== b.role) return null;
  if (JSON.stringify(a.parts) !== JSON.stringify(b.parts)) return null;
  if ((a.collapsed ?? undefined) !== (b.collapsed ?? undefined)) return null;
  return b.mid && a.mid !== b.mid ? { ...a, mid: b.mid } : a;
}

// A user prompt in its two spellings: the sender's own bubble (or the live
// echo of another client's) and the stored message — same role, same text.
function samePrompt(a: ChatMessage, b: ChatMessage): boolean {
  if (a.id === b.id) return true;
  if (a.role !== "user" || b.role !== "user") return false;
  const text = (m: ChatMessage) =>
    m.parts.reduce(
      (t, p) => (p.type === "text" ? t + p.text : t),
      "",
    );
  return text(a) === text(b);
}

/**
 * Merges a captured tail replay into the painted messages — null when the
 * view is Far Behind (no shared Anchor) or the two cannot be aligned, the
 * caller's cue to reload.
 *
 * From the Seam on, the replay's spelling wins — except for messages both
 * agree on (kept as painted) and, when `carryInFlight`, for output the
 * journal has not taken yet. Above the Seam the painted messages stay.
 */
export function mergeTailReplay(
  painted: ChatMessage[],
  replay: ChatMessage[],
  carryInFlight: boolean,
): ChatMessage[] | null {
  const replayIds = new Map<string, number>();
  const replayTools = new Map<string, number>();
  replay.forEach((r, j) => {
    replayIds.set(r.id, j);
    for (const p of r.parts)
      if (p.type === "tool-call") replayTools.set(p.toolCallId, j);
  });
  // Assistant text and thoughts keep the backend's message id from the live
  // stream into replay; a card keeps its call id. Local bubbles and prompt
  // echoes carry neither, so they never anchor.
  const counterpart = (m: ChatMessage): number => {
    if (!isLocalMessageId(m.id) && replayIds.has(m.id))
      return replayIds.get(m.id)!;
    for (const p of m.parts)
      if (p.type === "tool-call" && replayTools.has(p.toolCallId))
        return replayTools.get(p.toolCallId)!;
    return -1;
  };
  let k = -1;
  let j = -1;
  for (let i = painted.length - 1; i >= 0; i--) {
    const c = counterpart(painted[i]);
    if (c < 0) continue;
    // The Seam is the earliest painted message mapping into the region the
    // newer ones map into. One row sits at two places in the two spellings
    // (a card split off its text live, a step's thought and text), so a
    // painted message whose counterpart lies AFTER a newer one's is part of
    // the replaced region, not a firm anchor.
    if (k >= 0) {
      if (c < j) break;
    } else {
      j = c;
    }
    k = i;
  }
  if (k < 0) return null;

  const out = painted.slice(0, k);
  // Above the Seam the replay only knows more about a card that changed
  // state while we were away (a background call finishing): take its
  // spelling of that card.
  for (let a = 0; a < k; a++) {
    const m = painted[a];
    let parts: ChatPart[] | null = null;
    m.parts.forEach((p, i) => {
      if (p.type !== "tool-call") return;
      const c = replayTools.get(p.toolCallId);
      if (c === undefined) return;
      const r = replay[c].parts.find(
        (q): q is ToolCallPart =>
          q.type === "tool-call" && q.toolCallId === p.toolCallId,
      );
      if (!r || r.status === p.status) return;
      if (!parts) parts = [...m.parts];
      parts[i] = r;
    });
    if (parts) out[a] = { ...m, parts };
  }
  // A prompt sent from here stays its own bubble, but it must become editable
  // once its stored copy exists: lend the replay's rewind handle to the
  // messages just above the Seam as long as the two keep pairing up.
  for (
    let a = k - 1, b = j - 1;
    a >= 0 && b >= 0 && samePrompt(painted[a], replay[b]);
    a--, b--
  ) {
    const mid = replay[b].mid;
    if (mid && out[a].mid !== mid) out[a] = { ...out[a], mid };
  }

  let cut = k;
  let r = j;
  for (; cut < painted.length && r < replay.length; cut++, r++) {
    const kept = keepPainted(painted[cut], replay[r]);
    if (!kept) break;
    out.push(kept);
  }
  out.push(...replay.slice(r));
  // Leading same-role pairs of the two remainders are the same rows in both
  // spellings (the prompt bubble and its stored copy) and are already in.
  // What the painted view still has past that is newer than the journal —
  // the step still streaming at the disconnect; the backend takes a step's
  // text only once it finishes. It survives only mid-turn: with no turn
  // running, the replay is the whole truth.
  const tail = painted.slice(cut);
  let p = 0;
  while (
    p < tail.length &&
    r + p < replay.length &&
    tail[p].role === replay[r + p].role
  )
    p++;
  if (carryInFlight && r + p >= replay.length)
    out.push(...tail.slice(p).filter((m) => counterpart(m) < 0));

  // assistant-ui keys messages by id and throws on a repeated tool call id:
  // an alignment that would repeat either is not an alignment.
  const seen = new Set<string>();
  for (const m of out) {
    if (seen.has(`id:${m.id}`)) return null;
    seen.add(`id:${m.id}`);
    for (const q of m.parts) {
      if (q.type !== "tool-call") continue;
      if (seen.has(`tool:${q.toolCallId}`)) return null;
      seen.add(`tool:${q.toolCallId}`);
    }
  }
  return out.length === painted.length && out.every((m, i) => m === painted[i])
    ? painted
    : out;
}
