import type { SessionUpdate } from "./types";

export type AcpConnectionState = "connecting" | "open" | "closed";

// Deadline for open + initialize; WS events alone can't be trusted to fire
// (zombie sockets after deep sleep / network switch).
const CONNECT_TIMEOUT_MS = 15_000;

export interface ServerRequest {
  id: number;
  method: string;
  params: Record<string, unknown>;
}

export interface AcpHandlers {
  onState: (state: AcpConnectionState) => void;
  // `meta` is the notification-level `_meta` (replay flags like
  // zcode.collapsed ride there, not inside the update).
  onUpdate: (sessionId: string, update: SessionUpdate, meta?: unknown) => void;
  onServerRequest: (
    req: ServerRequest,
    respond: (result: unknown) => void,
  ) => void;
  onCancelRequest: (id: number) => void;
  // Bridge extension `$/zcode/turnState`: a turn started/ended somewhere the
  // client can't infer (another client's prompt, restored after re-attach).
  // The END transition carries the turn's usage block + wall time (bridge
  // emits them so attach-only clients can paint cache/speed readouts for
  // turns driven from elsewhere); both are optional.
  onTurnState?: (
    sessionId: string,
    running: boolean,
    turn?: { usage?: unknown; turnMs?: number },
  ) => void;
  // Bridge extension `$/zcode/workflowListChanged` (bridge 0.57.0): the
  // directory watch fired — a workflow was saved/deleted/moved by any means
  // on the connected instance.
  onWorkflowListChanged?: () => void;
  // Bridge extension `$/zcode/session_closed` (ADR-0006): another client
  // retired this conversation through the hub (one copy per session alias
  // reaches every attached socket). The receiver retires it locally the
  // same way its own close would — never a resume.
  onSessionClosed?: (sessionId: string) => void;
}

interface PendingEntry {
  resolve: (value: unknown) => void;
  reject: (err: Error) => void;
}

// initialize result — only the capability probe we consume is typed; the
// zcode extension rides in agentCapabilities._meta (REMOTE-CLIENTS.md).
export interface AcpInitializeResult {
  agentCapabilities?: {
    _meta?: { zcode?: { fs?: boolean } };
    [key: string]: unknown;
  };
  [key: string]: unknown;
}

// One AcpConnection owns one WebSocket, bound to one bridge instance for its
// whole lifetime (REMOTE-CLIENTS.md). Reconnection means building a new one.
export class AcpConnection {
  private ws: WebSocket | null = null;
  private nextId = 1;
  private pending = new Map<number, PendingEntry>();
  private closedByUs = false;

  constructor(
    private readonly hubUrl: string,
    private readonly token: string,
    private readonly instanceId: string,
    private readonly handlers: AcpHandlers,
  ) {}

  // Resolves once the socket is open and `initialize` (protocolVersion MUST be
  // the number 1) has completed; the result carries agentCapabilities for
  // feature gating (e.g. zcode fs). Any non-open outcome rejects.
  connect(): Promise<AcpInitializeResult> {
    return new Promise((resolve, reject) => {
      let settled = false;
      // A zombie socket (deep sleep, network switch) can sit open forever
      // without ever answering `initialize` — no WS event fires, so without
      // this deadline the connect() promise hangs and the reconnect loop
      // stalls with it.
      const timeout = setTimeout(() => {
        if (settled) return;
        settled = true;
        this.close();
        reject(new Error("connect timed out"));
      }, CONNECT_TIMEOUT_MS);
      this.handlers.onState("connecting");
      const ws = new WebSocket(this.wsUrl());
      this.ws = ws;
      const settle = <T>(fn: (v: T) => void, arg: T) => {
        if (settled) return;
        settled = true;
        clearTimeout(timeout);
        fn(arg);
      };

      ws.onopen = () => {
        this.request("initialize", {
          protocolVersion: 1,
          // Declared so the bridge routes ExitPlanMode / AskUserQuestion
          // through the richer elicitation/create form deterministically —
          // capabilities OR-merge across attached clients, so without this
          // the dialog style depends on whether another client (e.g. Zed)
          // happened to declare it first.
          clientCapabilities: { elicitation: { form: true } },
        }).then(
          (result) => {
            settle(resolve, result as AcpInitializeResult);
            this.handlers.onState("open");
          },
          (err: Error) => {
            settle(reject, err);
            this.close();
          },
        );
      };
      ws.onerror = () => {
        settle(
          reject,
          new Error("WebSocket error (bad URL, token, or instance id)"),
        );
      };
      ws.onclose = () => {
        this.failAllPending(new Error("connection closed"));
        settle(reject, new Error("connection closed before open"));
        if (!this.closedByUs) this.handlers.onState("closed");
      };
      ws.onmessage = (ev) => {
        if (typeof ev.data === "string") this.handleMessage(ev.data);
        // Binary frames are ignored per contract.
      };
    });
  }

