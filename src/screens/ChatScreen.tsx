import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  Check,
  ChevronDown,
  ChevronRight,
  Circle,
  FolderOpen,
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

function PlanPanel({ entries }: { entries: PlanEntry[] }) {
  const { t } = useTranslation();
  const done = entries.filter((e) => e.status === "completed").length;
  return (
    <details className="border-b border-hairline bg-surface/60 px-4 py-2 text-xs text-dim">
      <summary className="group flex cursor-pointer select-none items-center gap-1.5 text-dim [&::-webkit-details-marker]:hidden">
        <ChevronDown className="size-3.5 shrink-0 text-faint transition-transform group-open:rotate-180" />
        <span>
          {t("chat.plan")} · {done}/{entries.length}
        </span>
      </summary>
      <ul className="mt-1.5 flex flex-col gap-1 pb-1">
        {entries.map((e, i) => (
          <li key={i} className="flex items-start gap-1.5">
            {e.status === "completed" ? (
              <Check className="mt-0.5 size-3.5 shrink-0 text-emerald-400" />
            ) : e.status === "active" ? (
              <ChevronRight className="mt-0.5 size-3.5 shrink-0 text-blue-400" />
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
    </details>
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
  const fsCapable = useAppStore((s) => s.fsCapable);
  const dismissNotice = useAppStore((s) => s.dismissNotice);
  const notify = useAppStore((s) => s.notify);
  const configOpen = useAppStore((s) => s.configOpen);
  const closeSession = useAppStore((s) => s.closeSession);

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
      ? "bg-emerald-500"
      : connState === "reconnecting"
        ? "bg-amber-500 animate-pulse"
        : "bg-zinc-500";

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
          className="flex size-9 shrink-0 items-center justify-center rounded-full text-dim active:bg-white/[0.06]"
        >
          <Menu className="size-5" />
        </button>
        <div className="min-w-0 flex-1">
          <div className="truncate text-[15px] font-medium">{title}</div>
          {workspace && (
            <div className="flex items-baseline text-[11px] text-faint">
              {/* Stats live in the title bar, TUI style: context `17.5%/1.0M`,
                  bare cache percent, `tok/s`, then the quota dock line — the
                  hub's preformatted string (ADR-0021), byte-identical to the
                  TUI's bottom dock, refreshed at connect and each turn end.
                  The dock and tps segments are header-container gated; the
                  workspace path truncates so the numbers stay visible. */}
              <span className="min-w-0 truncate">{workspace}</span>
              {usage != null && usage.size > 0 && (
                <span className="shrink-0 whitespace-nowrap">
                  {" · "}
                  {fmtContext(usage.used, usage.size)}
                </span>
              )}
              {cacheHit != null && (
                <span className="shrink-0 whitespace-nowrap">
                  · {cacheHit}%
                </span>
              )}
              {turnStats && (
                <span className="hidden shrink-0 whitespace-nowrap @min-[520px]:inline">
                  · {turnStats.tps} tok/s
                </span>
              )}
              {quotaDock && (
                <span className="ml-auto hidden shrink-0 whitespace-nowrap pl-2 @min-[640px]:inline">
                  {quotaDock}
                </span>
              )}
            </div>
          )}
        </div>
        <span
          className={`mr-1.5 size-2 shrink-0 rounded-full ${statusDot}`}
          aria-label={connState}
        />
        {fsCapable && (
          <button
            onClick={() => setFilesOpen(true)}
            aria-label={t("files.title")}
            className="mr-1 flex size-9 shrink-0 items-center justify-center rounded-full text-dim active:bg-white/[0.06]"
          >
            <FolderOpen className="size-5" />
          </button>
        )}
        <button
          onClick={() => setPanelOpen(true)}
          aria-label={t("panel.session")}
          className="flex size-9 shrink-0 items-center justify-center rounded-full text-dim active:bg-white/[0.06]"
        >
          <SlidersHorizontal className="size-5" />
        </button>
        {usage && usage.size > 0 && (
          <div className="absolute inset-x-0 bottom-0 h-0.5 bg-white/[0.06]">
            <div
              className="h-full bg-blue-500"
              style={{
                width: `${Math.min(100, (usage.used / usage.size) * 100)}%`,
              }}
            />
          </div>
        )}
      </header>

      {connState === "reconnecting" && (
        <div className="bg-amber-950 px-4 py-1.5 text-center text-xs text-amber-300">
          {t("chat.reconnecting")}
        </div>
      )}

      {noticeText && (
        <div className="flex items-center justify-between gap-2 bg-white/[0.05] px-4 py-1.5 text-xs text-dim">
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
      {panelOpen && <SessionPanel onClose={() => setPanelOpen(false)} />}
      {createOpen && (
        <ProjectCreateDialog onClose={() => setCreateOpen(false)} />
      )}
      {historyOpen && (
        <ProjectHistoryDialog onClose={() => setHistoryOpen(false)} />
      )}
      {settingsOpen && <SettingsPanel onClose={() => setSettingsOpen(false)} />}
      {filesOpen && <FileBrowser onClose={() => setFilesOpen(false)} />}
      <PermissionDialog />
      <ElicitationDialog />

      {connState === "connecting" && (
        <div className="absolute inset-0 z-30 flex flex-col items-center justify-center gap-3 bg-canvas/90">
          <Spinner className="size-8" />
          <div className="text-sm text-dim">{t("chat.connecting")}</div>
        </div>
      )}
    </div>
  );
}
