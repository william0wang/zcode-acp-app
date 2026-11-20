// @vitest-environment node
// Shared terminal holder lifecycle (app ADR-0011 + mirror amendment): one
// socket outlives the screen — the pane mirror rides it always, ensure is
// idempotent per session, switching sessions attaches on the live socket,
// closeTerminal drops the lot, and live frames fold into the warm transcript
// and the pane list the views re-derive from.
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import {
  closeTerminal,
  ensureMirror,
  ensureTerminal,
  interruptPane,
  mirrorLive,
  reopenTerminal,
  shutdownTerminal,
  subscribeMirror,
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

test("ensure dials once, sends panes+open, and folds live frames into the warm log", () => {
  ensureTerminal("http://hub", "t", "s1", "My session");
  const ws = openUp();
  expect(ws.sent).toEqual([
    { v: 1, type: "panes" },
    { v: 1, type: "open", sessionId: "s1" },
  ]);

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
  expect(first.sent).toHaveLength(2); // panes + open, nothing more
  expect(terminalLive()?.title).toBe("renamed");
});

test("switching sessions attaches on the LIVE socket; closeTerminal drops it", () => {
  ensureTerminal("http://hub", "t", "s1");
  const ws = openUp();
  ws.server({
    v: 1,
    type: "state",
    terminal: { terminalId: "t_1", sessionId: "s1", state: "ready", turn: null, lastSeq: 2 },
  });
  ws.server({ v: 1, type: "events", events: [{ seq: 1, kind: "open" }, { seq: 2, kind: "input" }] });

  ensureTerminal("http://hub", "t", "s2");
  // Same socket (the mirror rides it), one more wire open, a fresh log —
  // the previous session's pane stays alive on the desktop, as v1's socket
  // close always left it.
  expect(FakeWebSocket.current).toBe(ws);
  expect(ws.closed).toBe(false);
  expect(ws.sent.map((f) => f.type)).toEqual(["panes", "open", "open"]);
  expect(ws.sent[2]).toEqual({ v: 1, type: "open", sessionId: "s2" });
  expect(terminalLive()?.sessionId).toBe("s2");
  expect(terminalLive()?.terminal).toBeNull();
  expect(terminalLog().size).toBe(0);

  closeTerminal();
  expect(ws.closed).toBe(true);
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

test("shutdown closes the pane on the wire and drops the holder", () => {
  ensureTerminal("http://hub", "t", "s1");
  const ws = openUp();
  shutdownTerminal();
  expect(ws.sent.map((f) => f.type)).toEqual(["panes", "open", "close"]);
  expect(terminalLive()).toBeNull();
  expect(terminalLog().size).toBe(0);
});

test("reopen asks the workbench for a fresh pane", () => {
  ensureTerminal("http://hub", "t", "s1");
  const ws = openUp();
  reopenTerminal();
  expect(ws.sent.filter((f) => f.type === "open")).toHaveLength(2);
});

test("no_carrier is fatal: the holder closes instead of laddering", () => {
  ensureTerminal("http://hub", "t", "s1");
  const ws = openUp();
  ws.server({ v: 1, type: "error", code: "no_carrier", message: "desktop workbench disconnected" });
  const live = terminalLive()!;
  expect(live.conn).toBe("closed");
  expect(live.error?.code).toBe("no_carrier");
  // The hub closes the socket after the error frame — no reconnect ladder.
  ws.onclose?.();
  expect(terminalLive()?.conn).toBe("closed");
});

test("after a fatal close, reopen redials and re-ensure does not no-op", () => {
  ensureTerminal("http://hub", "t", "s1");
  const first = openUp();
  first.server({ v: 1, type: "error", code: "no_carrier", message: "gone" });
  expect(terminalLive()?.conn).toBe("closed");

  // Reopen from the closed holder: a fresh socket, attach open on connect.
  reopenTerminal();
  expect(FakeWebSocket.current).not.toBe(first);
  expect(terminalLive()?.conn).toBe("connecting");
  expect(terminalLive()?.error).toBeNull();
  const second = openUp();
  expect(second.sent).toEqual([
    { v: 1, type: "panes" },
    { v: 1, type: "open", sessionId: "s1" },
  ]);

  // And the same-session ensure on a closed holder redials too (re-entering
  // the screen must never stare at a dead terminal).
  second.server({ v: 1, type: "error", code: "no_carrier", message: "gone" });
  ensureTerminal("http://hub", "t", "s1");
  expect(FakeWebSocket.current).not.toBe(second);
});

// ---- Terminal Mirror store (ADR-0011 amendment 2) ----

function pane(id: string, over: Record<string, unknown> = {}) {
  return {
    paneId: id,
    sessionId: "s1",
    cwd: "/w",
    state: "open",
    busy: "idle",
    command: null,
    sinceMs: null,
    lastExit: null,
    terminalId: null,
    ...over,
  };
}

test("ensureMirror dials once (panes, no open) and folds snapshot+deltas by paneId", () => {
  ensureMirror("http://hub", "t");
  const ws = openUp();
  expect(ws.sent).toEqual([{ v: 1, type: "panes" }]);
  expect(terminalLive()).toBeNull(); // mirror-only: no session attached

  // Re-ensure on the live socket is a no-op.
  ensureMirror("http://hub", "t");
  expect(FakeWebSocket.current).toBe(ws);
  expect(ws.sent).toHaveLength(1);

  ws.server({ v: 1, type: "panes", panes: [pane("p_2"), pane("p_1", { busy: "busy", command: "npm test" })] });
  expect(mirrorLive().panes.map((p) => p.paneId)).toEqual(["p_1", "p_2"]);
  ws.server({ v: 1, type: "pane", pane: pane("p_2", { busy: "busy", command: "cargo test" }) });
  expect(mirrorLive().panes.find((p) => p.paneId === "p_2")?.command).toBe("cargo test");
  ws.server({ v: 1, type: "pane", pane: pane("p_1", { state: "gone" }) });
  expect(mirrorLive().panes.map((p) => p.paneId)).toEqual(["p_2"]);

  // The snapshot supersedes prior deltas (reconnect resync).
  ws.server({ v: 1, type: "panes", panes: [pane("p_3")] });
  expect(mirrorLive().panes.map((p) => p.paneId)).toEqual(["p_3"]);
});

test("ensureTerminal reuses the mirror socket via attach — one connection, both surfaces", () => {
  ensureMirror("http://hub", "t");
  const ws = openUp();
  ws.server({ v: 1, type: "panes", panes: [pane("p_1")] });

  ensureTerminal("http://hub", "t", "s1", "My session");
  expect(FakeWebSocket.current).toBe(ws); // no second dial
  expect(ws.sent.map((f) => f.type)).toEqual(["panes", "open"]);
  expect(mirrorLive().panes).toHaveLength(1); // mirror kept its snapshot

  // Live pane deltas keep folding while the transcript is attached.
  ws.server({ v: 1, type: "pane", pane: pane("p_1", { busy: "busy" }) });
  expect(mirrorLive().panes[0].busy).toBe("busy");

  // A hub/token switch rebuilds the one socket from scratch.
  ensureMirror("http://other-hub", "t");
  expect(FakeWebSocket.current).not.toBe(ws);
  expect(ws.closed).toBe(true);
  expect(terminalLive()).toBeNull();
  expect(mirrorLive().panes).toHaveLength(0);
});

test("interruptPane rides the shared socket; a fatal close kills the mirror too, and ensureMirror redials", () => {
  ensureMirror("http://hub", "t");
  const ws = openUp();
  interruptPane("p_9");
  expect(ws.sent[1]).toEqual({ v: 1, type: "pane_interrupt", paneId: "p_9" });

  ws.server({ v: 1, type: "error", code: "no_carrier", message: "gone" });
  expect(mirrorLive().conn).toBe("closed");
  expect(mirrorLive().error?.code).toBe("no_carrier");

  // Re-entering the list redials the dead socket instead of no-op'ing.
  ensureMirror("http://hub", "t");
  expect(FakeWebSocket.current).not.toBe(ws);
  expect(mirrorLive().conn).toBe("connecting");
  expect(mirrorLive().error).toBeNull();
  const second = openUp();
  expect(second.sent).toEqual([{ v: 1, type: "panes" }]);

  // A transient error on a LIVE socket (unknown_pane is not fatal) is
  // cleared by the next pane frame — fresh data proves recovery.
  second.server({ v: 1, type: "error", code: "unknown_pane", message: "no such pane" });
  expect(mirrorLive().conn).toBe("open");
  expect(mirrorLive().error?.code).toBe("unknown_pane");
  second.server({ v: 1, type: "panes", panes: [pane("p_1")] });
  expect(mirrorLive().error).toBeNull();
  expect(mirrorLive().panes).toHaveLength(1);
});

test("subscribeMirror hears mirror changes and the unsubscribe silences it", () => {
  const seen: string[] = [];
  const unsub = subscribeMirror((m) => seen.push(`${m.conn}:${m.panes.length}`));
  ensureMirror("http://hub", "t");
  const ws = openUp();
  ws.server({ v: 1, type: "panes", panes: [pane("p_1")] });
  unsub();
  const count = seen.length;
  ws.server({ v: 1, type: "pane", pane: pane("p_2") });
  expect(seen.length).toBe(count);
  expect(seen[0]).toBe("closed:0");
  expect(seen.at(-1)).toBe("open:1");
});