  request(
    method: string,
    params: Record<string, unknown> = {},
  ): Promise<unknown> {
    const ws = this.ws;
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      return Promise.reject(new Error("connection not open"));
    }
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      // No timeout: session/prompt legitimately runs for minutes.
      ws.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
    });
  }

  notify(method: string, params: Record<string, unknown> = {}): void {
    this.send({ jsonrpc: "2.0", method, params });
  }

  respondServerRequest(id: number, result: unknown): void {
    this.send({ jsonrpc: "2.0", id, result });
  }

  close(): void {
    this.closedByUs = true;
    this.failAllPending(new Error("connection closed"));
    this.ws?.close();
    this.ws = null;
  }

  private wsUrl(): string {
    const url = new URL(this.hubUrl);
    if (url.protocol === "https:") url.protocol = "wss:";
    else if (url.protocol === "http:") url.protocol = "ws:";
    // ws:/wss: pass through unchanged
    url.pathname = url.pathname.replace(/\/+$/, "") + "/acp";
    // Browsers cannot set WS headers, hence ?token= (per contract).
    url.searchParams.set("instance", this.instanceId);
    url.searchParams.set("token", this.token);
    return url.toString();
  }

  private send(msg: unknown): void {
    const ws = this.ws;
    if (ws && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(msg));
  }

  private handleMessage(data: string): void {
    let msg: {
      id?: number;
      method?: string;
      params?: Record<string, unknown>;
      result?: unknown;
      error?: { message: string };
      _meta?: unknown;
    };
    try {
      msg = JSON.parse(data);
    } catch {
      return;
    }

    if (typeof msg.method === "string") {
      if (msg.id !== undefined) {
        // Server -> client request (permission / elicitation).
        this.handlers.onServerRequest(
          { id: msg.id, method: msg.method, params: msg.params ?? {} },
          (result) => this.respondServerRequest(msg.id as number, result),
        );
      } else if (msg.method === "session/update") {
        // The replay `_meta` (zcode.collapsed etc.) is TOP-LEVEL on the
        // notification per the wire contract; params-level is a fallback.
        const p = msg.params as
          | { sessionId?: string; update?: SessionUpdate; _meta?: unknown }
          | undefined;
        if (p && typeof p.sessionId === "string" && p.update) {
          this.handlers.onUpdate(p.sessionId, p.update, msg._meta ?? p._meta);
        }
      } else if (msg.method === "$/zcode/turnState") {
        const p = msg.params as
          | {
              sessionId?: string;
              running?: boolean;
              usage?: unknown;
              turnMs?: unknown;
            }
          | undefined;
        if (
          p &&
          typeof p.sessionId === "string" &&
          typeof p.running === "boolean"
        ) {
          const turn =
            p.usage != null || typeof p.turnMs === "number"
              ? {
                  usage: p.usage,
                  turnMs: typeof p.turnMs === "number" ? p.turnMs : undefined,
                }
              : undefined;
          this.handlers.onTurnState?.(p.sessionId, p.running, turn);
        }
      } else if (msg.method === "$/zcode/workflowListChanged") {
        this.handlers.onWorkflowListChanged?.();
      } else if (msg.method === "$/zcode/session_closed") {
        const sid = (msg.params as { sessionId?: unknown } | undefined)?.sessionId;
        if (typeof sid === "string" && sid) this.handlers.onSessionClosed?.(sid);
      } else if (msg.method === "$/cancel_request") {
        const id = (msg.params as { id?: number } | undefined)?.id;
        if (typeof id === "number") this.handlers.onCancelRequest(id);
      }
      return;
    }

    if (typeof msg.id === "number") {
      const entry = this.pending.get(msg.id);
      if (!entry) return;
      this.pending.delete(msg.id);
      if (msg.error)
        entry.reject(new Error(msg.error.message ?? "JSON-RPC error"));
      else entry.resolve(msg.result);
    }
  }

  private failAllPending(err: Error): void {
    for (const entry of this.pending.values()) entry.reject(err);
    this.pending.clear();
  }
}
