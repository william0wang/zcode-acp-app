// @vitest-environment node
// Session replay cache: a wake-reconnect (or a switch back to a recently
// viewed session) must paint the cached snapshot immediately and reconcile
// with a metadata-only attach (session/load _meta.zcode.limit: 0). An equal
// totalMessages skips the tail replay entirely; a moved history falls back
// to the traditional full replay. The cache survives closeSession and is
// bounded (LRU).
import { afterAll, beforeAll, expect, test, vi } from "vitest";

interface Frame {
  jsonrpc: "2.0";
  method?: string;
  id?: number;
  params?: Record<string, unknown>;
  result?: unknown;
}

// Minimal WS double: OPEN from birth, answers initialize; session/load goes
// through a per-test router — a returned result replies at once, undefined
// HOLDS the request until settleLoad answers it (tests control timing).
class FakeWebSocket {
  static OPEN = 1;
  static current: FakeWebSocket | null = null;
  readyState = FakeWebSocket.OPEN;
  onopen: (() => void) | null = null;
  onmessage: ((ev: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
  sent: Frame[] = [];
  loadHandler: ((frame: Frame) => unknown) | null = null;
  private held: Array<number | undefined> = [];

  constructor(_url: string) {
    FakeWebSocket.current = this;
    queueMicrotask(() => this.onopen?.());
  }

  send(data: string): void {
    const frame = JSON.parse(data) as Frame;
    this.sent.push(frame);
    if (frame.method === "initialize") {
      this.reply(frame.id, {
        protocolVersion: 1,
        agentCapabilities: {},
        agentInfo: { name: "zcode-acp-server", version: "0.44.1" },
      });
    } else if (frame.method === "session/load") {
      const result = this.loadHandler?.(frame);
      if (result !== undefined) this.reply(frame.id, result);
      else this.held.push(frame.id);
    }
  }

  server(frame: Frame): void {
    this.onmessage?.({ data: JSON.stringify(frame) });
  }

  settleLoad(result: unknown): void {
    const id = this.held.shift();
    if (id !== undefined) this.reply(id, result);
  }

  loads(): Frame[] {
    return this.sent.filter((f) => f.method === "session/load");
  }

  loadLimit(frame: Frame): unknown {
    return (
      (frame.params?._meta as { zcode?: { limit?: unknown } } | undefined)
        ?.zcode?.limit ?? null
    );
  }

  close(): void {
    this.readyState = 3; // CLOSED
  }

  private reply(id: number | undefined, result: unknown): void {
    if (id === undefined) return;
    this.server({ jsonrpc: "2.0", id, result });
  }
}

beforeAll(() => {
  const backing = new Map<string, string>();
  (globalThis as Record<string, unknown>).localStorage = {
    getItem: (k: string) => backing.get(k) ?? null,
    setItem: (k: string, v: string) => void backing.set(k, String(v)),
    removeItem: (k: string) => void backing.delete(k),
    clear: () => void backing.clear(),
  };
  (globalThis as Record<string, unknown>).WebSocket = FakeWebSocket;
  (globalThis as Record<string, unknown>).fetch = async (input: unknown) => {
    const url = String(input);
    const body = url.includes("/api/instances")
      ? JSON.stringify([
          {
            id: "inst1",
            workspace: "/w",
            sessions: [{ sessionId: "sess_1", updatedAt: 1 }],
          },
        ])
      : url.includes("/api/quota")
        ? JSON.stringify({
            glm: { kind: "unavailable" },
            opencode: { kind: "not_configured" },
          })
        : "{}";
    return new Response(body, {
      status: 200,
      headers: { "Content-Type": "application/json" },
    });
  };
});

afterAll(() => {
  // Stop the 4s discovery poll so the worker can exit.
  void import("../src/store/appStore").then((m) =>
    m.useAppStore.getState().disconnectHub(),
  );
});

const flush = () => new Promise((r) => setTimeout(r, 250));

function chunk(ws: FakeWebSocket, sessId: string, text: string): void {
  ws.server({
    jsonrpc: "2.0",
    method: "session/update",
    params: {
      sessionId: sessId,
      update: {
        sessionUpdate: "agent_message_chunk",
        content: { type: "text", text },
        messageId: `m_${text}`,
      },
    },
  });
}

async function attach() {
  const { useAppStore } = await import("../src/store/appStore");
  const store = useAppStore;
  store.getState().connectToHub({ hubUrl: "http://hub", token: "t" });
  await vi.waitFor(() => expect(store.getState().instances.length).toBe(1));
  void store.getState().connectInstance("inst1");
  await vi.waitFor(() => expect(store.getState().connState).toBe("open"));
  await vi.waitFor(() => expect(store.getState().activeSessionId).toBe("sess_1"));
  return store;
}

test("cache hit paints instantly and reconciles metadata-only; moved history replays; the cache survives closeSession", async () => {
  const store = await attach();
  const ws = FakeWebSocket.current!;
  const has = (needle: string) =>
    store
      .getState()
      .messages.some((m) => JSON.stringify(m.parts).includes(needle));

  // First attach: a full tail replay (limit 30) with one replayed message.
  await vi.waitFor(() => expect(ws.loads().length).toBe(1));
  expect(ws.loadLimit(ws.loads()[0])).toBe(30);
  chunk(ws, "sess_1", "SEED");
  ws.settleLoad({ replayMeta: { cursor: "c0", hasMore: true, totalMessages: 99 } });
  await vi.waitFor(() => expect(store.getState().loadingSession).toBe(false));
  await flush();
  expect(has("SEED")).toBe(true);

  // Wake-reconnect re-attach: the cached snapshot paints synchronously...
  void store.getState().loadSession("sess_1");
  expect(store.getState().loadingSession).toBe(true);
  expect(has("SEED")).toBe(true);
  await vi.waitFor(() => expect(ws.loads().length).toBe(2));
  // ...then a metadata-only reconcile (limit 0) confirms nothing changed.
  expect(ws.loadLimit(ws.loads()[1])).toBe(0);
  ws.settleLoad({
    replayMeta: { cursor: "end", hasMore: true, totalMessages: 99, turnActive: false },
    modes: { currentModeId: "code" },
    configOptions: [],
  });
  await vi.waitFor(() => expect(store.getState().loadingSession).toBe(false));
  expect(has("SEED")).toBe(true); // zero replay: painted messages kept
  expect(store.getState().totalMessages).toBe(99);
  expect(ws.loads().length).toBe(2); // no tail replay was sent

  // History moved while away: the painted snapshot is dropped and the tail
  // replays the traditional way.
  void store.getState().loadSession("sess_1");
  await vi.waitFor(() => expect(ws.loads().length).toBe(3));
  ws.settleLoad({
    replayMeta: { cursor: "end", hasMore: true, totalMessages: 100, turnActive: false },
  });
  await vi.waitFor(() => expect(ws.loads().length).toBe(4));
  expect(ws.loadLimit(ws.loads()[3])).toBe(30);
  expect(store.getState().messages.length).toBe(0); // painted cache dropped
  chunk(ws, "sess_1", "NEW");
  ws.settleLoad({ replayMeta: { cursor: "c1", hasMore: false, totalMessages: 100 } });
  await vi.waitFor(() => expect(store.getState().loadingSession).toBe(false));
  await flush();
  expect(has("NEW")).toBe(true);
  expect(has("SEED")).toBe(false); // replaced by the fresh replay
  expect(store.getState().totalMessages).toBe(100);

  // closeSession leaves the session view but keeps the cache: re-opening
  // paints immediately and reconciles metadata-only again.
  store.getState().closeSession();
  expect(store.getState().messages.length).toBe(0);
  expect(store.getState().activeSessionId).toBeNull();
  void store.getState().loadSession("sess_1");
  expect(has("NEW")).toBe(true); // painted before any response
  await vi.waitFor(() => expect(ws.loads().length).toBe(5));
  expect(ws.loadLimit(ws.loads()[4])).toBe(0);
  ws.settleLoad({
    replayMeta: { cursor: "end", hasMore: false, totalMessages: 100, turnActive: false },
  });
  await vi.waitFor(() => expect(store.getState().loadingSession).toBe(false));
});

test("the cache is LRU-bounded: the oldest session falls back to a full replay", async () => {
  const { useAppStore } = await import("../src/store/appStore");
  const store = useAppStore;
  const ws = FakeWebSocket.current!;
  expect(store.getState().connState).toBe("open"); // connection from the first test

  // Answer every load synchronously; each attach caches its session.
  ws.loadHandler = () => ({
    replayMeta: { cursor: "c", hasMore: false, totalMessages: 1 },
  });
  for (const id of ["lr_1", "lr_2", "lr_3", "lr_4", "lr_5", "lr_6"]) {
    await store.getState().loadSession(id);
    await vi.waitFor(() => expect(store.getState().loadingSession).toBe(false));
  }
  // Cache now holds sess_1 (oldest) + lr_1..lr_6 = 7 > 6: sess_1 was evicted.
  const loadsBefore = ws.loads().length;
  ws.loadHandler = null; // hold the next response
  void store.getState().loadSession("sess_1");
  await vi.waitFor(() => expect(ws.loads().length).toBe(loadsBefore + 1));
  expect(ws.loadLimit(ws.loads()[ws.loads().length - 1])).toBe(30); // cache miss
  ws.settleLoad({
    replayMeta: { cursor: "c", hasMore: false, totalMessages: 1 },
  });
  await vi.waitFor(() => expect(store.getState().loadingSession).toBe(false));
});

test("a turn watched live re-enters without a full replay (tail-id reconcile)", async () => {
  const { useAppStore } = await import("../src/store/appStore");
  const store = useAppStore;
  const ws = FakeWebSocket.current!;
  expect(store.getState().connState).toBe("open"); // connection from the first test
  const has = (needle: string) =>
    store
      .getState()
      .messages.some((m) => JSON.stringify(m.parts).includes(needle));
  const turnState = (running: boolean) =>
    ws.server({
      jsonrpc: "2.0",
      method: "$/zcode/turnState",
      params: { sessionId: "sess_1", running },
    });
  const loads0 = ws.loads().length;

  // A foreign turn runs while we watch: the live chunk lands, the turn ends,
  // and the end-of-turn snapshot caches it — with a totalMessages count that
  // still predates the turn (counts only refresh on replay).
  turnState(true);
  chunk(ws, "sess_1", "LIVE1");
  turnState(false);
  await flush();
  expect(has("LIVE1")).toBe(true);

  // Re-entry: the cache paints instantly; the metadata-only reconcile
  // reports the turn's messages (count moved) but the SAME tail id — the id
  // carries the verdict and the unchanged tail must NOT replay from scratch.
  void store.getState().loadSession("sess_1");
  expect(has("LIVE1")).toBe(true); // painted before any response
  await vi.waitFor(() => expect(ws.loads().length).toBe(loads0 + 1));
  expect(ws.loadLimit(ws.loads()[loads0])).toBe(0);
  ws.settleLoad({
    replayMeta: {
      cursor: "c",
      hasMore: false,
      totalMessages: 2,
      lastMessageId: "m_LIVE1",
      turnActive: false,
    },
  });
  await vi.waitFor(() => expect(store.getState().loadingSession).toBe(false));
  await flush();
  expect(has("LIVE1")).toBe(true); // kept, not dropped for a replay
  expect(ws.loads().length).toBe(loads0 + 1); // and no full replay was sent

  // Old bridge (no lastMessageId in the meta): the fallback is the count
  // check, and a moved count still forces the traditional full replay.
  void store.getState().loadSession("sess_1");
  await vi.waitFor(() => expect(ws.loads().length).toBe(loads0 + 2));
  expect(ws.loadLimit(ws.loads()[loads0 + 1])).toBe(0);
  ws.settleLoad({
    replayMeta: { cursor: "c", hasMore: false, totalMessages: 3, turnActive: false },
  });
  await vi.waitFor(() => expect(ws.loads().length).toBe(loads0 + 3));
  expect(ws.loadLimit(ws.loads()[loads0 + 2])).toBe(30); // full replay
  ws.settleLoad({
    replayMeta: { cursor: "c3", hasMore: false, totalMessages: 3 },
  });
  await vi.waitFor(() => expect(store.getState().loadingSession).toBe(false));
});

test("a thought-tailed turn reconciles on the raw thought_ id; a local optimistic tail falls back to the count check", async () => {
  const { useAppStore } = await import("../src/store/appStore");
  const store = useAppStore;
  const ws = FakeWebSocket.current!;
  expect(store.getState().connState).toBe("open"); // connection from the first test
  const has = (needle: string) =>
    store
      .getState()
      .messages.some((m) => JSON.stringify(m.parts).includes(needle));
  const turnState = (running: boolean) =>
    ws.server({
      jsonrpc: "2.0",
      method: "$/zcode/turnState",
      params: { sessionId: "sess_1", running },
    });
  const chunkAt = (kind: string, mid: string | undefined, text: string) =>
    ws.server({
      jsonrpc: "2.0",
      method: "session/update",
      params: {
        sessionId: "sess_1",
        update: {
          sessionUpdate: kind,
          content: { type: "text", text },
          ...(mid ? { messageId: mid } : {}),
        },
      },
    });
  const loads0 = ws.loads().length;

  // Foreign turn ending on a thought: the journal's newest entry — and so the
  // bridge's lastMessageId — is the thought's own `thought_`-prefixed id.
  turnState(true);
  chunkAt("agent_message_chunk", "m_t1", "T1");
  chunkAt("agent_thought_chunk", "thought_m_t1", "TH1");
  turnState(false);
  await flush();
  expect(has("TH1")).toBe(true);

  // Re-entry: both tails are the RAW thought id — equal ids keep the painted
  // snapshot. (The old prefix-stripping compare mismatched this and forced a
  // full tail replay on every re-entry of a thought-tailed session.)
  void store.getState().loadSession("sess_1");
  await vi.waitFor(() => expect(ws.loads().length).toBe(loads0 + 1));
  expect(ws.loadLimit(ws.loads()[loads0])).toBe(0);
  ws.settleLoad({
    replayMeta: {
      cursor: "c",
      hasMore: false,
      totalMessages: 2,
      lastMessageId: "thought_m_t1",
      turnActive: false,
    },
  });
  await vi.waitFor(() => expect(store.getState().loadingSession).toBe(false));
  await flush();
  expect(has("TH1")).toBe(true); // kept, not dropped for a replay
  expect(ws.loads().length).toBe(loads0 + 1); // and no full replay was sent

  // A turn whose only trace is the optimistic prompt bubble (cancelled or
  // failed before any backend content) leaves a LOCAL m-counter tail id that
  // can never match a backend id: the count check decides — equal count
  // keeps the snapshot, still no replay.
  void store.getState().sendPrompt("PROMPT");
  await flush();
  expect(has("PROMPT")).toBe(true);
  const promptFrame = [...ws.sent].reverse().find((f) => f.method === "session/prompt");
  expect(promptFrame?.id).toBeDefined();
  ws.server({ jsonrpc: "2.0", id: promptFrame!.id, result: { stopReason: "end_turn" } });
  await flush();
  expect(store.getState().isRunning).toBe(false);

  void store.getState().loadSession("sess_1");
  await vi.waitFor(() => expect(ws.loads().length).toBe(loads0 + 2));
  expect(ws.loadLimit(ws.loads()[loads0 + 1])).toBe(0);
  ws.settleLoad({
    replayMeta: {
      cursor: "c",
      hasMore: false,
      // 2, the count the thought-turn reconcile above just refreshed: equal
      // counts with a local tail id keep the snapshot — no replay.
      totalMessages: 2,
      lastMessageId: "m_t1",
      turnActive: false,
    },
  });
  await vi.waitFor(() => expect(store.getState().loadingSession).toBe(false));
  await flush();
  expect(has("PROMPT")).toBe(true); // count check carried the verdict
  expect(ws.loads().length).toBe(loads0 + 2); // no full replay

  // ...but a moved count under a local tail still forces the full replay.
  void store.getState().loadSession("sess_1");
  await vi.waitFor(() => expect(ws.loads().length).toBe(loads0 + 3));
  ws.settleLoad({
    replayMeta: {
      cursor: "c",
      hasMore: false,
      totalMessages: 9,
      lastMessageId: "m_t1",
      turnActive: false,
    },
  });
  await vi.waitFor(() => expect(ws.loads().length).toBe(loads0 + 4));
  expect(ws.loadLimit(ws.loads()[loads0 + 3])).toBe(30);
  ws.settleLoad({
    replayMeta: { cursor: "c9", hasMore: false, totalMessages: 9 },
  });
  await vi.waitFor(() => expect(store.getState().loadingSession).toBe(false));
});
