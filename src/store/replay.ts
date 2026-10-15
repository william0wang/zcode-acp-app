// Pure replay helpers extracted from appStore: message projection
// (session/update -> chat model), replay pagination metadata, and the
// per-session replay cache. No store access lives here — everything operates
// on the arrays/values it is handed.
import {
  contentDiffBlocks,
  contentText,
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
    const ensured = ensureMessage(msgs, "assistant");
    return {
      messages: ensured.messages.map((m) =>
        m.id === ensured.message.id ? { ...m, parts: [...m.parts, part] } : m,
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
