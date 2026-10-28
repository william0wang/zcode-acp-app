// @vitest-environment node
// Transport-layer coverage for AcpConnection: the connect deadline (zombie
// sockets never fire WS events), pending-request rejection on close, and the
// JSON-RPC routing paths (server requests, cancel, update meta).
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { AcpConnection } from "../src/lib/acp";

interface Frame {
  jsonrpc: "2.0";
  method?: string;
  id?: number;
  params?: Record<string, unknown>;
  result?: unknown;
  error?: { message: string };
  // Notification-level replay flags ride on session/update frames.
  _meta?: unknown;
}

class FakeWebSocket {
  static OPEN = 1;
  static current: FakeWebSocket | null = null;
  readyState = FakeWebSocket.OPEN;
  onopen: (() => void) | null = null;
  onmessage: ((ev: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
  closed = false;
  sent: Frame[] = [];

  constructor(_url: string) {
    FakeWebSocket.current = this;
  }

  send(data: string): void {
    this.sent.push(JSON.parse(data) as Frame);
  }

  server(frame: Frame): void {
    this.onmessage?.({ data: JSON.stringify(frame) });
  }

  close(): void {
    this.closed = true;
    this.onclose?.();
  }
}

(globalThis as Record<string, unknown>).WebSocket = FakeWebSocket;

function makeHandlers() {
  return {
    onState: vi.fn(),
    onUpdate: vi.fn(),
    onServerRequest: vi.fn(),
    onCancelRequest: vi.fn(),
    onTurnState: vi.fn(),
  };
}

// Runs connect() and completes the initialize handshake.
async function opened() {
  const h = makeHandlers();
  const conn = new AcpConnection("http://hub", "tok", "inst", h);
  const promise = conn.connect();
  const ws = FakeWebSocket.current!;
  ws.onopen?.();
  const init = ws.sent.find((f) => f.method === "initialize")!;
  ws.server({ jsonrpc: "2.0", id: init.id!, result: { protocolVersion: 1 } });
  await promise;
  return { conn, h, ws };
}

beforeEach(() => {
  FakeWebSocket.current = null;
});

afterEach(() => {
  vi.useRealTimers();
});

test("connect resolves once initialize answers, and reports state open", async () => {
  const h = makeHandlers();
  const conn = new AcpConnection("http://hub", "tok", "inst", h);
  const promise = conn.connect();
  const ws = FakeWebSocket.current!;
  ws.onopen?.();
  expect(ws.sent).toHaveLength(1);
  expect(ws.sent[0]).toMatchObject({ method: "initialize" });
  // Plan approval / AskUserQuestion route through elicitation/create only
  // for clients that declare the form capability — assert we do.
  expect(
    (ws.sent[0].params as { clientCapabilities?: unknown }).clientCapabilities,
  ).toMatchObject({ elicitation: { form: true } });
  ws.server({
    jsonrpc: "2.0",
    id: ws.sent[0].id!,
    result: { protocolVersion: 1, agentCapabilities: {} },
  });
  await expect(promise).resolves.toMatchObject({ protocolVersion: 1 });
  expect(h.onState).toHaveBeenCalledWith("open");
});

test("connect times out when the socket never opens (zombie)", async () => {
  vi.useFakeTimers();
  const h = makeHandlers();
  const conn = new AcpConnection("http://hub", "tok", "inst", h);
  const promise = conn.connect();
  const assertion = expect(promise).rejects.toThrow("connect timed out");
  await vi.advanceTimersByTimeAsync(15_000);
  await assertion;
  expect(FakeWebSocket.current!.closed).toBe(true);
  expect(h.onState).not.toHaveBeenCalledWith("open");
});

test("requests reject with 'connection closed' when we close", async () => {
  const { conn } = await opened();
  const pending = conn.request("session/prompt", { sessionId: "s" });
  conn.close();
  await expect(pending).rejects.toThrow("connection closed");
});

test("peer-side close rejects pending requests and reports state closed", async () => {
  const { conn, h, ws } = await opened();
  const pending = conn.request("session/prompt", { sessionId: "s" });
  // Peer-side close: closedByUs is false, so state "closed" must surface.
  ws.onclose?.();
  await expect(pending).rejects.toThrow("connection closed");
  expect(h.onState).toHaveBeenCalledWith("closed");
});

test("server requests route to the handler and the answer carries the id", async () => {
  const { h, ws } = await opened();
  ws.server({
    jsonrpc: "2.0",
    id: 42,
    method: "session/request_permission",
    params: { sessionId: "s", options: [] },
  });
  expect(h.onServerRequest).toHaveBeenCalledTimes(1);
  const respond = h.onServerRequest.mock.calls[0][1] as (r: unknown) => void;
  respond({ outcome: { outcome: "selected", optionId: "once" } });
  const answer = ws.sent.find((f) => f.id === 42);
  expect(answer?.result).toEqual({
    outcome: { outcome: "selected", optionId: "once" },
  });
});

test("$/cancel_request routes to onCancelRequest", async () => {
  const { h, ws } = await opened();
  ws.server({ jsonrpc: "2.0", method: "$/cancel_request", params: { id: 9 } });
  expect(h.onCancelRequest).toHaveBeenCalledWith(9);
});

test("$/zcode/session_closed routes the sid to onSessionClosed; a malformed copy is dropped", async () => {
  // The bridge's cross-client retirement broadcast (ADR-0006): one copy per
  // session alias reaches every attached socket. The handler is optional —
  // a connection built without one must not throw on the frame.
  const { h, ws } = await opened();
  const onSessionClosed = vi.fn();
  (h as Record<string, unknown>).onSessionClosed = onSessionClosed;
  ws.server({
    jsonrpc: "2.0",
    method: "$/zcode/session_closed",
    params: { sessionId: "sess_gone" },
  });
  expect(onSessionClosed).toHaveBeenCalledWith("sess_gone");
  ws.server({ jsonrpc: "2.0", method: "$/zcode/session_closed", params: {} });
  ws.server({ jsonrpc: "2.0", method: "$/zcode/session_closed", params: { sessionId: "" } });
  expect(onSessionClosed).toHaveBeenCalledTimes(1);
});

test("session/update forwards the update and the notification-level _meta", async () => {
  const { h, ws } = await opened();
  ws.server({
    jsonrpc: "2.0",
    method: "session/update",
    params: {
      sessionId: "s",
      update: {
        sessionUpdate: "agent_message_chunk",
        content: { type: "text", text: "x" },
      },
    },
    _meta: { zcode: { collapsed: true } },
  });
  expect(h.onUpdate).toHaveBeenCalledWith(
    "s",
    expect.objectContaining({ sessionUpdate: "agent_message_chunk" }),
    { zcode: { collapsed: true } },
  );
});
