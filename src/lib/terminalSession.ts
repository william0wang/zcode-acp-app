// Session-scoped Remote Terminal holder (app ADR-0011): the terminal
// outlives its screen. Leaving the overlay keeps the socket up — the phone
// keeps watching, a running turn settles into the warm transcript, and the
// chat-header dot stays truthful; re-entry is instant (same pane, same log,
// replay dedupes by seq). One background terminal at a time: opening a
// different session's terminal replaces it, hub/server resets drop it.
import { useEffect, useState } from "react";
import {
  TerminalConnection,
  mergeEvents,
  registryRestarted,
  type TerminalConnState,
  type TerminalEvent,
  type TerminalInfo,
} from "./remoteTerminal";

export interface TerminalError {
  code: string;
  message: string;
}

// The reactive snapshot consumers subscribe to. `version` bumps on every
// transcript/terminal change so views can re-derive from the log map ref.
export interface TerminalLiveState {
  sessionId: string;
  title: string | null;
  conn: TerminalConnState;
  terminal: TerminalInfo | null;
  error: TerminalError | null;
  version: number;
}

type Listener = (s: TerminalLiveState | null) => void;

let conn: TerminalConnection | null = null;
let log = new Map<number, TerminalEvent>();
let state: TerminalLiveState | null = null;
const listeners = new Set<Listener>();

function emit(): void {
  for (const l of listeners) l(state);
}

/** The live transcript fold (seq-keyed). Read-only for consumers. */
export function terminalLog(): Map<number, TerminalEvent> {
  return log;
}

export function terminalLive(): TerminalLiveState | null {
  return state;
}

/** The shared connection the terminal screen submits through. */
export function terminalConn(): TerminalConnection | null {
  return conn;
}

export function subscribeTerminal(l: Listener): () => void {
  listeners.add(l);
  l(state);
  return () => {
    listeners.delete(l);
  };
}

/** React binding for the header/overlay views. */
export function useTerminalLive(): TerminalLiveState | null {
  const [live, setLive] = useState<TerminalLiveState | null>(state);
  useEffect(() => subscribeTerminal(setLive), []);
  return live;
}

/**
 * Idempotent per session: the same session is a no-op (the overlay just
 * reattaches); a different session closes the previous terminal first.
 */
export function ensureTerminal(
  hubUrl: string,
  token: string,
  sessionId: string,
  title?: string | null,
): void {
  if (conn && state && state.sessionId === sessionId) {
    const nextTitle = title ?? null;
    if (nextTitle !== state.title) {
      state = { ...state, title: nextTitle };
      emit();
    }
    return;
  }
  closeTerminal();
  state = {
    sessionId,
    title: title ?? null,
    conn: "connecting",
    terminal: null,
    error: null,
    version: 0,
  };
  emit();
  conn = new TerminalConnection(hubUrl, token, sessionId, {
    onState: (c) => {
      if (!state) return;
      state = { ...state, conn: c };
      emit();
    },
    onTerminal: (terminal) => {
      if (!state) return;
      // A registry restart (workbench relaunch) resets the seq space — drop
      // the old epoch instead of interleaving two of them.
      if (registryRestarted(log, terminal.lastSeq)) log = new Map();
      state = { ...state, terminal, version: state.version + 1 };
      emit();
    },
    onEvents: (events) => {
      if (!state) return;
      mergeEvents(log, events);
      state = { ...state, version: state.version + 1 };
      emit();
    },
    onError: (code, message) => {
      if (!state) return;
      state = { ...state, error: { code, message }, version: state.version + 1 };
      emit();
    },
  });
  conn.connect();
}

/** Drop the terminal entirely (connection, transcript, state). */
export function closeTerminal(): void {
  conn?.close();
  conn = null;
  log = new Map();
  if (state) {
    state = null;
    emit();
  }
}
