// Remote Terminal wire client (app ADR-0011): one WebSocket to the hub's
// /api/terminals/ws, relayed verbatim onto the desktop workbench's control
// connection. The hub never interprets frames; the workbench owns the
// terminal state and the event log. This side just opens the session's
// terminal, folds the seq-numbered event log into a transcript, and sends
// one-line turns. Reconnect re-`open`s (idempotent — the workbench replays
// its whole ring) and the seq-keyed fold deduplicates; a turn running
// through a drop settles into the log and replays with everything else.
//
// Terminal Mirror (2026-11-20 addition): the SAME socket also carries the
// mirror of the desktop's open panes — `panes` snapshots on (re)connect, live
// `pane` deltas, `pane_interrupt` for the running ones. The mirror has no
// history: reconnect is a fresh snapshot, never a replay.

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

/** One mirrored desktop pane (spec §"The mirrored pane object"): state only —
 *  content never syncs, `busy` is what the shell hooks reported. */
export interface MirroredPane {
  paneId: string;
  sessionId: string;
  cwd: string;
  state: "open" | "gone";
  busy: "busy" | "idle" | "unknown";
  command: string | null;
  sinceMs: number | null;
  lastExit: number | null;
  terminalId: string | null;
}

export interface TerminalHandlers {
  onState: (state: TerminalConnState) => void;
  onTerminal: (terminal: TerminalInfo) => void;
  onEvents: (events: TerminalEvent[]) => void;
  onError: (code: string, message: string) => void;
  /** Terminal Mirror: the snapshot answer (and the reconnect resync). */
  onPanes?: (panes: MirroredPane[]) => void;
  /** Terminal Mirror: one live delta, folded by paneId. */
  onPane?: (pane: MirroredPane) => void;
}

// Codes that make retrying pointless until the user acts: the exec switch is
// off, the workbench doesn't know the session, or — no_carrier — the hub has
// verified the desktop control link is GONE (it errors every client and
// refuses new dials). Laddering that would spin "reconnecting" against a dead
// carrier forever; the terminal settles closed (error banner + reopen/close
// actions) and re-opens when the desktop is back. Plain transport drops DO
// retry — the carrier may be fine under a network blip.
const FATAL_CODES = new Set(["disabled", "unknown_session", "no_carrier"]);

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
    /** null = mirror-only (the list view); set by `attach` on entry. */
    private sessionId: string | null,
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
      // The mirror resyncs first — a fresh snapshot, never a replay (the
      // mirror keeps no history by design).
      ws.send(JSON.stringify({ v: 1, type: "panes" }));
      // Open (re)attaches and replays the whole ring — idempotent, and the
      // seq-keyed fold upstream deduplicates against what we already hold.
      if (this.sessionId)
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
    if (!this.sessionId) return;
    this.send({ v: 1, type: "turn", sessionId: this.sessionId, command });
  }

  /** Ctrl-C for the running turn (same byte a local Ctrl-C writes). */
  interrupt(): void {
    if (!this.sessionId) return;
    this.send({ v: 1, type: "interrupt", sessionId: this.sessionId });
  }

  /** Terminal Mirror: Ctrl-C for ANY desktop pane — same byte, repeatable,
   *  never escalated; the busy→idle delta is the feedback. */
  interruptPane(paneId: string): void {
    this.send({ v: 1, type: "pane_interrupt", paneId });
  }

  /** Attach a session's terminal (list→transcript entry, or a session
   *  switch on the shared connection): the wire `open` rides now when the
   *  socket is up and every reconnect after. */
  attach(sessionId: string): void {
    this.sessionId = sessionId;
    this.send({ v: 1, type: "open", sessionId });
  }

  /** Drop the session attachment (back to mirror-only). The pane itself
   *  stays alive on the desktop — no wire frame. */
  detach(): void {
    this.sessionId = null;
  }

  /** Close the pane itself (wire `close`): the workbench kills the pane and
   *  reports `gone`; a later `open` spawns a fresh one. */
  requestClose(): void {
    if (!this.sessionId) return;
    this.send({ v: 1, type: "close", sessionId: this.sessionId });
  }

  /** Ask for a fresh pane after `gone` (same frame as the attach open). */
  reopen(): void {
    if (!this.sessionId) return;
    this.send({ v: 1, type: "open", sessionId: this.sessionId });
  }

  /** Dial again after a fatal close (transport + attach open in one). */
  redial(): void {
    this.connect();
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
      panes?: unknown;
      pane?: unknown;
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
      case "panes": {
        // The mirror snapshot replaces whatever we held (reconnect resync —
        // there is no cursor, the mirror keeps no history).
        if (Array.isArray(frame.panes))
          this.handlers.onPanes?.(frame.panes.filter(isMirroredPane));
        break;
      }
      case "pane":
        if (isMirroredPane(frame.pane)) this.handlers.onPane?.(frame.pane);
        break;
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

function isMirroredPane(v: unknown): v is MirroredPane {
  if (typeof v !== "object" || v === null) return false;
  const p = v as Record<string, unknown>;
  return (
    typeof p.paneId === "string" &&
    typeof p.sessionId === "string" &&
    typeof p.cwd === "string" &&
    (p.state === "open" || p.state === "gone") &&
    (p.busy === "busy" || p.busy === "idle" || p.busy === "unknown") &&
    (p.command === null || typeof p.command === "string") &&
    (p.sinceMs === null || typeof p.sinceMs === "number") &&
    (p.lastExit === null || typeof p.lastExit === "number") &&
    (p.terminalId === null || typeof p.terminalId === "string")
  );
}

/**
 * Fold one live `pane` delta into the mirror list (idempotent by paneId): a
 * `gone` delta drops the row — there is nothing to re-open, a NEW pane the
 * user opens is a new paneId. A snapshot supersedes everything prior.
 */
export function foldPane(panes: MirroredPane[], delta: MirroredPane): MirroredPane[] {
  const kept = panes.filter((p) => p.paneId !== delta.paneId);
  if (delta.state === "gone") return kept;
  kept.push(delta);
  kept.sort((a, b) => (a.paneId < b.paneId ? -1 : a.paneId > b.paneId ? 1 : 0));
  return kept;
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
