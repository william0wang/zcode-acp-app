// @vitest-environment node
// Transport-layer coverage for the Remote Terminal client (app ADR-0011):
// URL/token wiring, the open frame on connect, frame dispatch (state /
// chunked events / error), the reconnect ladder, fatal-error handling, the
// seq-keyed transcript fold, and the localStorage input history.
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import {
  TerminalConnection,
  mergeEvents,
  loadHistory,
  pushHistory,
  registryRestarted,
  type TerminalEvent,
  type TerminalHandlers,
} from "../src/lib/remoteTerminal";

class FakeWebSocket {
  static OPEN = 1;
  static current: FakeWebSocket | null = null;
  readyState = FakeWebSocket.OPEN;
  onopen: (() => void) | null = null;
  onmessage: ((ev: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
  closed = false;
  sent: Record<string, unknown>[] = [];
  url: string;

  constructor(url: string) {
    this.url = url;
    FakeWebSocket.current = this;
  }

  send(data: string): void {
    this.sent.push(JSON.parse(data) as Record<string, unknown>);
  }

  server(frame: Record<string, unknown>): void {
    this.onmessage?.({ data: JSON.stringify(frame) });
  }

  close(): void {
    this.closed = true;
    this.onclose?.();
  }
}

(globalThis as Record<string, unknown>).WebSocket = FakeWebSocket;

function makeHandlers(): TerminalHandlers {
  return {
    onState: vi.fn(),
    onTerminal: vi.fn(),
    onEvents: vi.fn(),
    onError: vi.fn(),
  };
}

// connect() and complete the socket open (the open frame is sent there).
function opened(sessionId = "s1") {
  const h = makeHandlers();
  const conn = new TerminalConnection("http://hub/", "tok", sessionId, h);
  conn.connect();
  const ws = FakeWebSocket.current!;
  ws.onopen?.();
  return { conn, h, ws };
}

beforeEach(() => {
  FakeWebSocket.current = null;
});

afterEach(() => {
  vi.useRealTimers();
  (globalThis as Record<string, unknown>).localStorage = undefined;
});

test("dials /api/terminals/ws with the token in the query and sends open on connect", () => {
  const { h, ws } = opened("sess_9");
  expect(ws.url).toBe("ws://hub/api/terminals/ws?token=tok");
  expect(ws.sent).toEqual([{ v: 1, type: "open", sessionId: "sess_9" }]);
  expect(h.onState).toHaveBeenCalledWith("connecting");
  expect(h.onState).toHaveBeenCalledWith("open");
});

test("https hub URLs upgrade to wss", () => {
  const conn = new TerminalConnection("https://hub.example/x/", "t", "s", makeHandlers());
  conn.connect();
  expect(FakeWebSocket.current!.url).toBe("wss://hub.example/x/api/terminals/ws?token=t");
});

test("dispatches state frames and chunked event batches separately", () => {
  const { h, ws } = opened();
  ws.server({
    v: 1,
    type: "state",
    terminal: { terminalId: "t_1", sessionId: "s1", cwd: "/w", state: "ready", turn: null, lastSeq: 3 },
  });
  expect(h.onTerminal).toHaveBeenCalledWith(
    expect.objectContaining({ terminalId: "t_1", state: "ready" }),
  );
  // A replay burst arrives as several budget-sized frames (workbench 96 KiB
  // chunking) — each is a plain append batch.
  ws.server({ v: 1, type: "events", events: [{ seq: 1, kind: "open", cwd: "/w" }] });
  ws.server({ v: 1, type: "events", events: [{ seq: 2, kind: "input", text: "ls" }] });
  expect(h.onEvents).toHaveBeenCalledTimes(2);
  expect(h.onEvents).toHaveBeenLastCalledWith([expect.objectContaining({ seq: 2 })]);
  // Malformed rows inside a batch are dropped, not fatal.
  ws.server({ v: 1, type: "events", events: [{ seq: 3 }, "junk", { kind: "exit" }] });
  expect(h.onEvents).toHaveBeenCalledTimes(2);
});

test("turn and interrupt frames carry the session id", () => {
  const { conn, ws } = opened();
  conn.sendCommand("npm test");
  conn.interrupt();
  expect(ws.sent[1]).toEqual({ v: 1, type: "turn", sessionId: "s1", command: "npm test" });
  expect(ws.sent[2]).toEqual({ v: 1, type: "interrupt", sessionId: "s1" });
});

test("a fatal error (disabled) surfaces the code and stops the ladder", () => {
  vi.useFakeTimers();
  const { h } = opened();
  // The hub's errorClose: one error frame, then the close.
  FakeWebSocket.current!.server({ v: 1, type: "error", code: "disabled", message: "off" });
  expect(h.onError).toHaveBeenCalledWith("disabled", "off");
  expect(h.onState).toHaveBeenLastCalledWith("closed");
  const dead = FakeWebSocket.current!;
  dead.close();
  expect(dead.closed).toBe(true);
  vi.advanceTimersByTime(60_000);
  // No reconnect: still the same (dead) socket, state stays closed.
  expect(FakeWebSocket.current).toBe(dead);
  expect(h.onState).toHaveBeenLastCalledWith("closed");
});

test("transport drops reconnect on the 1s-doubling ladder and open resets it", async () => {
  vi.useFakeTimers();
  const h = makeHandlers();
  const conn = new TerminalConnection("http://hub", "t", "s1", h);
  conn.connect();
  const a = FakeWebSocket.current!;
  a.onopen?.();
  a.close(); // peer-side drop; attempt 0 → 1s
  expect(h.onState).toHaveBeenLastCalledWith("reconnecting");
  await vi.advanceTimersByTimeAsync(1_000);
  const b = FakeWebSocket.current!;
  expect(b).not.toBe(a);
  b.onopen?.();
  expect(b.sent).toEqual([{ v: 1, type: "open", sessionId: "s1" }]);
  b.close(); // b opened, so the counter reset: 1s again
  await vi.advanceTimersByTimeAsync(1_000);
  const c = FakeWebSocket.current!;
  expect(c).not.toBe(b);
  c.close(); // c never opened — the counter climbs: 2s this time
  await vi.advanceTimersByTimeAsync(1_000);
  expect(FakeWebSocket.current).toBe(c);
  await vi.advanceTimersByTimeAsync(1_000);
  expect(FakeWebSocket.current).not.toBe(c);
  conn.close();
});

test("close() is final: no reconnect, state closed", async () => {
  vi.useFakeTimers();
  const { conn, h, ws } = opened();
  conn.close();
  expect(ws.closed).toBe(true);
  expect(h.onState).toHaveBeenLastCalledWith("closed");
  await vi.advanceTimersByTimeAsync(120_000);
  expect(FakeWebSocket.current).toBe(ws);
});

function ev(seq: number, kind: TerminalEvent["kind"], text?: string): TerminalEvent {
  return { seq, kind, text };
}

test("mergeEvents folds by seq: dedupes, keeps order, survives ring gaps", () => {
  const log = new Map<number, TerminalEvent>();
  mergeEvents(log, [ev(1, "open"), ev(2, "input", "ls"), ev(3, "exit")]);
  // Reconnect replay (from zero) duplicates nothing.
  mergeEvents(log, [ev(1, "open"), ev(2, "input", "ls"), ev(3, "exit"), ev(4, "input", "cd")]);
  expect([...log.keys()]).toEqual([1, 2, 3, 4]);
});

test("registryRestarted flags a workbench relaunch via the state frame's lastSeq", () => {
  const log = new Map<number, TerminalEvent>();
  mergeEvents(log, [ev(1, "open"), ev(2, "input", "ls"), ev(3, "exit")]);
  // Same epoch: the total-append counter never falls below the held max.
  expect(registryRestarted(log, 3)).toBe(false);
  expect(registryRestarted(log, 7)).toBe(false);
  // Workbench relaunch: fresh registry's lastSeq (2) < held max (3).
  expect(registryRestarted(log, 2)).toBe(true);
  // After the caller clears, the new epoch folds cleanly.
  log.clear();
  mergeEvents(log, [ev(1, "open"), ev(2, "input", "pwd")]);
  expect([...log.keys()]).toEqual([1, 2]);
  expect(registryRestarted(log, 2)).toBe(false);
});

test("history: push collapses consecutive duplicates, caps at 50, survives bad storage", () => {
  const store = new Map<string, string>();
  (globalThis as Record<string, unknown>).localStorage = {
    getItem: (k: string) => store.get(k),
    setItem: (k: string, v: string) => void store.set(k, v),
  };
  pushHistory("ls");
  pushHistory("ls"); // consecutive duplicate collapses
  pushHistory("npm test");
  expect(loadHistory()).toEqual(["ls", "npm test"]);
  for (let i = 0; i < 60; i++) pushHistory(`cmd${i}`);
  expect(loadHistory()).toHaveLength(50);
  expect(loadHistory()[0]).toBe("cmd10");
  // A throwing localStorage (private mode) never breaks a submit.
  (globalThis as Record<string, unknown>).localStorage = {
    getItem: () => null,
    setItem: () => {
      throw new Error("quota");
    },
  };
  pushHistory("still fine");
  expect(loadHistory()).toEqual([]);
  // Absent storage reads as empty.
  (globalThis as Record<string, unknown>).localStorage = undefined;
  expect(loadHistory()).toEqual([]);
});
