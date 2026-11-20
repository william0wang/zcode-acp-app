import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  ArrowUp,
  ChevronLeft,
  Monitor,
  Square,
  SquareTerminal,
  X,
} from "lucide-react";
import {
  loadHistory,
  pushHistory,
  type MirroredPane,
  type TerminalConnState,
  type TerminalEvent,
  type TerminalInfo,
} from "../lib/remoteTerminal";
import {
  ensureMirror,
  ensureTerminal,
  interruptPane,
  reopenTerminal,
  shutdownTerminal,
  terminalConn,
  terminalLog,
  useMirrorLive,
  useTerminalLive,
} from "../lib/terminalSession";
import { useBackHandler } from "../lib/backNav";
import { useAppStore } from "../store/appStore";

// Full-screen Remote Terminal overlay (app ADR-0011 + mirror amendment),
// list-first: the desktop pane mirror (state only — busy badge, current
// command, duration, stop) with the v1 transcript behind a remote-pane tap
// or the per-session affordance. The v1 transcript is the workbench's
// seq-numbered event log folded by seq (reconnect replays deduplicate);
// the composer submits one-line turns; a running turn swaps it to an
// interrupt. One shared socket (lib/terminalSession) carries both
// surfaces; leaving this screen keeps it and the warm transcript (the
// panes stay alive on the desktop either way), so re-entry is instant and
// never spawns a new pane.

function fmtDuration(ms: number | undefined): string {
  if (typeof ms !== "number" || ms < 0) return "";
  return `${(ms / 1000).toFixed(1)}s`;
}

