// @vitest-environment node
// The rewind actions' switch-away guard: the edit/retry request pends on the
// wire while the user can still back out and open another session. When the
// ack then lands in the background, the reload must NOT run — it would drag
// activeSessionId back to the edited session and poison the on-screen
// session's pagination state (loadSession's tail set wrote unguarded).
import { afterAll, beforeAll, beforeEach, expect, test, vi } from "vitest";

interface Frame {
  jsonrpc: "2.0";
  method?: string;
  id?: number;
  params?: Record<string, unknown>;
}

// Minimal WS double: OPEN from birth, answers initialize/session/load, and
// HOLDS session/edit_user_message + session/retry_turn responses until
// settleRewind() (the fake bridge replies only when told to).
class FakeWebSocket {
  static OPEN = 1;
  static current: FakeWebSocket | null = null;
  readyState = FakeWebSocket.OPEN;
  onopen: (() => void) | null = null;
  onmessage: ((ev: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
  loads: string[] = [];
  private rewindIds: number[] = [];

  constructor(_url: string) {
    FakeWebSocket.current = this;
    queueMicrotask(() => this.onopen?.());
  }

  send(data: string): void {
    const frame = JSON.parse(data) as Frame;
    if (frame.method === "initialize") {
      this.reply(frame.id, { protocolVersion: 1, agentCapabilities: {} });
    } else if (frame.method === "session/load") {
      this.loads.push(String(frame.params?.sessionId));
      this.reply(frame.id, {});
    } else if (
      frame.method === "session/edit_user_message" ||
      frame.method === "session/retry_turn"
    ) {
      this.rewindIds.push(frame.id as number);
    }
  }

  /** Release every held rewind ack. */
  settleRewind(): void {
    for (const id of this.rewindIds.splice(0)) this.reply(id, {});
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
  useAppStore.setState({
    instances: [
      {
        id: "inst1",
        workspace: "/w",
        sessions: [
          { sessionId: "sess_1", updatedAt: 2 },
          { sessionId: "sess_2", updatedAt: 1 },
        ],
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

beforeEach(() => {
  const backing = new Map<string, string>();
  (globalThis as Record<string, unknown>).localStorage = {
    getItem: (k: string) => backing.get(k) ?? null,
    setItem: (k: string, v: string) => backing.set(k, String(v)),
    removeItem: (k: string) => backing.delete(k),
    clear: () => backing.clear(),
  };
});

afterAll(() => {
  void import("../src/store/appStore").then((m) =>
    m.useAppStore.getState().disconnectHub(),
  );
});

test("an edit ack landing after a session switch does not hijack the view", async () => {
  const store = await connectedStore();
  const ws = FakeWebSocket.current!;
  const loadsBefore = ws.loads.length;

  const pending = store.getState().editUserMessage("msg_1", "edited");
  // The user backs out and opens the other session while the edit pends.
  await store.getState().openSession("inst1", "sess_2");
  expect(store.getState().activeSessionId).toBe("sess_2");

  ws.settleRewind();
  await pending;

  // Still on sess_2; no reload was started for sess_1; no boundary chip.
  expect(store.getState().activeSessionId).toBe("sess_2");
  expect(store.getState().rewindMark).toBeNull();
  // (The suite's cross-test reconnect may re-attach sess_2 on its own —
  // the regression signal is that sess_1 is never reloaded in the
  // background.)
  expect(ws.loads.slice(loadsBefore)).not.toContain("sess_1");
});

test("a retry ack landing after a session switch does not hijack the view", async () => {
  const store = await connectedStore();
  const ws = FakeWebSocket.current!;
  const loadsBefore = ws.loads.length;

  const pending = store.getState().retryTurn("msg_1");
  await store.getState().openSession("inst1", "sess_2");
  expect(store.getState().activeSessionId).toBe("sess_2");

  ws.settleRewind();
  await pending;

  expect(store.getState().activeSessionId).toBe("sess_2");
  expect(store.getState().rewindMark).toBeNull();
  expect(ws.loads.slice(loadsBefore)).not.toContain("sess_1");
});
