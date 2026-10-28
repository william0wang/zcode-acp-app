// @vitest-environment node
// Self-heal for a serve bridge that idle-exited under a still-open session:
// the hub answers 404 "unknown instance" (or 502 "bridge unreachable") on
// the session-bound fs routes, the client flags it instanceDead, and the
// store respawns the bridge through the same resume flow a manual re-open
// uses — the retried listing then lands on the NEW instance id. Regression:
// the file browser used to loop "加载失败/重试" forever because the retry
// reused the dead instance id while the cached root listing masked the death.
import { beforeEach, expect, test } from "vitest";

interface Frame {
  jsonrpc: "2.0";
  method?: string;
  id?: number;
  params?: Record<string, unknown>;
  result?: unknown;
}

// Answers initialize + session/load — everything the connect flow needs.
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
      this.reply(frame.id, { protocolVersion: 1, agentCapabilities: {} });
    } else if (frame.method === "session/load") {
      this.reply(frame.id, { sessionId: "sess1" });
    }
  }

  reply(id: number | undefined, result: unknown): void {
    this.onmessage?.({ data: JSON.stringify({ jsonrpc: "2.0", id, result }) });
  }

  close(): void {
    this.readyState = 3;
    this.onclose?.();
  }
}
(globalThis as Record<string, unknown>).WebSocket = FakeWebSocket;

let createPosts = 0;
let createBodies: Record<string, unknown>[] = [];
let fsCalls: string[] = [];
let createFails = false;
// The hub's close tombstone: an AUTO respawn of a remotely closed session is
// refused with 409 "session closed" (REMOTE-CLIENTS "Closing a session").
let createClosed = false;
// Discovery rows: connectToHub's background refresh OVERWRITES instances, so
// the route must carry the dead instance too — otherwise the 400ms fs-retry
// window races the refresh and erases the workspace the respawn needs.
let discoveryRows: unknown[] = [{ id: "i1", workspace: "/w" }];

beforeEach(() => {
  createPosts = 0;
  createBodies = [];
  fsCalls = [];
  createFails = false;
  createClosed = false;
  discoveryRows = [{ id: "i1", workspace: "/w" }];
  const backing = new Map<string, string>();
  (globalThis as Record<string, unknown>).localStorage = {
    getItem: (k: string) => backing.get(k) ?? null,
    setItem: (k: string, v: string) => backing.set(k, String(v)),
    removeItem: (k: string) => backing.delete(k),
    clear: () => backing.clear(),
  };
  (globalThis as Record<string, unknown>).fetch = async (
    input: unknown,
    init?: RequestInit,
  ): Promise<Response> => {
    const url = String(input);
    if (url.includes("/fs/list")) {
      fsCalls.push(url);
      if (url.includes("/i1/")) return new Response("unknown instance", { status: 404 });
      return Response.json({ root: "/w", entries: [], truncated: false });
    }
    if (url.includes("/fs/file")) {
      fsCalls.push(url);
      if (url.includes("/i1/")) return new Response("unknown instance", { status: 404 });
      return new Response("line1\nline2\n", {
        status: 200,
        headers: { "X-Zcode-First-Line": "1" },
      });
    }
    if (url.endsWith("/api/instances") && init?.method === "POST") {
      createPosts++;
      createBodies.push(JSON.parse(String(init.body)) as Record<string, unknown>);
      if (createFails) return new Response("boom", { status: 500 });
      if (createClosed) return new Response("session closed", { status: 409 });
      return Response.json({ id: "i2", reused: false });
    }
    if (url.includes("/api/instances")) return Response.json(discoveryRows);
    if (url.includes("/api/quota")) return Response.json([]);
    throw new Error(`test bug: no scripted response for ${init?.method} ${url}`);
  };
});

async function store() {
  const mod = await import("../src/store/appStore");
  return mod.useAppStore;
}

async function connectedStore() {
  const useAppStore = await store();
  useAppStore.getState().connectToHub({ hubUrl: "http://hub/", token: "t" });
  useAppStore.setState({
    instanceId: "i1",
    activeSessionId: "sess1",
    connState: "open",
    instances: [{ id: "i1", workspace: "/w" }],
  });
  return useAppStore;
}

test("fsList respawns a dead bridge and retries on the new instance", async () => {
  const useAppStore = await connectedStore();

  const listing = await useAppStore.getState().fsList("src");

  expect(listing).toEqual({ root: "/w", entries: [], truncated: false });
  expect(useAppStore.getState().instanceId).toBe("i2");
  // Exactly one respawn, and the retried call rode the new instance id.
  expect(createPosts).toBe(1);
  expect(fsCalls.some((u) => u.includes("/i1/"))).toBe(true);
  expect(fsCalls.some((u) => u.includes("/i2/"))).toBe(true);
});

test("fsFileText heals through the same respawn", async () => {
  const useAppStore = await connectedStore();

  await useAppStore.getState().fsFileText("src/a.ts", 1, 50);

  expect(createPosts).toBe(1);
  expect(useAppStore.getState().instanceId).toBe("i2");
});

test("a failed respawn surfaces failure instead of looping", async () => {
  createFails = true;
  const useAppStore = await connectedStore();

  await expect(useAppStore.getState().fsList("src")).resolves.toBeNull();
  expect(createPosts).toBe(1);
});

test("without the dead instance's workspace there is no respawn", async () => {
  discoveryRows = []; // discovery no longer lists the dead bridge either
  const useAppStore = await store();
  useAppStore.getState().connectToHub({ hubUrl: "http://hub/", token: "t" });
  useAppStore.setState({
    instanceId: "i1",
    activeSessionId: "sess1",
    connState: "open",
    instances: [],
  });

  await expect(useAppStore.getState().fsList("src")).resolves.toBeNull();
  expect(createPosts).toBe(0);
});

test("the respawn declares AUTO intent — the hub's tombstone may refuse it", async () => {
  const useAppStore = await connectedStore();

  const listing = await useAppStore.getState().fsList("src");

  expect(listing).toEqual({ root: "/w", entries: [], truncated: false });
  expect(createBodies[0]).toEqual({
    workspacePath: "/w",
    sessionId: "sess1",
    intent: "auto",
  });
});

test("a 409 \"session closed\" retires the conversation instead of respawning it", async () => {
  createClosed = true;
  const useAppStore = await connectedStore();

  const listing = await useAppStore.getState().fsList("src");

  // The refusal is the answer, not a failure: the conversation closes with
  // the close notice, no retry loop, no instance switch.
  expect(listing).toBeNull();
  expect(createPosts).toBe(1);
  expect(useAppStore.getState().instanceId).toBe("i1");
  expect(useAppStore.getState().activeSessionId).toBeNull();
  expect(useAppStore.getState().notice).toBe("notice.sessionClosed");
});

test("a manual resume keeps the default USER intent (no intent key on the wire)", async () => {
  const useAppStore = await store();
  useAppStore.getState().connectToHub({ hubUrl: "http://hub/", token: "t" });

  // POST + attach; the answer's i2 connection fails in this harness (no WS
  // for it) — the request shape is what this test pins.
  await useAppStore.getState().resumeProjectSession("/w", "sess1");

  expect(createBodies[0]).toEqual({ workspacePath: "/w", sessionId: "sess1" });
});
