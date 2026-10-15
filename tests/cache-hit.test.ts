// @vitest-environment node
// Cache hit rate: the per-turn usage rides the session/prompt response
// (UNSTABLE ACP field). The store computes cachedRead / (input + cachedRead +
// cachedWrite) — inputTokens is cache-exclusive by bridge convention — shows
// it next to the context bar, and clears it when another session loads.
import { afterAll, beforeAll, expect, test, vi } from "vitest";

interface Frame {
  jsonrpc: "2.0";
  method?: string;
  id?: number;
  params?: Record<string, unknown>;
  result?: unknown;
}

// Prompt-response fixtures: full cache numbers, then a turn without any.
const USAGE_CACHED = {
  totalTokens: 10500,
  inputTokens: 1000,
  outputTokens: 500,
  cachedReadTokens: 8000,
  cachedWriteTokens: 1000,
};

class FakeWebSocket {
  static OPEN = 1;
  static current: FakeWebSocket | null = null;
  readyState = FakeWebSocket.OPEN;
  onopen: (() => void) | null = null;
  onmessage: ((ev: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;

  constructor(_url: string) {
    FakeWebSocket.current = this;
    queueMicrotask(() => this.onopen?.());
  }

  send(data: string): void {
    const frame = JSON.parse(data) as Frame;
    if (frame.method === "initialize") {
      this.reply(frame.id, { protocolVersion: 1, agentCapabilities: {} });
    } else if (frame.method === "session/load") {
      this.reply(frame.id, { sessionId: frame.params?.sessionId });
    } else if (frame.method === "session/prompt") {
      this.reply(frame.id, { stopReason: "end_turn", usage: USAGE_CACHED });
    }
  }

  server(frame: Frame): void {
    this.onmessage?.({ data: JSON.stringify(frame) });
  }

  die(): void {
    this.readyState = 3;
    this.onclose?.();
  }

  close(): void {
    this.readyState = 3;
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
    setItem: (k: string, v: string) => backing.set(k, String(v)),
    removeItem: (k: string) => backing.delete(k),
    clear: () => backing.clear(),
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
  void import("../src/store/appStore").then((m) =>
    m.useAppStore.getState().disconnectHub(),
  );
});

test("a prompt response with cache numbers sets cacheHit (8000/10000)", async () => {
  const { useAppStore } = await import("../src/store/appStore");
  const store = useAppStore;

  store.getState().connectToHub({ hubUrl: "http://hub", token: "t" });
  await vi.waitFor(() => expect(store.getState().instances.length).toBe(1));
  void store.getState().connectInstance("inst1");
  await vi.waitFor(() => expect(store.getState().connState).toBe("open"));
  await vi.waitFor(() =>
    expect(store.getState().loadingSession).toBe(false),
  );
  expect(store.getState().cacheHit).toBeNull();

  await store.getState().sendPrompt("hi");
  // 8000 cached read / (1000 input + 8000 read + 1000 write) = 80%.
  expect(store.getState().cacheHit).toBe(80);

  // Reloading the session (switch away and back) keeps the readout: the
  // turn-end snapshot carries cacheHit, so the title bar does not blank
  // until the next locally-run turn.
  await store.getState().loadSession("sess_1");
  expect(store.getState().cacheHit).toBe(80);
});

test("the same response feeds the turn speed readout (turnStats)", async () => {
  const { useAppStore } = await import("../src/store/appStore");
  const store = useAppStore;

  await store.getState().sendPrompt("again");
  const ts = store.getState().turnStats;
  expect(ts).not.toBeNull();
  expect(ts?.outputTokens).toBe(500);
  // The fake bridge settles within one tick, so the 0.1s floor applies —
  // tps is the floor-divided rate, seconds the floored wall time.
  expect(ts?.seconds).toBeGreaterThanOrEqual(0.1);
  expect(ts?.tps).toBeGreaterThan(0);

  // Switching sessions keeps the readout (snapshot restore, like cacheHit).
  await store.getState().loadSession("sess_1");
  expect(store.getState().turnStats?.outputTokens).toBe(500);
});

test("a foreign turn's end broadcast (usage + turnMs) feeds the readouts", async () => {
  const { useAppStore } = await import("../src/store/appStore");
  const store = useAppStore;
  const ws = FakeWebSocket.current!;

  // A turn driven from ANOTHER client: the app only sees the turn-state
  // broadcast, never a prompt response of its own.
  ws.server({
    jsonrpc: "2.0",
    method: "$/zcode/turnState",
    params: { sessionId: "sess_1", running: true },
  });
  expect(store.getState().isRunning).toBe(true);
  ws.server({
    jsonrpc: "2.0",
    method: "$/zcode/turnState",
    params: {
      sessionId: "sess_1",
      running: false,
      usage: {
        totalTokens: 10500,
        inputTokens: 1000,
        outputTokens: 200,
        cachedReadTokens: 8000,
        cachedWriteTokens: 1000,
      },
      turnMs: 4000,
    },
  });
  // 8000 / (1000 + 8000 + 1000) = 80%; 200 tokens over 4s = 50 tok/s.
  expect(store.getState().cacheHit).toBe(80);
  expect(store.getState().turnStats).toEqual({
    tps: 50,
    outputTokens: 200,
    seconds: 4,
  });
  expect(store.getState().isRunning).toBe(false);
});
