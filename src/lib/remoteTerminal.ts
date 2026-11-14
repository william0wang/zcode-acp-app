// Remote Terminal wire client (app ADR-0011): one WebSocket to the hub's
// /api/terminals/ws, relayed verbatim onto the desktop workbench's control
// connection. The hub never interprets frames; the workbench owns the
// terminal state and the event log. This side just opens the session's
// terminal, folds the seq-numbered event log into a transcript, and sends
// one-line turns. Reconnect re-`open`s (idempotent — the workbench replays
// its whole ring) and the seq-keyed fold deduplicates; a turn running
// through a drop settles into the log and replays with everything else.

export type TerminalConnState = "connecting" | "open" | "reconnecting" | "closed";

export interface TerminalTurnInfo {
  turnId: string;
  command: string;
  state: "running";
}

/** The workbench's Remote Terminal object (spec §"The Remote Terminal object"). */
export interface TerminalInfo {
  terminalId: string;
  sessionId: string;
  cwd: string;
  state: "opening" | "ready" | "gone";
  turn: TerminalTurnInfo | null;
  lastSeq: number;
}

export interface TerminalEvent {
  seq: number;
  kind: "open" | "input" | "output" | "exit" | "note" | "close";
  turnId?: string;
  text?: string;
  cwd?: string;
  exitCode?: number;
  durationMs?: number;
}

export interface TerminalHandlers {
  onState: (state: TerminalConnState) => void;
  onTerminal: (terminal: TerminalInfo) => void;
  onEvents: (events: TerminalEvent[]) => void;
  onError: (code: string, message: string) => void;
}

// Codes that make retrying pointless until the user re-enters: the exec
// switch is off, or the workbench doesn't know the session. no_carrier and
// transport drops DO retry — the workbench may come back any moment.
const FATAL_CODES = new Set(["disabled", "unknown_session"]);

// Same ladder as appStore.scheduleReconnect: 1s doubling, 30s cap.
const RECONNECT_MAX_MS = 30_000;

export class TerminalConnection {
  private ws: WebSocket | null = null;
  private retryTimer: ReturnType<typeof setTimeout> | null = null;
  private attempt = 0;
  private closedByUs = false;

  constructor(
    private readonly hubUrl: string,
    private readonly token: string,
    private readonly sessionId: string,
    private readonly handlers: TerminalHandlers,
  ) {}

  connect(): void {
    this.closedByUs = false;
    this.handlers.onState("connecting");
    const ws = new WebSocket(this.wsUrl());
    this.ws = ws;
    ws.onopen = () => {
      if (this.ws !== ws) return;
      this.attempt = 0;
      this.handlers.onState("open");
      // Open (re)attaches and replays the whole ring — idempotent, and the
      // seq-keyed fold upstream deduplicates against what we already hold.
      ws.send(JSON.stringify({ v: 1, type: "open", sessionId: this.sessionId }));
    };
    ws.onmessage = (ev) => {
      if (typeof ev.data === "string") this.handleMessage(ev.data);
    };
    ws.onclose = () => {
      if (this.ws !== ws) return;
      this.ws = null;
      if (this.closedByUs) {
        this.handlers.onState("closed");
        return;
      }
      this.scheduleReconnect();
    };
    // onerror is always followed by onclose (browser contract) — the
    // reconnect ladder lives there.
    ws.onerror = () => {};
  }

  /** Submit one command turn. The workbench answers `error busy` while one runs. */
  sendCommand(command: string): void {
    this.send({ v: 1, type: "turn", sessionId: this.sessionId, command });
  }

  /** Ctrl-C for the running turn (same byte a local Ctrl-C writes). */
  interrupt(): void {
    this.send({ v: 1, type: "interrupt", sessionId: this.sessionId });
  }

  close(): void {
    // Leaving the screen is NOT a terminal close — the pane stays alive on
    // the desktop for walk-back continuity (spec §Motivation).
    this.closedByUs = true;
    if (this.retryTimer) clearTimeout(this.retryTimer);
    this.retryTimer = null;
    this.ws?.close();
    this.ws = null;
    this.handlers.onState("closed");
  }

  private scheduleReconnect(): void {
    this.handlers.onState("reconnecting");
    const delay = Math.min(RECONNECT_MAX_MS, 1000 * 2 ** this.attempt++);
    this.retryTimer = setTimeout(() => {
      this.retryTimer = null;
      this.connect();
    }, delay);
  }

  private send(frame: Record<string, unknown>): void {
    if (this.ws && this.ws.readyState === WebSocket.OPEN)
      this.ws.send(JSON.stringify(frame));
  }

