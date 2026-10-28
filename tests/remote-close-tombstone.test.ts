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
// session/load at once with an empty history.
class FakeWebSocket {
  static OPEN = 1;
  static current: FakeWebSocket | null = null;
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
      this.reply(frame.id, {
        replayMeta: { cursor: "end", hasMore: false, totalMessages: 0, turnActive: false },
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
});
