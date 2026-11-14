// @vitest-environment node
// Session-held terminal lifecycle (app ADR-0011): the connection outlives
// the screen — ensure is idempotent per session, switching sessions replaces
// the terminal, closeTerminal drops it, and live frames fold into the warm
// transcript the next view re-derives from.
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import {
  closeTerminal,
  ensureTerminal,
  subscribeTerminal,
  terminalLive,
  terminalLog,
  type TerminalLiveState,
} from "../src/lib/terminalSession";

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

function openUp(): FakeWebSocket {
  const ws = FakeWebSocket.current!;
  ws.onopen?.();
  return ws;
}

beforeEach(() => {
  FakeWebSocket.current = null;
});

afterEach(() => {
  closeTerminal();
  vi.useRealTimers();
});

test("ensure dials once, sends open, and folds live frames into the warm log", () => {
  ensureTerminal("http://hub", "t", "s1", "My session");
  const ws = openUp();
  expect(ws.sent).toEqual([{ v: 1, type: "open", sessionId: "s1" }]);

  ws.server({
    v: 1,
    type: "state",
    terminal: { terminalId: "t_1", sessionId: "s1", state: "ready", turn: null, lastSeq: 1 },
  });
  ws.server({ v: 1, type: "events", events: [{ seq: 1, kind: "open", cwd: "/w" }] });

  const live = terminalLive()!;
  expect(live.sessionId).toBe("s1");
  expect(live.title).toBe("My session");
  expect(live.conn).toBe("open");
  expect(live.terminal?.state).toBe("ready");
  expect([...terminalLog().keys()]).toEqual([1]);
});

test("a running turn drives the live state the header badge reads", () => {
  ensureTerminal("http://hub", "t", "s1");
  const ws = openUp();
  ws.server({
    v: 1,
    type: "state",
    terminal: {
      terminalId: "t_1",
      sessionId: "s1",
      state: "ready",
      turn: { turnId: "x", command: "sleep 100", state: "running" },
      lastSeq: 2,
    },
  });
  expect(terminalLive()?.terminal?.turn?.state).toBe("running");
});

test("re-ensure the same session is a no-op — the screen reattaches, never re-opens", () => {
  ensureTerminal("http://hub", "t", "s1");
  const first = openUp();
  ensureTerminal("http://hub", "t", "s1", "renamed");
  expect(FakeWebSocket.current).toBe(first);
  expect(first.sent).toHaveLength(1);
  expect(terminalLive()?.title).toBe("renamed");
});

test("switching sessions replaces the terminal; closeTerminal drops it", () => {
  ensureTerminal("http://hub", "t", "s1");
  const first = openUp();
  ensureTerminal("http://hub", "t", "s2");
  expect(first.closed).toBe(true);
  const second = openUp();
  expect(second.url).toContain("token=t");
  expect(second.sent).toEqual([{ v: 1, type: "open", sessionId: "s2" }]);
  expect(terminalLive()?.sessionId).toBe("s2");

  closeTerminal();
  expect(second.closed).toBe(true);
  expect(terminalLive()).toBeNull();
  expect(terminalLog().size).toBe(0);
});

test("a registry restart clears the old epoch from the warm log", () => {
  ensureTerminal("http://hub", "t", "s1");
  const ws = openUp();
  ws.server({
    v: 1,
    type: "state",
    terminal: { terminalId: "t_1", sessionId: "s1", state: "ready", turn: null, lastSeq: 3 },
  });
  ws.server({ v: 1, type: "events", events: [{ seq: 1, kind: "open" }, { seq: 2, kind: "close" }] });
  expect(terminalLog().size).toBe(2);
  // Workbench relaunch: fresh registry, lastSeq below the held max.
  ws.server({
    v: 1,
    type: "state",
    terminal: { terminalId: "t_2", sessionId: "s1", state: "ready", turn: null, lastSeq: 1 },
  });
  ws.server({ v: 1, type: "events", events: [{ seq: 1, kind: "open", cwd: "/w" }] });
  expect([...terminalLog().keys()]).toEqual([1]);
});

test("subscribers hear state changes and the unsubscribe silences them", () => {
  const seen: Array<TerminalLiveState | null> = [];
  const unsub = subscribeTerminal((s) => seen.push(s));
  ensureTerminal("http://hub", "t", "s1");
  unsub();
  const count = seen.length;
  closeTerminal();
  expect(seen.length).toBe(count);
});