  private handleMessage(data: string): void {
    let frame: {
      type?: unknown;
      terminal?: unknown;
      events?: unknown;
      code?: unknown;
      message?: unknown;
    };
    try {
      frame = JSON.parse(data);
    } catch {
      return; // forward-compat: garbage frames are dropped, not fatal
    }
    switch (frame.type) {
      case "state":
        if (isTerminalInfo(frame.terminal)) this.handlers.onTerminal(frame.terminal);
        break;
      case "events": {
        // Replay bursts arrive chunked (workbench 96 KiB budget) — each
        // frame is a plain append batch.
        if (Array.isArray(frame.events)) {
          const events = frame.events.filter(isTerminalEvent);
          if (events.length > 0) this.handlers.onEvents(events);
        }
        break;
      }
      case "error": {
        const code = typeof frame.code === "string" ? frame.code : "error";
        const message = typeof frame.message === "string" ? frame.message : code;
        this.handlers.onError(code, message);
        // A fatal verdict (exec switch off, unknown session) will not heal
        // on retry — stop the ladder and let the screen show the reason.
        if (FATAL_CODES.has(code)) this.close();
        break;
      }
      default:
        break; // forward-compat: unknown kinds are ignored
    }
  }

  private wsUrl(): string {
    const url = new URL(this.hubUrl);
    if (url.protocol === "https:") url.protocol = "wss:";
    else if (url.protocol === "http:") url.protocol = "ws:";
    url.pathname = url.pathname.replace(/\/+$/, "") + "/api/terminals/ws";
    // Browsers cannot set WS headers, hence ?token= (per contract).
    url.searchParams.set("token", this.token);
    return url.toString();
  }
}

function isTerminalInfo(v: unknown): v is TerminalInfo {
  if (typeof v !== "object" || v === null) return false;
  const t = v as Record<string, unknown>;
  return (
    typeof t.terminalId === "string" &&
    typeof t.sessionId === "string" &&
    (t.state === "opening" || t.state === "ready" || t.state === "gone") &&
    typeof t.lastSeq === "number" &&
    (t.turn === null ||
      (typeof t.turn === "object" &&
        typeof (t.turn as Record<string, unknown>).turnId === "string"))
  );
}

function isTerminalEvent(v: unknown): v is TerminalEvent {
  if (typeof v !== "object" || v === null) return false;
  const e = v as Record<string, unknown>;
  return (
    typeof e.seq === "number" &&
    (e.kind === "open" ||
      e.kind === "input" ||
      e.kind === "output" ||
      e.kind === "exit" ||
      e.kind === "note" ||
      e.kind === "close")
  );
}

/**
 * Fold a batch into the seq-keyed transcript map. Idempotent by seq, so the
 * open-replay (from zero) and live appends land the same way and reconnects
 * never duplicate.
 */
export function mergeEvents(
  log: Map<number, TerminalEvent>,
  batch: TerminalEvent[],
): void {
  for (const e of batch) if (!log.has(e.seq)) log.set(e.seq, e);
}

/**
 * True when a `state` frame proves the workbench's registry restarted: the
 * `lastSeq` counter is TOTAL appends (not the ring max), so within one epoch
 * it can never fall below the highest seq we already hold. A workbench
 * relaunch resets it — the caller drops the old epoch's rows instead of
 * interleaving two seq spaces.
 */
export function registryRestarted(
  log: Map<number, TerminalEvent>,
  lastSeq: number,
): boolean {
  let max = 0;
  for (const seq of log.keys()) if (seq > max) max = seq;
  return lastSeq < max;
}

// ---- input history (localStorage, up-arrow recall) ----

const HISTORY_KEY = "zcode.terminal.history";
const HISTORY_CAP = 50;

export function loadHistory(): string[] {
  try {
    const raw = localStorage.getItem(HISTORY_KEY);
    const parsed = raw ? (JSON.parse(raw) as unknown) : null;
    return Array.isArray(parsed) && parsed.every((x) => typeof x === "string")
      ? (parsed as string[])
      : [];
  } catch {
    return [];
  }
}

/** Record one submitted command; consecutive duplicates collapse (shell convention). */
export function pushHistory(command: string): string[] {
  const list = loadHistory();
  if (list[list.length - 1] !== command) {
    list.push(command);
    while (list.length > HISTORY_CAP) list.shift();
    try {
      localStorage.setItem(HISTORY_KEY, JSON.stringify(list));
    } catch {
      // Private mode / quota: recall still works for this session.
    }
  }
  return list;
}
