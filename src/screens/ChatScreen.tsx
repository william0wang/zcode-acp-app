import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  Check,
  ChevronDown,
  ChevronRight,
  Circle,
  Menu,
  SlidersHorizontal,
} from "lucide-react";
import { ChatView } from "../chat/ChatView";
import { Drawer } from "../components/Drawer";
import { ProjectCreateDialog } from "../components/ProjectCreateDialog";
import { ProjectHistoryDialog } from "../components/ProjectHistoryDialog";
import { SettingsPanel } from "../components/SidePanel";
import { SessionPanel } from "../components/SessionPanel";
import { FileBrowser } from "../components/FileBrowser";
import { PermissionDialog } from "../components/PermissionDialog";
import { ElicitationDialog } from "../components/ElicitationDialog";
import { Spinner } from "../components/Spinner";
import { ConfigScreenHost } from "./ConfigScreenHost";
import { useBackHandler } from "../lib/backNav";
import { useTerminalLive } from "../lib/terminalSession";
import { haptic } from "../lib/haptic";
import { Popover } from "../ui";
import { useAppStore, type PlanEntry } from "../store/appStore";

function baseName(path: string | undefined): string {
  if (!path) return "";
  const parts = path.replace(/\/+$/, "").split("/");
  return parts[parts.length - 1] || path;
}

// TUI parity (martty stats dock): `128000` -> `128K`, `1000000` -> `1.0M`.
function fmtTokens(n: number): string {
  if (n < 1000) return String(n);
  const unit = n < 1_000_000 ? 1000 : 1_000_000;
  const scaled = n / unit;
  const rounded =
    scaled >= 100 ? Math.round(scaled) : Math.round(scaled * 10) / 10;
  return `${rounded}${unit === 1000 ? "K" : "M"}`;
}

// Context gauge as the TUI prints it — `17.5%/1.0M`, a one-decimal percent
// of the real model window, not the raw used/size counts.
function fmtContext(used: number, size: number): string {
  return `${Math.min(100, (used / size) * 100).toFixed(1)}%/${fmtTokens(size)}`;
}

