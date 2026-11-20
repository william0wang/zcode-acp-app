// Shared Remote Terminal holder (app ADR-0011 + mirror amendment): ONE
// socket serves both surfaces — the pane mirror rides always, the v1
// session terminal attaches on ENTRY (open-on-entry, not open-on-connect)
// and settles into the background when its screen closes. Leaving a screen
// keeps the socket up — the phone keeps watching, a running turn settles
// into the warm transcript, and the chat-header dot stays truthful;
// re-entry is instant (same pane, same log, replay dedupes by seq). One
// background terminal at a time: attaching a different session replaces
// it on the same socket with a fresh log; hub/server resets drop the lot.
import { useEffect, useState } from "react";
import {
  TerminalConnection,
  foldPane,
  mergeEvents,
  registryRestarted,
  type MirroredPane,
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

/** The desktop pane mirror's live snapshot (spec §Mirror): state only. */
export interface MirrorLiveState {
  conn: TerminalConnState;
  panes: MirroredPane[];
  error: TerminalError | null;
  version: number;
}

type SessionListener = (s: TerminalLiveState | null) => void;
type MirrorListener = (s: MirrorLiveState) => void;

// The one shared connection + the profile it was dialed for (a hub switch
// rebuilds it; both stores ride the same handlers).
let conn: TerminalConnection | null = null;
let connKey = "";
let log = new Map<number, TerminalEvent>();
let state: TerminalLiveState | null = null;
const listeners = new Set<SessionListener>();

let mirror: MirrorLiveState = { conn: "closed", panes: [], error: null, version: 0 };
const mirrorListeners = new Set<MirrorListener>();

function emit(): void {
  for (const l of listeners) l(state);
}

function emitMirror(): void {
  for (const l of mirrorListeners) l(mirror);
}

/** The live transcript fold (seq-keyed). Read-only for consumers. */
export function terminalLog(): Map<number, TerminalEvent> {
  return log;
}

export function terminalLive(): TerminalLiveState | null {
  return state;
}

/** The mirror's current snapshot (read-only getter, symmetric with the above). */
export function mirrorLive(): MirrorLiveState {
  return mirror;
}

/** The shared connection the terminal screen submits through. */
export function terminalConn(): TerminalConnection | null {
  return conn;
}

export function subscribeTerminal(l: SessionListener): () => void {
  listeners.add(l);
  l(state);
  return () => {
    listeners.delete(l);
  };
}

export function subscribeMirror(l: MirrorListener): () => void {
  mirrorListeners.add(l);
  l(mirror);
  return () => {
    mirrorListeners.delete(l);
  };
}

/** React binding for the header/overlay views (the v1 terminal). */
export function useTerminalLive(): TerminalLiveState | null {
  const [live, setLive] = useState<TerminalLiveState | null>(state);
  useEffect(() => subscribeTerminal(setLive), []);
  return live;
}

/** React binding for the pane-list view. */
export function useMirrorLive(): MirrorLiveState {
  const [live, setLive] = useState<MirrorLiveState>(mirror);
  useEffect(() => subscribeMirror(setLive), []);
  return live;
}

// One handler set feeds both stores: transport state and errors are shared
// (same socket, same fate — a fatal close kills the mirror too), while
// terminal/transcript frames only touch the session side and pane frames
// only the mirror.
function buildHandlers() {
  return {
    onState: (c: TerminalConnState) => {
      // Reconnected ⇒ whatever error the drop carried (hub restarting,
      // carrier gone…) no longer describes the world.
      if (c === "open") {
        if (state) state = { ...state, conn: c, error: null };
        mirror = { ...mirror, conn: c, error: null };
      } else {
        if (state) state = { ...state, conn: c };
        mirror = { ...mirror, conn: c };
      }
      emit();
      emitMirror();
    },
    onTerminal: (terminal: TerminalInfo) => {
      if (!state) return;
      // A registry restart (workbench relaunch) resets the seq space — drop
      // the old epoch instead of interleaving two of them.
      if (registryRestarted(log, terminal.lastSeq)) log = new Map();
      state = { ...state, terminal, version: state.version + 1 };
      emit();
    },
    onEvents: (events: TerminalEvent[]) => {
      if (!state) return;
      mergeEvents(log, events);
      state = { ...state, version: state.version + 1 };
      emit();
    },
    onError: (code: string, message: string) => {
      const error = { code, message };
      if (state) state = { ...state, error, version: state.version + 1 };
      mirror = { ...mirror, error, version: mirror.version + 1 };
      emit();
      emitMirror();
    },
    onPanes: (panes: MirroredPane[]) => {
      // The snapshot replaces whatever we held (reconnect resync — the
      // mirror keeps no history by design). Sorted defensively so the
      // store's "by paneId" order holds even against an unsorted sender.
      // Fresh data also clears a transient error (`unknown_pane` and
      // friends land on a LIVE socket — only new frames prove recovery).
      const sorted = [...panes].sort((a, b) =>
        a.paneId < b.paneId ? -1 : a.paneId > b.paneId ? 1 : 0,
      );
      mirror = { ...mirror, panes: sorted, error: null, version: mirror.version + 1 };
      emitMirror();
    },
    onPane: (pane: MirroredPane) => {
      mirror = {
        ...mirror,
        panes: foldPane(mirror.panes, pane),
        error: null,
        version: mirror.version + 1,
      };
      emitMirror();
    },
  };
}

function dial(
  hubUrl: string,
  token: string,
  sessionId: string | null,
  title?: string | null,
): void {
  connKey = `${hubUrl}\n${token}`;
  conn = new TerminalConnection(hubUrl, token, sessionId, buildHandlers());
  if (sessionId) {
    state = {
      sessionId,
      title: title ?? null,
      conn: "connecting",
      terminal: null,
      error: null,
      version: 0,
    };
  }
  mirror = { ...mirror, conn: "connecting", error: null, version: mirror.version + 1 };
  emit();
  emitMirror();
  conn.connect();
}

/**
 * The pane list's connection (mirror-only, sessionId null): reuse the
 * shared socket when it is live for this profile, dial fresh otherwise.
 * A fatally-closed socket (carrier gone, exec off) redials on re-entry —
 * the panes snapshot answers on connect.
 */
export function ensureMirror(hubUrl: string, token: string): void {
  const key = `${hubUrl}\n${token}`;
  if (conn && connKey === key) {
    if (mirror.conn !== "closed") return;
    if (state) {
      state = { ...state, conn: "connecting", error: null };
      emit();
    }
    mirror = { ...mirror, conn: "connecting", error: null, version: mirror.version + 1 };
    emitMirror();
    conn.redial();
    return;
  }
  closeTerminal();
  dial(hubUrl, token, null);
}

/**
 * Idempotent per session: the same session is a no-op (the transcript just
 * reattaches); a different session attaches on the LIVE socket (wire `open`
 * rides now, fresh log — the previous session's pane stays alive on the
 * desktop, exactly as v1's socket close left it).
 */
export function ensureTerminal(
  hubUrl: string,
  token: string,
  sessionId: string,
  title?: string | null,
): void {
  const key = `${hubUrl}\n${token}`;
  if (conn && connKey !== key) closeTerminal();
  if (!conn) {
    dial(hubUrl, token, sessionId, title);
    return;
  }
  if (state && state.sessionId === sessionId) {
    const nextTitle = title ?? null;
    if (nextTitle !== state.title) {
      state = { ...state, title: nextTitle };
      emit();
    }
    // A fatally-closed holder (carrier gone, exec off, unknown session)
    // must not no-op: re-entry redials instead of staring at a dead
    // terminal.
    if (state.conn !== "closed") return;
    state = { ...state, conn: "connecting", error: null };
    emit();
    mirror = { ...mirror, conn: "connecting", error: null, version: mirror.version + 1 };
    emitMirror();
    conn.redial();
    return;
  }
  // Session switch on the live socket: attach sends the wire `open` when
  // the socket is up (otherwise onopen lands it) and the transcript starts
  // over for the new pane.
  const wasClosed = mirror.conn === "closed";
  log = new Map();
  state = {
    sessionId,
    title: title ?? null,
    conn: wasClosed ? "connecting" : mirror.conn,
    terminal: null,
    error: null,
    version: 0,
  };
  emit();
  conn.attach(sessionId);
  if (wasClosed) {
    mirror = { ...mirror, conn: "connecting", error: null, version: mirror.version + 1 };
    emitMirror();
    conn.redial();
  }
}

/** Terminal Mirror: Ctrl-C any desktop pane (repeatable, no escalation). */
export function interruptPane(paneId: string): void {
  conn?.interruptPane(paneId);
}

/** Drop the connection, transcript, session state AND the mirror. */
export function closeTerminal(): void {
  conn?.close();
  conn = null;
  connKey = "";
  log = new Map();
  if (state) state = null;
  mirror = { conn: "closed", panes: [], error: null, version: mirror.version + 1 };
  emit();
  emitMirror();
}

/**
 * Close the terminal EVERYWHERE: the workbench pane (wire `close`) and the
 * phone-side holder. Re-entry afterwards spawns a fresh pane.
 */
export function shutdownTerminal(): void {
  conn?.requestClose();
  closeTerminal();
}

/**
 * Re-open this session's terminal: after `gone` (shell exited, pane closed)
 * that is a wire `open` — the workbench spawns a fresh pane, the transcript
 * continues; after a fatal close (carrier gone…) it is a full redial, the
 * onopen attach re-`open`s by itself.
 */
export function reopenTerminal(): void {
  if (!conn || !state) return;
  if (state.conn === "closed") {
    state = { ...state, conn: "connecting", error: null };
    emit();
    mirror = { ...mirror, conn: "connecting", error: null, version: mirror.version + 1 };
    emitMirror();
    conn.redial();
    return;
  }
  conn.reopen();
}
