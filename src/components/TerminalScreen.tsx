import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { ArrowUp, ChevronLeft, Square, X } from "lucide-react";
import {
  loadHistory,
  pushHistory,
  type TerminalConnState,
  type TerminalEvent,
  type TerminalInfo,
} from "../lib/remoteTerminal";
import {
  ensureTerminal,
  reopenTerminal,
  shutdownTerminal,
  terminalConn,
  terminalLog,
  useTerminalLive,
} from "../lib/terminalSession";
import { useBackHandler } from "../lib/backNav";
import { useAppStore } from "../store/appStore";

// Full-screen Remote Terminal overlay (app ADR-0011): the mobile wiring into
// the desktop workbench's session terminal. The transcript is the workbench's
// seq-numbered event log folded by seq (reconnect replays deduplicate); the
// composer submits one-line turns; a running turn swaps it to an interrupt.
// The terminal itself lives in lib/terminalSession — leaving this screen
// keeps the socket and the warm transcript (the pane stays alive on the
// desktop either way), so re-entry is instant and never spawns a new pane.

function fmtDuration(ms: number | undefined): string {
  if (typeof ms !== "number" || ms < 0) return "";
  return `${(ms / 1000).toFixed(1)}s`;
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
  let tone = "text-amber-400";
  if (conn === "connecting") label = t("chat.connecting");
  else if (conn === "reconnecting") label = t("chat.reconnecting");
  else if (conn === "closed") (label = t("remoteTerminal.closed")), (tone = "text-red-400");
  else if (terminal?.state === "opening") label = t("remoteTerminal.opening");
  else if (terminal?.state === "gone")
    (label = t("remoteTerminal.gone")), (tone = "text-red-400");
  if (!label) return null;
  return (
    <span className={`flex shrink-0 items-center gap-1 text-[10px] font-medium ${tone}`}>
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
        <p className="truncate py-1 text-[10px] text-faint">
          {t("remoteTerminal.openedAt", { cwd: ev.cwd ?? "" })}
        </p>
      );
    case "close":
      return (
        <p className="py-1 text-[10px] text-faint">{t("remoteTerminal.closedPane")}</p>
      );
    case "note":
      return <p className="py-0.5 text-xs italic text-faint">{ev.text}</p>;
    case "input":
      return (
        <p className="whitespace-pre-wrap break-all font-mono text-[13px] font-medium text-ink">
          <span className="text-faint">$ </span>
          {ev.text}
        </p>
      );
    case "output":
      return (
        <pre className="whitespace-pre-wrap break-all font-mono text-[13px] leading-relaxed text-dim">
          {ev.text}
        </pre>
      );
    case "exit": {
      const ok = ev.exitCode === 0;
      const dur = fmtDuration(ev.durationMs);
      return (
        <p
          className={`font-mono text-[11px] ${ok ? "text-emerald-400/90" : "text-red-400"}`}
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

export function TerminalScreen() {
  const { t } = useTranslation();
  const target = useAppStore((s) => s.terminalTarget);
  const profile = useAppStore((s) => s.profile);
  const closeTerminalScreen = useAppStore((s) => s.closeTerminal);

  const live = useTerminalLive();
  const [input, setInput] = useState("");

  // Input history is local to the composer; everything terminal-shaped is
  // owned by the session holder and survives this screen.
  const historyRef = useRef<string[]>([]);
  const histIdxRef = useRef<number | null>(null);
  const draftRef = useRef("");
  const scrollRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    if (!profile || !target) return;
    historyRef.current = loadHistory();
    histIdxRef.current = null;
    ensureTerminal(profile.hubUrl, profile.token, target.sessionId, target.title);
    // No cleanup: the connection outlives the screen by design. It is closed
    // by opening another session's terminal or a hub/server reset.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [profile?.hubUrl, profile?.token, target?.sessionId]);

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
    closeTerminalScreen();
    return true;
  });

  const conn = live?.conn ?? "connecting";
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
  const error = live?.error ?? null;

  return (
    <div className="fixed inset-0 z-40 flex flex-col bg-canvas text-ink">
      <header className="flex shrink-0 items-center gap-1 border-b border-hairline px-1.5 pb-2 pt-[max(var(--safe-top),0.5rem)]">
        <button
          onClick={closeTerminalScreen}
          aria-label={t("common.back")}
          className="flex size-9 shrink-0 items-center justify-center rounded-lg text-dim active:bg-white/[0.06]"
        >
          <ChevronLeft className="size-5" />
        </button>
        <div className="min-w-0 flex-1">
          <h1 className="truncate text-sm font-semibold">
            {live?.title || target.title || t("chat.untitled")}
          </h1>
          {terminal?.cwd && (
            <p className="truncate font-mono text-[10px] text-faint">{terminal.cwd}</p>
          )}
        </div>
        <ConnChip conn={conn} terminal={terminal} />
        <button
          onClick={() => {
            shutdownTerminal();
            closeTerminalScreen();
          }}
          aria-label={t("remoteTerminal.closeAction")}
          className="flex size-9 shrink-0 items-center justify-center rounded-full text-dim active:bg-white/[0.06]"
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
          <div className="mt-3 rounded-xl bg-red-500/10 px-3 py-2 ring-1 ring-inset ring-red-500/40">
            <p className="text-xs font-medium text-red-400">
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
            className="w-full rounded-xl bg-blue-500 px-4 py-3 text-sm font-medium text-white active:bg-blue-600 disabled:opacity-50"
          >
            {t("remoteTerminal.reopen")}
          </button>
        ) : running ? (
          <div className="flex items-center gap-2">
            <div className="flex min-w-0 flex-1 items-center gap-2 rounded-xl bg-raised px-3 py-2.5 ring-1 ring-inset ring-hairline">
              <span className="size-1.5 shrink-0 animate-pulse rounded-full bg-blue-400" />
              <span className="truncate font-mono text-sm text-dim">
                {terminal?.turn?.command}
              </span>
            </div>
            <button
              onClick={() => terminalConn()?.interrupt()}
              aria-label={t("remoteTerminal.interrupt")}
              className="flex size-11 shrink-0 items-center justify-center rounded-xl bg-red-500/15 text-red-400 ring-1 ring-inset ring-red-500/40 active:bg-red-500/25"
            >
              <Square className="size-4 fill-current" />
            </button>
          </div>
        ) : (
          <div className="flex items-center gap-2">
            <div className="flex min-w-0 flex-1 items-center rounded-xl bg-raised px-3 ring-1 ring-inset ring-hairline focus-within:ring-blue-500/50">
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
              className="flex size-11 shrink-0 items-center justify-center rounded-xl bg-blue-500 text-white active:bg-blue-600 disabled:opacity-50"
            >
              <ArrowUp className="size-5" />
            </button>
          </div>
        )}
      </div>
    </div>
  );
}