// Whole-second elapsed for running panes (the transcript's turn rows keep
// the finer fmtDuration — there the workbench reports real durations).
function fmtElapsed(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m${(s % 60)}s`;
  return `${Math.floor(m / 60)}h${m % 60}m`;
}

function ConnChip({
  conn,
  terminal,
}: {
  conn: TerminalConnState;
  terminal: TerminalInfo | null;
}) {
  const { t } = useTranslation();
  // Ready + open is the non-state: no chip, just the terminal.
  if (conn === "open" && terminal?.state === "ready" && !terminal.turn) return null;
  let label: string | null = null;
  let tone = "text-warning";
  if (conn === "connecting") label = t("chat.connecting");
  else if (conn === "reconnecting") label = t("chat.reconnecting");
  else if (conn === "closed") (label = t("remoteTerminal.closed")), (tone = "text-danger");
  else if (terminal?.state === "opening") label = t("remoteTerminal.opening");
  else if (terminal?.state === "gone")
    (label = t("remoteTerminal.gone")), (tone = "text-danger");
  if (!label) return null;
  return (
    <span className={`flex shrink-0 items-center gap-1 text-2xs font-medium ${tone}`}>
      <span className="size-1.5 animate-pulse rounded-full bg-current" />
      {label}
    </span>
  );
}

function EventRow({ ev }: { ev: TerminalEvent }) {
  const { t } = useTranslation();
  switch (ev.kind) {
    case "open":
      return (
        <p className="truncate py-1 text-2xs text-faint">
          {t("remoteTerminal.openedAt", { cwd: ev.cwd ?? "" })}
        </p>
      );
    case "close":
      return (
        <p className="py-1 text-2xs text-faint">{t("remoteTerminal.closedPane")}</p>
      );
    case "note":
      return <p className="py-0.5 text-xs italic text-faint">{ev.text}</p>;
    case "input":
      return (
        <p className="whitespace-pre-wrap break-all font-mono text-sm font-medium text-ink">
          <span className="text-faint">$ </span>
          {ev.text}
        </p>
      );
    case "output":
      return (
        <pre className="whitespace-pre-wrap break-all font-mono text-sm leading-relaxed text-dim">
          {ev.text}
        </pre>
      );
    case "exit": {
      const ok = ev.exitCode === 0;
      const dur = fmtDuration(ev.durationMs);
      return (
        <p
          className={`font-mono text-2xs ${ok ? "text-success/90" : "text-danger"}`}
        >
          {t("remoteTerminal.exitLine", {
            code: ev.exitCode ?? "?",
            s: dur || "—",
          })}
        </p>
      );
    }
  }
}

function BusyBadge({ pane }: { pane: MirroredPane }) {
  const { t } = useTranslation();
  if (pane.busy === "busy")
    return (
      <span className="flex shrink-0 items-center gap-1 text-2xs font-medium text-warning">
        <span className="size-1.5 animate-pulse rounded-full bg-current" />
        {t("remoteTerminal.paneRunning")}
      </span>
    );
  if (pane.busy === "idle")
    return (
      <span className="flex shrink-0 items-center gap-1 text-2xs text-faint">
        <span className="size-1.5 rounded-full bg-current/60" />
        {t("remoteTerminal.paneIdle")}
      </span>
    );
  return (
    <span className="flex shrink-0 items-center gap-1 text-2xs text-faint">
      <span className="size-1.5 rounded-full bg-current/40" />
      {t("remoteTerminal.paneUnknown")}
    </span>
  );
}

// One mirrored pane row: state only — cwd, busy badge, the current command
// (busy) or the last exit (idle), a live duration while busy, and the one
// control the mirror has: stop (a plain Ctrl-C, repeatable, no escalation).
// Tap-through to the v1 transcript exists only for remote panes
// (terminalId set) — a purely local pane has no transcript by design.
function PaneRow({
  pane,
  now,
  onOpenTranscript,
}: {
  pane: MirroredPane;
  now: number;
  onOpenTranscript: (pane: MirroredPane) => void;
}) {
  const { t } = useTranslation();
  const elapsed =
    pane.busy === "busy" && pane.sinceMs != null ? fmtElapsed(now - pane.sinceMs) : null;
  const stoppable = pane.busy !== "idle";
  return (
    <div
      role={pane.terminalId != null ? "button" : undefined}
      onClick={pane.terminalId != null ? () => onOpenTranscript(pane) : undefined}
      className={`flex items-center gap-2 rounded-xl bg-raised px-3 py-2.5 ring-1 ring-inset ring-hairline ${
        pane.terminalId != null ? "active:bg-active" : ""
      }`}
    >
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <p className="min-w-0 flex-1 truncate font-mono text-xs text-dim">{pane.cwd}</p>
          <BusyBadge pane={pane} />
        </div>
        <div className="mt-0.5 flex items-center gap-2">
          {pane.busy === "busy" ? (
            <p className="min-w-0 flex-1 truncate font-mono text-2xs text-faint">
              {pane.command ?? ""}
            </p>
          ) : pane.busy === "idle" && pane.lastExit != null ? (
            <p
              className={`min-w-0 flex-1 truncate font-mono text-2xs ${
                pane.lastExit === 0 ? "text-success/90" : "text-danger"
              }`}
            >
              {pane.command != null ? `${pane.command} · ` : ""}
              {t("remoteTerminal.paneExit", { code: pane.lastExit })}
            </p>
          ) : (
            <p className="min-w-0 flex-1 truncate font-mono text-2xs text-faint">
              {pane.command ?? ""}
            </p>
          )}
          {elapsed != null && (
            <span className="shrink-0 font-mono text-2xs text-warning/90">{elapsed}</span>
          )}
        </div>
      </div>
      {stoppable && (
        <button
          onClick={(e) => {
            e.stopPropagation();
            interruptPane(pane.paneId);
          }}
          aria-label={t("remoteTerminal.stop")}
          className="flex size-9 shrink-0 items-center justify-center rounded-lg bg-danger-wash text-danger ring-1 ring-inset ring-danger/40 active:brightness-125"
        >
          <Square className="size-3.5 fill-current" />
        </button>
      )}
    </div>
  );
}

export function TerminalScreen() {
  const { t } = useTranslation();
  const target = useAppStore((s) => s.terminalTarget);
  const profile = useAppStore((s) => s.profile);
  const instances = useAppStore((s) => s.instances);
  const closeTerminalScreen = useAppStore((s) => s.closeTerminal);

  const mirror = useMirrorLive();
  const live = useTerminalLive();
  const [input, setInput] = useState("");
  // The open transcript ("list" view while null). Deep links (a set
  // target.sessionId) start at the transcript; everything else starts at
  // the pane list.
  const [open, setOpen] = useState<{ sessionId: string; title?: string } | null>(
    target?.sessionId ? { sessionId: target.sessionId, title: target.title } : null,
  );

  // Input history is local to the composer; everything terminal-shaped is
  // owned by the session holder and survives this screen.
  const historyRef = useRef<string[]>([]);
  const histIdxRef = useRef<number | null>(null);
  const draftRef = useRef("");
  const scrollRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    if (!profile) return;
    if (open) {
      historyRef.current = loadHistory();
      histIdxRef.current = null;
      ensureTerminal(profile.hubUrl, profile.token, open.sessionId, open.title);
    } else {
      ensureMirror(profile.hubUrl, profile.token);
    }
    // No cleanup: the shared connection outlives the screen by design. It
    // is closed by shutdown, a profile switch, or a hub/server reset.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [profile?.hubUrl, profile?.token, open?.sessionId]);

  const transcript = useMemo(() => {
    void live?.version; // re-derive on transcript churn
    return [...terminalLog().values()].sort((a, b) => a.seq - b.seq);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [live?.version, live?.sessionId]);

  // Follow the tail unless the user scrolled up to read.
  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
    if (atBottom) el.scrollTop = el.scrollHeight;
  }, [transcript]);

  useBackHandler(() => {
    // Transcript → list → close: the hardware back walks one level at a
    // time, mirroring the chevron.
    if (open) setOpen(null);
    else closeTerminalScreen();
    return true;
  });

  // Whole-second tick while any pane runs — the duration column counts up
  // from each pane's sinceMs.
  const [nowTick, setNowTick] = useState(0);
  useEffect(() => {
    if (!mirror.panes.some((p) => p.busy === "busy")) return;
    const id = setInterval(() => setNowTick((n) => n + 1), 1000);
    return () => clearInterval(id);
  }, [mirror.version]);
  const now = useMemo(() => Date.now(), [nowTick]);

  const conn = live?.conn ?? mirror.conn;
  const terminal = live?.terminal ?? null;
  const running = terminal?.turn?.state === "running";
  const canSend = conn === "open" && terminal?.state === "ready" && !running;

  const submit = () => {
    const command = input.trim();
    if (!command || !canSend) return;
    terminalConn()?.sendCommand(command);
    pushHistory(command);
    historyRef.current = loadHistory();
    histIdxRef.current = null;
    draftRef.current = "";
    setInput("");
  };

  // Up/down recall (shell convention): up walks back through history from
  // the draft, down walks forward and past the newest back to the draft.
  const recall = (dir: 1 | -1) => {
    const history = historyRef.current;
    if (history.length === 0) return;
    const idx = histIdxRef.current;
    if (dir === -1) {
      const next = idx === null ? history.length - 1 : Math.max(0, idx - 1);
      if (idx === null) draftRef.current = input;
      histIdxRef.current = next;
      setInput(history[next] ?? "");
    } else if (idx !== null) {
      const next = idx + 1;
      if (next >= history.length) {
        histIdxRef.current = null;
        setInput(draftRef.current);
      } else {
        histIdxRef.current = next;
        setInput(history[next] ?? "");
      }
    }
  };

  if (!target) return null;

  // ---- list view (the mirror) ----

  const titleBySession = new Map<string, string>();
  for (const inst of instances)
    for (const s of inst.sessions ?? [])
      if (s.title && !titleBySession.has(s.sessionId)) titleBySession.set(s.sessionId, s.title);
  const groups = new Map<string, MirroredPane[]>();
  for (const p of mirror.panes) groups.set(p.sessionId, [...(groups.get(p.sessionId) ?? []), p]);

  // A pane whose tab has no session carries sessionId "" — there is no v1
  // terminal to open for it (the workbench would answer unknown_session,
  // which fatally closes the SHARED socket and takes the mirror down too).
  const openTranscript = (sessionId: string, title?: string) => {
    if (!sessionId) return;
    setOpen({ sessionId, title: title ?? titleBySession.get(sessionId) });
  };

  const listError = mirror.error;
  const listConn = mirror.conn;

  const listView = !open && (
    <div className="fixed inset-0 z-40 flex flex-col bg-canvas text-ink">
      <header className="flex shrink-0 items-center gap-1 border-b border-hairline px-1.5 pb-2 pt-[max(var(--safe-top),0.5rem)]">
        <button
          onClick={closeTerminalScreen}
          aria-label={t("common.back")}
          className="flex size-9 shrink-0 items-center justify-center rounded-lg text-dim hover:bg-hover active:bg-active"
        >
          <ChevronLeft className="size-5" />
        </button>
        <div className="min-w-0 flex-1">
          <h1 className="truncate text-sm font-semibold">{t("remoteTerminal.listTitle")}</h1>
        </div>
        <ConnChip conn={listConn} terminal={null} />
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto px-3 py-2">
        {[...groups.entries()].map(([sessionId, panes]) => (
          <section key={sessionId} className="mb-4">
            <div className="mb-1.5 flex items-center gap-2 px-1">
              <Monitor className="size-3.5 shrink-0 text-faint" />
              <h2 className="min-w-0 flex-1 truncate text-xs font-medium text-dim">
                {titleBySession.get(sessionId) ??
                  (panes[0]?.cwd ?? sessionId.slice(0, 8))}
              </h2>
              {/* The v1 open/reopen flow: this session's own terminal,
                  spawnable even with no pane on screen yet. Session-less
                  groups (a workbench pane outside any session tab) get no
                  affordance — there is nothing to open. */}
              {sessionId !== "" && (
                <button
                  onClick={() => openTranscript(sessionId)}
                  aria-label={t("remoteTerminal.openSessionTerminal")}
                  className="flex size-8 shrink-0 items-center justify-center rounded-lg text-faint hover:bg-hover active:bg-active"
                >
                  <SquareTerminal className="size-4" />
                </button>
              )}
            </div>
            <div className="flex flex-col gap-2">
              {panes.map((pane) => (
                <PaneRow
                  key={pane.paneId}
                  pane={pane}
                  now={now}
                  onOpenTranscript={(p) => openTranscript(p.sessionId)}
                />
              ))}
            </div>
          </section>
        ))}
        {groups.size === 0 && listConn === "open" && !listError && (
          <p className="mt-6 text-center text-xs text-faint">
            {t("remoteTerminal.listEmpty")}
          </p>
        )}
        {listError && (
          <div className="mt-3 rounded-xl bg-danger-wash px-3 py-2 ring-1 ring-inset ring-danger/40">
            <p className="text-xs font-medium text-danger">
              {listError.code === "disabled"
                ? t("remoteTerminal.disabled")
                : listError.code === "no_carrier"
                  ? t("remoteTerminal.noCarrier")
                  : listError.message}
            </p>
            {listConn === "closed" && profile && (
              <button
                onClick={() => ensureMirror(profile.hubUrl, profile.token)}
                className="mt-2 rounded-lg bg-ink px-3 py-1.5 text-2xs font-medium text-canvas active:brightness-90"
              >
                {t("common.retry")}
              </button>
            )}
          </div>
        )}
      </div>
    </div>
  );

  // ---- transcript view (v1) ----

  const error = live?.error ?? null;

  const transcriptView = open && (
    <div className="fixed inset-0 z-40 flex flex-col bg-canvas text-ink">
      <header className="flex shrink-0 items-center gap-1 border-b border-hairline px-1.5 pb-2 pt-[max(var(--safe-top),0.5rem)]">
        <button
          onClick={() => setOpen(null)}
          aria-label={t("common.back")}
          className="flex size-9 shrink-0 items-center justify-center rounded-lg text-dim hover:bg-hover active:bg-active"
        >
          <ChevronLeft className="size-5" />
        </button>
        <div className="min-w-0 flex-1">
          <h1 className="truncate text-sm font-semibold">
            {live?.title || open.title || t("chat.untitled")}
          </h1>
          {terminal?.cwd && (
            <p className="truncate font-mono text-2xs text-faint">{terminal.cwd}</p>
          )}
        </div>
        <ConnChip conn={conn} terminal={terminal} />
        <button
          onClick={() => {
            shutdownTerminal();
            closeTerminalScreen();
          }}
          aria-label={t("remoteTerminal.closeAction")}
          className="flex size-9 shrink-0 items-center justify-center rounded-full text-dim hover:bg-hover active:bg-active"
        >
          <X className="size-5" />
        </button>
      </header>

      <div ref={scrollRef} className="min-h-0 flex-1 overflow-y-auto px-3 py-2">
        {transcript.length === 0 && conn === "open" && !error && (
          <p className="mt-6 text-center text-xs text-faint">
            {t("remoteTerminal.empty")}
          </p>
        )}
        <div className="flex flex-col gap-1">
          {transcript.map((ev) => (
            <EventRow key={ev.seq} ev={ev} />
          ))}
        </div>
        {error && (
          <div className="mt-3 rounded-xl bg-danger-wash px-3 py-2 ring-1 ring-inset ring-danger/40">
            <p className="text-xs font-medium text-danger">
              {error.code === "disabled"
                ? t("remoteTerminal.disabled")
                : error.code === "no_carrier"
                  ? t("remoteTerminal.noCarrier")
                  : error.message}
            </p>
          </div>
        )}
      </div>

      <div className="shrink-0 border-t border-hairline px-3 pb-[max(var(--safe-bottom),0.5rem)] pt-2">
        {terminal?.state === "gone" || conn === "closed" ? (
          // Dead end states: the pane is gone (shell exited / closed) or the
          // connection fatally closed (carrier gone…). The only way forward
          // is reopening — a fresh pane, or a full redial when closed. While
          // the ladder is already reconnecting (hub restart), the wire open
          // would be dropped — let the ladder land it instead.
          <button
            onClick={() => reopenTerminal()}
            disabled={conn === "reconnecting"}
            className="w-full rounded-xl bg-ink px-4 py-3 text-sm font-medium text-canvas active:brightness-90 disabled:opacity-50"
          >
            {t("remoteTerminal.reopen")}
          </button>
        ) : running ? (
          <div className="flex items-center gap-2">
            <div className="flex min-w-0 flex-1 items-center gap-2 rounded-xl bg-raised px-3 py-2.5 ring-1 ring-inset ring-hairline">
              <span className="size-1.5 shrink-0 animate-pulse rounded-full bg-info" />
              <span className="truncate font-mono text-sm text-dim">
                {terminal?.turn?.command}
              </span>
            </div>
            <button
              onClick={() => terminalConn()?.interrupt()}
              aria-label={t("remoteTerminal.interrupt")}
              className="flex size-11 shrink-0 items-center justify-center rounded-xl bg-danger-wash text-danger ring-1 ring-inset ring-danger/40 active:brightness-125"
            >
              <Square className="size-4 fill-current" />
            </button>
          </div>
        ) : (
          <div className="flex items-center gap-2">
            <div className="flex min-w-0 flex-1 items-center rounded-xl bg-raised px-3 ring-1 ring-inset ring-hairline focus-within:ring-2 focus-within:ring-info/50">
              <input
                value={input}
                onChange={(e) => setInput(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter" && !e.nativeEvent.isComposing) submit();
                  else if (e.key === "ArrowUp" && e.currentTarget.selectionStart === 0) {
                    e.preventDefault();
                    recall(-1);
                  } else if (e.key === "ArrowDown") {
                    e.preventDefault();
                    recall(1);
                  }
                }}
                placeholder={
                  canSend
                    ? t("remoteTerminal.inputPlaceholder")
                    : t("remoteTerminal.inputWaiting")
                }
                disabled={!canSend}
                autoCapitalize="none"
                autoCorrect="off"
                spellCheck={false}
                enterKeyHint="send"
                className="min-w-0 flex-1 bg-transparent py-2.5 font-mono text-sm text-ink placeholder:font-sans placeholder:text-faint focus:outline-none disabled:opacity-50"
              />
            </div>
            <button
              onClick={submit}
              disabled={!canSend || !input.trim()}
              aria-label={t("remoteTerminal.send")}
              className="flex size-11 shrink-0 items-center justify-center rounded-xl bg-ink text-canvas active:brightness-90 disabled:opacity-50"
            >
              <ArrowUp className="size-5" />
            </button>
          </div>
        )}
      </div>
    </div>
  );

  return (
    <>
      {listView}
      {transcriptView}
    </>
  );
}