// Plan panel (requirements 1.8): auto-expands while a turn runs (the plan
// is live progress then), collapses when idle; the first manual toggle wins
// permanently (the ThoughtCard rule). The summary carries a done/total
// progress bar.
function PlanPanel({ entries }: { entries: PlanEntry[] }) {
  const { t } = useTranslation();
  const isRunning = useAppStore((s) => s.isRunning);
  const [open, setOpen] = useState(false);
  const [touched, setTouched] = useState(false);
  useEffect(() => {
    if (!touched) setOpen(isRunning);
  }, [isRunning, touched]);
  const done = entries.filter((e) => e.status === "completed").length;
  return (
    <div className="border-b border-hairline bg-surface/60 px-4 py-2 text-xs text-dim">
      <button
        onClick={() => {
          setTouched(true);
          setOpen((o) => !o);
        }}
        className="flex w-full select-none items-center gap-1.5 text-left text-dim"
      >
        <ChevronDown
          className={`size-3.5 shrink-0 text-faint transition-transform ${open ? "rotate-180" : ""}`}
        />
        <span className="shrink-0">
          {t("chat.plan")} · {done}/{entries.length}
        </span>
        <span className="ml-2 h-1 min-w-8 flex-1 overflow-hidden rounded-full bg-hover">
          <span
            className="block h-full rounded-full bg-info transition-[width] duration-300"
            style={{ width: `${(done / Math.max(entries.length, 1)) * 100}%` }}
          />
        </span>
      </button>
      {open && (
        <ul className="mt-1.5 flex flex-col gap-1 pb-1">
          {entries.map((e, i) => (
            <li key={i} className="flex items-start gap-1.5">
              {e.status === "completed" ? (
                <Check className="mt-0.5 size-3.5 shrink-0 text-success" />
              ) : e.status === "active" ? (
                <ChevronRight className="mt-0.5 size-3.5 shrink-0 text-info" />
              ) : (
                <Circle className="mt-0.5 size-3.5 shrink-0 text-faint" />
              )}
              <span
                className={
                  e.status === "completed"
                    ? "text-faint line-through"
                    : "text-dim"
                }
              >
                {e.content}
              </span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

// Context stats (requirements 1.6/1.7): the always-visible segment stays
// minimal (percent + window); tps, cache and the quota dock fold into this
// popover, which is where the ≥80% warning and the compact entry live.
function ContextStats({
  usage,
  cacheHit,
  turnStats,
  quotaDock,
}: {
  usage: { used: number; size: number };
  cacheHit: number | null;
  turnStats: { tps: number; outputTokens: number; seconds: number } | null;
  quotaDock: string | null;
}) {
  const { t } = useTranslation();
  const isRunning = useAppStore((s) => s.isRunning);
  const sendPrompt = useAppStore((s) => s.sendPrompt);
  const availableCommands = useAppStore((s) => s.availableCommands);
  const pct = Math.min(100, (usage.used / usage.size) * 100);
  const high = pct >= 80;
  const compactOk =
    !isRunning &&
    availableCommands.some((c) => c.name.replace(/^\$/, "") === "compact");
  const row = "flex items-center justify-between gap-4";
  return (
    <Popover
      trigger={
        <button
          className={`shrink-0 whitespace-nowrap ${high ? "font-semibold text-warning" : ""}`}
          aria-label={t("chat.statsTitle")}
        >
          {fmtContext(usage.used, usage.size)}
        </button>
      }
      align="start"
      className="w-64"
    >
      <div className="flex flex-col gap-1.5 px-2 py-1.5 text-xs text-dim">
        <div className={row}>
          <span className="text-faint">{t("chat.statsTitle")}</span>
          <span className={high ? "font-medium text-warning" : ""}>
            {pct.toFixed(1)}% · {fmtTokens(usage.used)}/{fmtTokens(usage.size)}
          </span>
        </div>
        {cacheHit != null && (
          <div className={row}>
            <span className="text-faint">{t("chat.statsCache")}</span>
            <span className="tabular-nums">{cacheHit}%</span>
          </div>
        )}
        {turnStats && (
          <div className={row}>
            <span className="text-faint">{t("chat.statsSpeed")}</span>
            <span className="tabular-nums">
              {turnStats.tps} tok/s · {turnStats.seconds}s
            </span>
          </div>
        )}
        {quotaDock && (
          <div className={`${row} border-t border-hairline pt-1.5`}>
            <span className="whitespace-nowrap text-faint">{quotaDock}</span>
          </div>
        )}
        {high && (
          <div className="border-t border-hairline pt-1.5">
            <p className="text-warning">{t("chat.contextHigh")}</p>
            {compactOk && (
              <button
                onClick={() => {
                  haptic();
                  void sendPrompt("/compact");
                }}
                className="mt-1.5 w-full rounded-lg bg-raised px-3 py-2 text-xs font-medium text-ink ring-1 ring-inset ring-hairline hover:bg-hover active:bg-active"
              >
                {t("chat.compact")}
              </button>
            )}
          </div>
        )}
      </div>
    </Popover>
  );
}

// Connect overlay (requirements 1.9): after 10s the spinner grows a hint
// and a cancel that tears the attempt down (no silent auto-retry).
function ConnectingOverlay() {
  const { t } = useTranslation();
  const abortConnection = useAppStore((s) => s.abortConnection);
  const [slow, setSlow] = useState(false);
  useEffect(() => {
    const id = setTimeout(() => setSlow(true), 10_000);
    return () => clearTimeout(id);
  }, []);
  return (
    <div className="absolute inset-0 z-30 flex flex-col items-center justify-center gap-3 bg-canvas/90">
      <Spinner className="size-8" />
      <div className="text-sm text-dim">{t("chat.connecting")}</div>
      {slow && (
        <>
          <div className="max-w-64 text-center text-xs text-faint">
            {t("chat.connectSlow")}
          </div>
          <button
            onClick={() => {
              haptic();
              abortConnection();
            }}
            className="rounded-xl bg-raised px-4 py-2.5 text-sm font-medium text-ink ring-1 ring-inset ring-hairline hover:bg-hover active:bg-active"
          >
            {t("common.cancel")}
          </button>
        </>
      )}
    </div>
  );
}

export function ChatScreen() {
  const { t } = useTranslation();
  const [drawerOpen, setDrawerOpen] = useState(false);
  const [panelOpen, setPanelOpen] = useState(false);
  const [filesOpen, setFilesOpen] = useState(false);
  // Session-independent overlays reachable from the drawer: they live here,
  // not in the drawer, which unmounts (and would take them down) on close.
  const [createOpen, setCreateOpen] = useState(false);
  const [historyOpen, setHistoryOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const instances = useAppStore((s) => s.instances);
  const instanceId = useAppStore((s) => s.instanceId);
  const activeSessionId = useAppStore((s) => s.activeSessionId);
  const connState = useAppStore((s) => s.connState);
  const planEntries = useAppStore((s) => s.planEntries);
  const notice = useAppStore((s) => s.notice);
  const usage = useAppStore((s) => s.usage);
  const dismissNotice = useAppStore((s) => s.dismissNotice);
  const notify = useAppStore((s) => s.notify);
  const configOpen = useAppStore((s) => s.configOpen);
  const closeSession = useAppStore((s) => s.closeSession);
  const fileBrowserTarget = useAppStore((s) => s.fileBrowserTarget);
  const clearFileBrowserTarget = useAppStore((s) => s.clearFileBrowserTarget);
  // The header is too narrow for per-tool buttons: terminal and files enter
  // from the session panel. The panel button still carries this session's
  // Remote Terminal (ADR-0011) background state — amber pulse while a turn
  // runs, blue while the socket is up.
  const terminalLive = useTerminalLive();

  // Route-level back: leave the session for the picker. Overlays mounted on
  // top (drawer, panels, file browser, config screens) register later and so
  // answer the gesture first. A pending permission/elicitation is a modal
  // agent question — the gesture neither dismisses it (that would be an
  // accidental answer) nor navigates under it, so it is consumed as a no-op.
  // It lives here rather than in the dialogs because children register
  // BEFORE this screen (effects run bottom-up), which would put their
  // no-op below closeSession in the stack.
  useBackHandler(() => {
    const s = useAppStore.getState();
    const sid = s.activeSessionId;
    if (sid && (s.permissions[sid] || s.elicitations[sid])) return true;
    closeSession();
    return true;
  });

  // Android download feedback (MainActivity): the fallback path (pre-Android
  // 10) hands file downloads to the system DownloadManager, which the page
  // cannot observe directly — the native side reports started/done/failed
  // over this DOM event, with the DownloadManager failure reason attached.
  // Lives here, not in FileViewer, because a download can finish
  // after the viewer has closed.
  useEffect(() => {
    const onDownload = (e: Event) => {
      const { name, state, reason } = (
        e as CustomEvent<{
          name: string;
          state: string;
          reason?: string | null;
        }>
      ).detail;
      if (!name) return;
      notify(
        state === "started"
          ? t("viewer.downloadStarted", { name })
          : state === "done"
            ? t("viewer.downloadDone", { name })
            : t("viewer.downloadFailed", {
                name: reason ? `${name} (${reason})` : name,
              }),
      );
    };
    window.addEventListener("zcode:download", onDownload);
    return () => window.removeEventListener("zcode:download", onDownload);
  }, [t, notify]);

  const instance = instances.find((i) => i.id === instanceId);
  const session = instance?.sessions?.find(
    (s) => s.sessionId === activeSessionId,
  );
  const workspace = baseName(instance?.workspace);
  const messages = useAppStore((s) => s.messages);
  const cacheHit = useAppStore((s) => s.cacheHit);
  const turnStats = useAppStore((s) => s.turnStats);
  const quotaDock = useAppStore((s) => s.quotaDock);

  // Discovery titles are often absent; fall back to the first user message
  // of the replayed history, then to the placeholder.
  let derived: string | null = null;
  for (const m of messages) {
    if (m.role !== "user") continue;
    const part = m.parts.find((p) => p.type === "text");
    if (part && part.type === "text" && part.text.trim()) {
      derived = part.text.trim();
      break;
    }
  }
  const title = session?.title || derived?.slice(0, 60) || t("chat.untitled");

  const statusDot =
    connState === "open"
      ? "bg-success"
      : connState === "reconnecting"
        ? "bg-warning animate-pulse"
        : "bg-faint";

  const noticeText = notice
    ? notice.startsWith("notice.")
      ? t(notice)
      : notice
    : null;

  // ZCode configuration (ADR-0009) takes over the whole screen: a
  // full-screen task, not an overlay over a live chat. The session stays
  // attached underneath — its WS keeps receiving broadcasts — and returning
  // picks the transcript back up where it was. Placed after every hook.
  if (configOpen) return <ConfigScreenHost />;

  return (
    <div className="relative flex h-full flex-col bg-canvas text-ink">
      <header className="@container relative flex items-center gap-1 border-b border-hairline px-1.5 pb-2 pt-[max(var(--safe-top),0.5rem)]">
        <button
          onClick={() => setDrawerOpen(true)}
          aria-label={t("chat.sessions")}
          className="flex size-9 shrink-0 items-center justify-center rounded-full text-dim hover:bg-hover active:bg-active"
        >
          <Menu className="size-5" />
        </button>
        <div className="min-w-0 flex-1">
          <div className="truncate text-base font-medium">{title}</div>
          {workspace && (
            <div className="flex items-baseline text-2xs text-faint">
              {/* Stats live in the title bar, TUI style: the context chip is
                  the always-visible segment (`17.5%/1.0M`); cache, tok/s and
                  the quota dock line (ADR-0021) fold into its popover, so
                  nothing is container-gated away anymore (requirements 1.6).
                  ≥80% turns the chip and the header bar warning (1.7). */}
              <span className="min-w-0 truncate">{workspace}</span>
              {usage != null && usage.size > 0 && (
                <>
                  <span className="shrink-0">{" · "}</span>
                  <ContextStats
                    usage={usage}
                    cacheHit={cacheHit}
                    turnStats={turnStats}
                    quotaDock={quotaDock}
                  />
                </>
              )}
            </div>
          )}
        </div>
        <span
          className={`mr-1.5 size-2 shrink-0 rounded-full ${statusDot}`}
          aria-label={connState}
        />
        <button
          onClick={() => setPanelOpen(true)}
          aria-label={t("panel.session")}
          className="relative flex size-9 shrink-0 items-center justify-center rounded-full text-dim hover:bg-hover active:bg-active"
        >
          <SlidersHorizontal className="size-5" />
          {activeSessionId &&
            terminalLive?.sessionId === activeSessionId &&
            (terminalLive.terminal?.turn?.state === "running" ? (
              <span className="absolute right-1.5 top-1.5 size-1.5 animate-pulse rounded-full bg-warning" />
            ) : terminalLive.conn === "open" &&
                terminalLive.terminal?.state !== "gone" ? (
              <span className="absolute right-1.5 top-1.5 size-1.5 rounded-full bg-info/70" />
            ) : null)}
        </button>
        {usage && usage.size > 0 && (
          <div className="absolute inset-x-0 bottom-0 h-0.5 bg-hover">
            <div
              className={`h-full ${
                usage.used / usage.size >= 0.8 ? "bg-warning" : "bg-info"
              }`}
              style={{
                width: `${Math.min(100, (usage.used / usage.size) * 100)}%`,
              }}
            />
          </div>
        )}
      </header>

      {connState === "reconnecting" && (
        <div className="bg-warning-wash px-4 py-1.5 text-center text-xs text-warning">
          {t("chat.reconnecting")}
        </div>
      )}

      {noticeText && (
        <div className="flex items-center justify-between gap-2 bg-hover px-4 py-1.5 text-xs text-dim">
          <span className="truncate">{noticeText}</span>
          <button onClick={dismissNotice} className="shrink-0 text-faint">
            ✕
          </button>
        </div>
      )}

      {planEntries && planEntries.length > 0 && (
        <PlanPanel entries={planEntries} />
      )}

      <div className="min-h-0 flex-1">
        <ChatView />
      </div>

      {drawerOpen && (
        <Drawer
          onClose={() => setDrawerOpen(false)}
          onCreate={() => setCreateOpen(true)}
          onHistory={() => setHistoryOpen(true)}
          onSettings={() => setSettingsOpen(true)}
        />
      )}
      {panelOpen && (
        <SessionPanel
          onClose={() => setPanelOpen(false)}
          onOpenFiles={() => setFilesOpen(true)}
        />
      )}
      {createOpen && (
        <ProjectCreateDialog onClose={() => setCreateOpen(false)} />
      )}
      {historyOpen && (
        <ProjectHistoryDialog onClose={() => setHistoryOpen(false)} />
      )}
      {settingsOpen && <SettingsPanel onClose={() => setSettingsOpen(false)} />}
      {(filesOpen || fileBrowserTarget != null) && (
        <FileBrowser
          onClose={() => {
            setFilesOpen(false);
            clearFileBrowserTarget();
          }}
          initialPath={fileBrowserTarget ?? undefined}
        />
      )}
      <PermissionDialog />
      <ElicitationDialog />

      {connState === "connecting" && <ConnectingOverlay />}
    </div>
  );
}
