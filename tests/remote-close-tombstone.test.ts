// @vitest-environment node
// Remote close, phone side (ADR-0006 follow-up): the bridge's
// $/zcode/session_closed broadcast retires the conversation locally — the
// row leaves the list and the open conversation closes — and the local
// tombstone keeps the 4s discovery poll from painting a stale hub row back
// as a ghost until the hub catches up. A USER re-open clears the tombstone:
// close retires, it never deletes.
import { afterAll, beforeAll, expect, test, vi } from "vitest";

interface Frame {
  jsonrpc: "2.0";
  method?: string;
  id?: number;
  params?: Record<string, unknown>;
  result?: unknown;
}

// Minimal WS double: OPEN from birth, answers initialize and settles every
// session/load at once with an empty history. Loads carrying
// `_meta.zcode.intent: "auto"` for a sid in `retired` answer the bridge's
// close disclosure (`_meta.zcode.sessionClosed`) instead — the wire contract
// of REMOTE-CLIENTS "Closing a session"; user-intent loads re-arm normally.
class FakeWebSocket {
  static OPEN = 1;
  static current: FakeWebSocket | null = null;
  static retired = new Set<string>();
  readyState = FakeWebSocket.OPEN;
  onopen: (() => void) | null = null;
  onmessage: ((ev: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
  sent: Frame[] = [];

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
      const intent = (frame.params?._meta as { zcode?: { intent?: unknown } } | undefined)
        ?.zcode?.intent;
      const autoClosed = intent === "auto" && FakeWebSocket.retired.has(String(frame.params?.sessionId));
      this.reply(frame.id, {
        replayMeta: { cursor: "end", hasMore: false, totalMessages: 0, turnActive: false },
        ...(autoClosed ? { _meta: { zcode: { sessionClosed: true } } } : {}),
      });
    }
  }

  server(frame: Frame): void {
    this.onmessage?.({ data: JSON.stringify(frame) });
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
  // The hub's cached listing keeps carrying sess_1 — the stale window the
  // tombstone exists for (one heartbeat, or a poll answered pre-close).
  (globalThis as Record<string, unknown>).fetch = async (input: unknown) => {
    const url = String(input);
    const body = url.includes("/api/instances")
      ? JSON.stringify([
          {
            id: "inst1",
            workspace: "/w",
            sessions: [
              { sessionId: "sess_1", updatedAt: 2 },
              { sessionId: "sess_2", updatedAt: 1 },
            ],
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

test("the session_closed broadcast retires the row and leaves the conversation; the poll cannot paint it back until a user re-opens it", async () => {
  const { useAppStore } = await import("../src/store/appStore");
  const store = useAppStore;
  store.getState().connectToHub({ hubUrl: "http://hub", token: "t" });
  await vi.waitFor(() => expect(store.getState().instances.length).toBe(1));
  void store.getState().connectInstance("inst1", "sess_1");
  await vi.waitFor(() => expect(store.getState().connState).toBe("open"));
  await vi.waitFor(() => expect(store.getState().activeSessionId).toBe("sess_1"));
  await vi.waitFor(() => expect(store.getState().loadingSession).toBe(false));
  const ws = FakeWebSocket.current!;
  const sids = () =>
    store
      .getState()
      .instances.flatMap((i) => (i.sessions ?? []).map((s) => s.sessionId))
      .sort();
  expect(sids()).toEqual(["sess_1", "sess_2"]);

  // Another client closed sess_1 through the hub: the bridge's broadcast
  // reaches our socket. We leave the conversation and the row is gone.
  ws.server({
    jsonrpc: "2.0",
    method: "$/zcode/session_closed",
    params: { sessionId: "sess_1" },
  });
  expect(store.getState().activeSessionId).toBeNull();
  expect(store.getState().notice).toBe("notice.sessionClosed");
  expect(sids()).toEqual(["sess_2"]);

  // The hub's listing still carries sess_1 (stale cache): the wholesale
  // replace must not resurrect the row — the tombstone filters it.
  await store.getState().refreshInstances();
  expect(sids()).toEqual(["sess_2"]);

  // A copy for a session we do not hold is a no-op beyond the row drop.
  ws.server({
    jsonrpc: "2.0",
    method: "$/zcode/session_closed",
    params: { sessionId: "sess_other" },
  });
  expect(store.getState().activeSessionId).toBeNull();
  expect(sids()).toEqual(["sess_2"]);

  // A human re-opens sess_1 (user intent): the tombstone clears, the load
  // goes out, and the next poll shows the row again.
  void store.getState().openSession("inst1", "sess_1");
  await vi.waitFor(() => expect(store.getState().activeSessionId).toBe("sess_1"));
  await vi.waitFor(() => expect(store.getState().loadingSession).toBe(false));
  await store.getState().refreshInstances();
  expect(sids()).toEqual(["sess_1", "sess_2"]);

  // The user default stays implicit: a picked session's load carries no
  // intent key at all (the old wire shape an old bridge expects).
  const userLoad = FakeWebSocket.current!.sent.find(
    (f) => f.method === "session/load" && f.params?.sessionId === "sess_1",
  )!;
  expect(
    (userLoad.params?._meta as { zcode?: Record<string, unknown> })?.zcode?.intent,
  ).toBeUndefined();
});

test("a missed broadcast is caught by the AUTO reconnect replay: sessionClosed retires instead of resurrecting", async () => {
  const { useAppStore } = await import("../src/store/appStore");
  const store = useAppStore;
  // sess_1 is active on inst1 from the previous test. The conversation gets
  // closed from another surface while our socket is down (deep sleep): the
  // broadcast never reaches us — the bridge's retirement is the only truth.
  FakeWebSocket.retired.add("sess_1");
  const sids = () =>
    store
      .getState()
      .instances.flatMap((i) => (i.sessions ?? []).map((s) => s.sessionId))
      .sort();
  FakeWebSocket.current!.onclose?.(); // zombie socket drops — reconnect ladder

  // The reconnect replays the active session as an AUTO load...
  await vi.waitFor(
    () =>
      expect(
        FakeWebSocket.current!.sent.some(
          (f) =>
            f.method === "session/load" &&
            f.params?.sessionId === "sess_1" &&
            (f.params?._meta as { zcode?: { intent?: string } })?.zcode?.intent === "auto",
        ),
      ).toBe(true),
    { timeout: 5000 },
  );
  // ...and the bridge's sessionClosed disclosure retires it locally: the
  // conversation closes, the row drops, and the stale hub listing (still
  // carrying sess_1) cannot paint the ghost back.
  await vi.waitFor(() => expect(store.getState().activeSessionId).toBeNull());
  expect(store.getState().notice).toBe("notice.sessionClosed");
  await store.getState().refreshInstances();
  expect(sids()).toEqual(["sess_2"]);

  // Manual re-launch still works: a user open clears the tombstone and the
  // user-intent load re-arms (no disclosure on the answer).
  void store.getState().openSession("inst1", "sess_1");
  await vi.waitFor(() => expect(store.getState().activeSessionId).toBe("sess_1"));
  await vi.waitFor(() => expect(store.getState().loadingSession).toBe(false));
  await store.getState().refreshInstances();
  expect(sids()).toEqual(["sess_1", "sess_2"]);
});
