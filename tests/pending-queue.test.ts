// @vitest-environment node
// The pending queue's disconnected-mode contract: a prompt sent while the
// socket is down (phone slept mid-session) must park in the persisted queue
// instead of vanishing — the composer is cleared before sendPrompt runs, so
// the old early-return lost the draft for good. The post-reconnect attach
// (loadSession) flushes the queue, and force-send must not eat the queue
// head while disconnected (runPrompt would silently swallow it).
import { afterAll, beforeAll, beforeEach, expect, test, vi } from "vitest";

interface Frame {
  jsonrpc: "2.0";
  method?: string;
  id?: number;
  params?: Record<string, unknown>;
  result?: unknown;
}

// Minimal WS double: OPEN from birth, answers initialize/session/load,
// records session/prompt payloads for assertions, HOLDS prompt responses
// until settleAll() (the fake bridge replies only when told to).
class FakeWebSocket {
  static OPEN = 1;
  static current: FakeWebSocket | null = null;
  readyState = FakeWebSocket.OPEN;
  onopen: (() => void) | null = null;
  onmessage: ((ev: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
  prompts: unknown[][] = [];
  private promptIds: number[] = [];

  constructor(_url: string) {
    FakeWebSocket.current = this;
    queueMicrotask(() => this.onopen?.());
  }

  send(data: string): void {
    const frame = JSON.parse(data) as Frame;
    if (frame.method === "initialize") {
      this.reply(frame.id, { protocolVersion: 1, agentCapabilities: {} });
    } else if (frame.method === "session/load") {
      this.reply(frame.id, {});
    } else if (frame.method === "session/prompt") {
      this.promptIds.push(frame.id as number);
      this.prompts.push(frame.params?.prompt ?? []);
    }
  }

  settleAll(): void {
    for (const id of this.promptIds.splice(0)) this.reply(id, {});
  }

  private reply(id: number | undefined, result: unknown): void {
    this.onmessage?.({ data: JSON.stringify({ jsonrpc: "2.0", id, result }) });
  }

  close(): void {
    this.readyState = 3;
  }
}

async function connectedStore() {
  const { useAppStore } = await import("../src/store/appStore");
  useAppStore.getState().connectToHub({ hubUrl: "http://hub/", token: "t" });
  // Skip the discovery wait: a re-connect in the same module reuses the
  // running poll timer, so its next tick can be seconds out — seed the row
  // directly instead (the fs-revive suite does the same). pendingPrompts
  // resets too: a previous test's leftover queue would flush into this
  // connection's session and steal the first prompt slot.
  useAppStore.setState({
    instances: [
      {
        id: "inst1",
        workspace: "/w",
        sessions: [{ sessionId: "sess_1", updatedAt: 1 }],
      },
    ],
    pendingPrompts: {},
  });
  void useAppStore.getState().connectInstance("inst1");
  await vi.waitFor(() => expect(useAppStore.getState().connState).toBe("open"));
  await vi.waitFor(() =>
    expect(useAppStore.getState().activeSessionId).toBe("sess_1"),
  );
  return useAppStore;
}

beforeAll(() => {
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

beforeEach(() => {
  const backing = new Map<string, string>();
  (globalThis as Record<string, unknown>).localStorage = {
    getItem: (k: string) => backing.get(k) ?? null,
    setItem: (k: string, v: string) => void backing.set(k, String(v)),
    removeItem: (k: string) => void backing.delete(k),
    clear: () => void backing.clear(),
  };
});

afterAll(() => {
  // Stop the discovery poll so the worker can exit.
  void import("../src/store/appStore").then((m) =>
    m.useAppStore.getState().disconnectHub(),
  );
});

test("a prompt sent while disconnected parks in the queue and flushes after reconnect", async () => {
  const store = await connectedStore();

  // The phone-slept state: the socket is gone but the session selection
  // survives for the respawn.
  store.setState({ connState: "disconnected" });
  await store.getState().sendPrompt("typed while asleep");
  expect(store.getState().pendingPrompts["sess_1"]?.map((d) => d.text)).toEqual([
    "typed while asleep",
  ]);

  // Force-send while disconnected must NOT dequeue (runPrompt would swallow
  // the draft silently).
  store.getState().forceSendPending();
  expect(store.getState().pendingPrompts["sess_1"]).toHaveLength(1);

  // Reconnect path: connectInstance -> attach -> loadSession -> flush.
  void store.getState().connectInstance("inst1");
  await vi.waitFor(() => expect(store.getState().connState).toBe("open"));
  await vi.waitFor(() =>
    expect(FakeWebSocket.current?.prompts).toContainEqual([
      { type: "text", text: "typed while asleep" },
    ]),
  );
  expect(store.getState().pendingPrompts["sess_1"]).toBeUndefined();
});

test("editPending returns a queued draft to the composer and keeps queue order", async () => {
  const store = await connectedStore();

  store.setState({ isRunning: true });
  await store.getState().sendPrompt("first", [
    { mimeType: "image/png", data: "aGk=" },
  ]);
  await store.getState().sendPrompt("second");
  store.setState({ isRunning: false });

  store.getState().editPending(0);
  expect(store.getState().pendingPrompts["sess_1"]?.map((d) => d.text)).toEqual([
    "second",
  ]);
  const prefill = store.getState().composerPrefill;
  expect(prefill?.text).toBe("first");
  expect(prefill?.images).toEqual([{ mimeType: "image/png", data: "aGk=" }]);
});

test("forceSendPending(index) sends THAT bubble, not the queue head", async () => {
  const store = await connectedStore();

  // A runs (response held); B and C queue behind it.
  void store.getState().sendPrompt("A");
  await vi.waitFor(() => expect(store.getState().isRunning).toBe(true));
  void store.getState().sendPrompt("B");
  void store.getState().sendPrompt("C");

  // Target the THIRD bubble: C moves to the front and the running turn
  // cancels; A's settle then flushes C out, B stays queued.
  store.getState().forceSendPending(2);
  FakeWebSocket.current!.settleAll();
  await vi.waitFor(() =>
    expect(FakeWebSocket.current?.prompts).toHaveLength(2),
  );
  expect(JSON.stringify(FakeWebSocket.current!.prompts[1])).toContain("C");
  expect(
    store.getState().pendingPrompts["sess_1"]?.map((d) => d.text),
  ).toEqual(["B"]);
});

test("back-to-back prompts stay separate bubbles even with no assistant turn between", async () => {
  const store = await connectedStore();

  // A goes out; B queues while A's turn is held open by the fake bridge.
  void store.getState().sendPrompt("A");
  await vi.waitFor(() => expect(store.getState().isRunning).toBe(true));
  void store.getState().sendPrompt("B");
  expect(store.getState().pendingPrompts["sess_1"]).toHaveLength(1);

  // A settles with ZERO assistant output (response racing the trailing
  // chunks, or a failed turn): the flush must still open a NEW user bubble
  // for B — not append B onto A's bubble.
  FakeWebSocket.current!.settleAll();
  await vi.waitFor(() =>
    expect(FakeWebSocket.current?.prompts).toHaveLength(2),
  );
  // Let B's own settle land, then read the painted transcript.
  FakeWebSocket.current!.settleAll();
  await new Promise((r) => setTimeout(r, 50));

  const users = store.getState().messages.filter((m) => m.role === "user");
  expect(users).toHaveLength(2);
  expect(JSON.stringify(users[0].parts)).toContain("A");
  expect(JSON.stringify(users[1].parts)).toContain("B");
  expect(users[0].id).not.toBe(users[1].id);
});
