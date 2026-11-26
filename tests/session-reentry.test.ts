// @vitest-environment node
// Session re-entry (ADR-0015): a resident session keeps its painted messages
// on re-entry; a tail that changed while we were GONE (a turn missed offline)
// replays into a CAPTURED batch that merges at the Seam instead of clearing
// the view. In-flight output survives only while the turn still runs; an
// unalignable view falls back to the streamed replay; { reload: true } skips
// the merge. Live-watched content carries the same ids into replay, so a
// re-entry right after watching a turn is a metadata-only no-op.
import { afterAll, beforeAll, expect, test, vi } from "vitest";

interface Frame {
  jsonrpc: "2.0";
  method?: string;
  id?: number;
  params?: Record<string, unknown>;
  result?: unknown;
}

class FakeWebSocket {
  static OPEN = 1;
  static current: FakeWebSocket | null = null;
  readyState = FakeWebSocket.OPEN;
  onopen: (() => void) | null = null;
  onmessage: ((ev: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
  sent: Frame[] = [];
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
      this.held.push(frame.id);
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

// Per-test instance/session pairs — every test attaches its own instance, so
// load counts are absolute (no cross-test offsets).
const INSTANCES: Array<{ id: string; sessionId: string }> = [];

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
      ? JSON.stringify(
          INSTANCES.map(({ id, sessionId }) => ({
            id,
            workspace: "/w",
            sessions: [{ sessionId, updatedAt: 1 }],
          })),
        )
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
  void import("../src/store/appStore").then((m) =>
    m.useAppStore.getState().disconnectHub(),
  );
});

const flush = () => new Promise((r) => setTimeout(r, 250));

// A replayed chunk; mid marks the replay batch (the merge's split key).
function replayChunk(
  ws: FakeWebSocket,
  sessId: string,
  text: string,
  opts: { mid?: boolean; messageId?: string } = {},
): void {
  ws.server({
    jsonrpc: "2.0",
    method: "session/update",
    params: {
      sessionId: sessId,
      update: {
        sessionUpdate: "agent_message_chunk",
        content: { type: "text", text },
        ...(opts.messageId ? { messageId: opts.messageId } : {}),
      },
      ...(opts.mid === false
        ? {}
        : {
            _meta: { zcode: { mid: `stored_${text}` } },
          }),
    },
  });
}

// Attach to a FRESH instance (its own WebSocket) and wait for the session's
// first tail replay to be in flight.
async function attach(
  sessionId: string,
): Promise<{
  store: (typeof import("../src/store/appStore"))["useAppStore"];
  ws: FakeWebSocket;
}> {
  const { useAppStore } = await import("../src/store/appStore");
  const store = useAppStore;
  const instId = `inst_${sessionId}`;
  INSTANCES.push({ id: instId, sessionId });
  store.getState().connectToHub({ hubUrl: "http://hub", token: "t" });
  await vi.waitFor(() => expect(store.getState().instances.length).toBe(INSTANCES.length));
  // A previous test's instance stays connected: detach before connecting the
  // next one so each test owns exactly one socket and one session view.
  const prev = store.getState().instanceId;
  if (prev && prev !== instId) void store.getState().closeSession();
  void store.getState().connectInstance(instId);
  await vi.waitFor(() => expect(store.getState().connState).toBe("open"));
  await vi.waitFor(() => expect(store.getState().activeSessionId).toBe(sessionId));
  const ws = FakeWebSocket.current!;
  await vi.waitFor(() => expect(ws.loads().length).toBe(1));
  return { store, ws };
}

test("resident re-entry keeps the painted view; a missed turn merges at the Seam; a Far Behind view reloads", async () => {
  const { store, ws } = await attach("sess_a");
  const state = () => store.getState();
  const has = (needle: string) =>
    state().messages.some((m) => JSON.stringify(m.parts).includes(needle));
  const byId = (id: string) => state().messages.find((m) => m.id === id);

  // Initial tail replay: two stored messages.
  replayChunk(ws, "sess_a", "H1", { messageId: "mid_h1" });
  replayChunk(ws, "sess_a", "H2", { messageId: "mid_h2" });
  ws.settleLoad({
    replayMeta: { cursor: "c0", hasMore: true, totalMessages: 2, lastMessageId: "mid_h2" },
  });
  await vi.waitFor(() => expect(state().loadingSession).toBe(false));
  await flush();
  expect(has("H1")).toBe(true);
  expect(has("H2")).toBe(true);

  // Resident re-entry with nothing new: one metadata-only load, no replay,
  // the painted array keeps its identity.
  const painted1 = state().messages;
  void state().loadSession("sess_a");
  await vi.waitFor(() => expect(ws.loads().length).toBe(2));
  expect(ws.loadLimit(ws.loads()[1])).toBe(0);
  ws.settleLoad({
    replayMeta: { cursor: "c0", hasMore: true, totalMessages: 2, lastMessageId: "mid_h2", turnActive: false },
  });
  await vi.waitFor(() => expect(state().loadingSession).toBe(false));
  await flush();
  expect(state().messages).toBe(painted1); // untouched, not re-rendered

  // A turn ran while we were GONE (offline, another surface): the painted
  // view never saw H3. The reconcile reports the moved tail id; the captured
  // replay repeats H2 and adds H3 — the merge keeps H1/H2 painted
  // (identity-stable) and appends only what is new. No cleared view.
  const painted2 = state().messages;
  void state().loadSession("sess_a");
  await vi.waitFor(() => expect(ws.loads().length).toBe(3));
  expect(ws.loadLimit(ws.loads()[2])).toBe(0);
  ws.settleLoad({
    replayMeta: { cursor: "c0", hasMore: true, totalMessages: 3, lastMessageId: "mid_h3", turnActive: false },
  });
  await vi.waitFor(() => expect(ws.loads().length).toBe(4));
  expect(ws.loadLimit(ws.loads()[3])).toBe(30); // captured tail replay
  replayChunk(ws, "sess_a", "H2", { messageId: "mid_h2" });
  replayChunk(ws, "sess_a", "H3", { messageId: "mid_h3" });
  ws.settleLoad({
    replayMeta: { cursor: "c1", hasMore: true, totalMessages: 3, lastMessageId: "mid_h3" },
  });
  await vi.waitFor(() => expect(state().loadingSession).toBe(false));
  await flush();
  expect(has("H1")).toBe(true);
  expect(has("H2")).toBe(true);
  expect(has("H3")).toBe(true);
  // H1/H2 kept their object identity: no re-render of the settled view.
  expect(byId("mid_h1")).toBe(painted2[0]);
  expect(byId("mid_h2")).toBe(painted2[1]);
  // Exactly one metadata reconcile + one tail replay for the re-entry.
  expect(ws.loads().length).toBe(4);

  // Far Behind: the replay shares no anchor with the painted view — the
  // merge returns null, the view clears, and the tail streams the
  // traditional way (captured attempt first, streamed replay after).
  void state().loadSession("sess_a");
  await vi.waitFor(() => expect(ws.loads().length).toBe(5));
  ws.settleLoad({
    replayMeta: { cursor: "cx", hasMore: false, totalMessages: 50, lastMessageId: "mid_x50", turnActive: false },
  });
  await vi.waitFor(() => expect(ws.loads().length).toBe(6));
  expect(ws.loadLimit(ws.loads()[5])).toBe(30);
  replayChunk(ws, "sess_a", "FRESH", { messageId: "mid_fresh" });
  ws.settleLoad({
    replayMeta: { cursor: "cx", hasMore: false, totalMessages: 50, lastMessageId: "mid_fresh" },
  });
  // The merge failed (no anchor): the cleared view re-requests the tail.
  await vi.waitFor(() => expect(ws.loads().length).toBe(7));
  expect(ws.loadLimit(ws.loads()[6])).toBe(30);
  replayChunk(ws, "sess_a", "FRESH", { messageId: "mid_fresh" });
  ws.settleLoad({
    replayMeta: { cursor: "cx", hasMore: false, totalMessages: 50, lastMessageId: "mid_fresh" },
  });
  await vi.waitFor(() => expect(state().loadingSession).toBe(false));
  await flush();
  expect(has("FRESH")).toBe(true);
  expect(has("H1")).toBe(false); // the unalignable view was dropped
});

test("in-flight output carries only while the turn still runs; a restored turn re-enters running", async () => {
  const { store, ws } = await attach("sess_b");
  const state = () => store.getState();
  const has = (needle: string) =>
    state().messages.some((m) => JSON.stringify(m.parts).includes(needle));
  const turnState = (running: boolean) =>
    ws.server({
      jsonrpc: "2.0",
      method: "$/zcode/turnState",
      params: { sessionId: "sess_b", running },
    });

  // Seed one stored message.
  replayChunk(ws, "sess_b", "S1", { messageId: "mid_s1" });
  ws.settleLoad({
    replayMeta: { cursor: "c0", hasMore: false, totalMessages: 1, lastMessageId: "mid_s1" },
  });
  await vi.waitFor(() => expect(state().loadingSession).toBe(false));
  await flush();
  expect(has("S1")).toBe(true);

  // A foreign turn is streaming: its text is NOT in the journal yet.
  turnState(true);
  replayChunk(ws, "sess_b", "PART", { mid: false, messageId: "mid_part" });
  await flush();
  expect(has("PART")).toBe(true);
  expect(state().isRunning).toBe(true);

  // Re-entry with the turn still active: the captured replay (journal only,
  // no PART yet) must KEEP the in-flight text and restore the running state.
  void state().loadSession("sess_b");
  await vi.waitFor(() => expect(ws.loads().length).toBe(2));
  ws.settleLoad({
    replayMeta: { cursor: "c0", hasMore: false, totalMessages: 1, lastMessageId: "mid_s1", turnActive: true },
  });
  await vi.waitFor(() => expect(ws.loads().length).toBe(3));
  replayChunk(ws, "sess_b", "S1", { messageId: "mid_s1" });
  ws.settleLoad({
    replayMeta: { cursor: "c0", hasMore: false, totalMessages: 1, lastMessageId: "mid_s1", turnActive: true },
  });
  await vi.waitFor(() => expect(state().loadingSession).toBe(false));
  await flush();
  expect(has("PART")).toBe(true); // in-flight output survived the merge
  expect(state().isRunning).toBe(true); // running UI restored

  // The turn ends: the settle caches a settled snapshot and the running
  // state clears.
  turnState(false);
  await flush();
  expect(state().isRunning).toBe(false);
});

test("{ reload: true } skips the merge and streams the replay over a cleared view", async () => {
  const { store, ws } = await attach("sess_c");
  const state = () => store.getState();
  const has = (needle: string) =>
    state().messages.some((m) => JSON.stringify(m.parts).includes(needle));

  replayChunk(ws, "sess_c", "R1", { messageId: "mid_r1" });
  ws.settleLoad({
    replayMeta: { cursor: "c0", hasMore: false, totalMessages: 1, lastMessageId: "mid_r1" },
  });
  await vi.waitFor(() => expect(state().loadingSession).toBe(false));
  await flush();
  expect(has("R1")).toBe(true);

  // The caller knows the history was rewritten (edit/retry): no reconcile,
  // no capture, no merge — straight to the streamed replay.
  void state().loadSession("sess_c", { reload: true });
  await vi.waitFor(() => expect(ws.loads().length).toBe(2));
  expect(ws.loadLimit(ws.loads()[1])).toBe(30); // straight to the replay
  replayChunk(ws, "sess_c", "REWRITTEN", { messageId: "mid_rw" });
  ws.settleLoad({
    replayMeta: { cursor: "cx", hasMore: false, totalMessages: 1, lastMessageId: "mid_rw" },
  });
  await vi.waitFor(() => expect(state().loadingSession).toBe(false));
  await flush();
  expect(has("REWRITTEN")).toBe(true);
  expect(has("R1")).toBe(false);
  expect(ws.loads().length).toBe(2); // one load, no reconcile
});
