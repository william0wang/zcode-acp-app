import { useEffect, useMemo, useReducer, useRef, useState } from "react";
import type { ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { Pin, PinOff, Search, Workflow } from "lucide-react";
import { useAppStore } from "../store/appStore";
import { fmtRelative } from "../lib/time";
import { haptic } from "../lib/haptic";
import { Skeleton } from "../ui";

// Shared flat session list (drawer + entry screen): search field, per-session
// activity badge, relative time.

export interface SessionRowItem {
  sessionId: string;
  instanceId: string;
  title?: string;
  updatedAt?: number;
  workspace?: string;
  // "serve" = a remote session-create incubated bridge; only those may be
  // shut down from the app (the hub refuses editor bridges with 403).
  origin?: string;
  // Coarse "running" | "idle" from the hub heartbeat (REST) — used when no
  // instance connection exists to feed the live broadcast-based activity.
  status?: string;
  // Journal workflow runs of this session still pending/running (bridge
  // 0.57.0 workflowActivity join) — history rows only; renders the workflow
  // chip. Absent on gate-off/older bridges = no chip.
  workflowActive?: number;
}

// Session Activity (CONTEXT.md): awaiting confirmation > running > just
// finished (60s window) > idle. Broadcast-only, so idle is the default.
// `restRunning` is the heartbeat fallback (ADR-0005) for list rows seen
// without any instance connection — coarse and up to ~10s stale.
function ActivityBadge({
  sessionId,
  restRunning,
}: {
  sessionId: string;
  restRunning?: boolean;
}) {
  const { t } = useTranslation();
  const activity = useAppStore((s) => s.sessionStates[sessionId]);
  // Re-render once the just-finished window lapses (no store event for it).
  const [, force] = useReducer((x: number) => x + 1, 0);
  useEffect(() => {
    if (!activity?.finishedAt) return;
    const remain = 60_000 - (Date.now() - activity.finishedAt);
    if (remain <= 0) return;
    const id = setTimeout(force, remain + 100);
    return () => clearTimeout(id);
  }, [activity?.finishedAt]);

  if (!activity) {
    if (restRunning) {
      return (
        <span className="flex shrink-0 items-center gap-1 text-2xs font-medium text-info">
          <span className="size-1.5 animate-pulse rounded-full bg-info" />
          {t("chat.activityRunning")}
        </span>
      );
    }
    return null;
  }
  if (activity.awaitingPermission) {
    return (
      <span className="flex shrink-0 items-center gap-1 text-2xs font-medium text-warning">
        <span className="size-1.5 animate-pulse rounded-full bg-warning" />
        {t("chat.activityAwaiting")}
      </span>
    );
  }
  if (activity.running) {
    return (
      <span className="flex shrink-0 items-center gap-1 text-2xs font-medium text-info">
        <span className="size-1.5 animate-pulse rounded-full bg-info" />
        {t("chat.activityRunning")}
      </span>
    );
  }
  if (activity.finishedAt && Date.now() - activity.finishedAt < 60_000) {
    return (
      <span className="shrink-0 text-2xs font-medium text-success">
        {t("chat.activityFinished")}
      </span>
    );
  }
  return null;
}

function baseName(path: string | undefined): string {
  if (!path) return "";
  const parts = path.replace(/\/+$/, "").split("/");
  return parts[parts.length - 1] || path;
}

// Long-press (H5-friendly retire gesture, ADR-0006): a pointerdown timer
// cancelled by movement (scroll intent) or release. Spread onto the row.
// The trailing click after a fired long-press is swallowed, else releasing
// the finger would also open the session. Exported for the project
// chooser's delete gesture (same sheet pattern).
export function useLongPress(onLongPress: (() => void) | undefined, ms = 500) {
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const origin = useRef<{ x: number; y: number } | null>(null);
  const fired = useRef(false);
  const clear = () => {
    if (timer.current) clearTimeout(timer.current);
    timer.current = null;
  };
  useEffect(() => clear, []);
  return {
    onPointerDown: (e: React.PointerEvent) => {
      fired.current = false;
      if (!onLongPress) return;
      origin.current = { x: e.clientX, y: e.clientY };
      clear();
      timer.current = setTimeout(() => {
        fired.current = true;
        onLongPress();
      }, ms);
    },
    onPointerMove: (e: React.PointerEvent) => {
      const o = origin.current;
      if (!o || !timer.current) return;
      if (Math.abs(e.clientX - o.x) > 10 || Math.abs(e.clientY - o.y) > 10)
        clear();
    },
    onPointerUp: clear,
    onPointerLeave: clear,
    onPointerCancel: clear,
    onClickCapture: (e: React.MouseEvent) => {
      if (!fired.current) return;
      e.preventDefault();
      e.stopPropagation();
      fired.current = false;
    },
    // Suppress the WebView's own long-press menu alongside ours.
    onContextMenu: (e: React.MouseEvent) => e.preventDefault(),
  };
}

// (Swipe-to-pin was removed 2026-11-19, user feedback: the reveal's wide
// tinted background clashed with the row and the reveal itself was not a
// tappable target. Pinning lives in the long-press sheet below — mobile's
// context menu.)
function SessionRow({
  item,
  active,
  disabled,
  pinned,
  unread,
  onSelect,
  onLongPress,
}: {
  item: SessionRowItem;
  active: boolean;
  disabled?: boolean;
  pinned: boolean;
  unread: number;
  onSelect: () => void;
  onLongPress?: () => void;
}) {
  const { t, i18n } = useTranslation();
  const press = useLongPress(onLongPress);
  return (
    <button
      onClick={onSelect}
      disabled={disabled}
      {...press}
      className={`relative flex w-full flex-col px-4 py-3 text-left select-none [-webkit-touch-callout:none] disabled:opacity-50 ${
        active ? "bg-selected" : "hover:bg-hover active:bg-active"
      }`}
    >
        <span className="flex min-w-0 items-center gap-2">
          {pinned && <Pin className="size-3 shrink-0 text-info" />}
          <span
            className={`min-w-0 flex-1 truncate text-sm ${active ? "font-medium text-ink" : "text-dim"}`}
          >
            {item.title || t("chat.untitled")}
          </span>
          {unread > 0 && (
            <span className="flex size-4 min-w-4 shrink-0 items-center justify-center rounded-full bg-info px-1 text-2xs font-semibold text-white">
              {unread > 99 ? "99+" : unread}
            </span>
          )}
          {(item.workflowActive ?? 0) > 0 && (
            <span className="flex shrink-0 items-center gap-1 text-2xs font-medium text-chart-6">
              <Workflow className="size-3 animate-pulse" />
              {item.workflowActive}
            </span>
          )}
          <ActivityBadge
            sessionId={item.sessionId}
            restRunning={item.status === "running"}
          />
        </span>
        <span className="mt-0.5 flex items-center justify-between gap-2 text-xs text-faint">
          <span className="min-w-0 truncate">{baseName(item.workspace)}</span>
          {item.updatedAt && (
            <span className="shrink-0 tabular-nums">
              {fmtRelative(item.updatedAt, i18n.language)}
            </span>
          )}
        </span>
    </button>
  );
}

export function SessionList({
  sessions,
  activeKey,
  connecting,
  onSelect,
  emptyHint,
  loading = false,
  readOnly = false,
  onDelete,
  footer,
}: {
  sessions: SessionRowItem[];
  activeKey: string | null;
  connecting?: boolean;
  onSelect: (instanceId: string, sessionId: string) => void;
  emptyHint: string;
  /** First discovery poll still in flight: paint skeleton rows, not empty. */
  loading?: boolean;
  // History mode (ADR-0008): rows are store entries a bridge may not even
  // hold, so the full action sheet (rename/retire — both instance-scoped
  // bridge ops) is off. Pass `onDelete` to arm a delete-only sheet instead
  // (ADR-0031 tombstone — the history surface's own row action).
  readOnly?: boolean;
  onDelete?: (item: SessionRowItem) => void;
  // Rendered at the bottom of the scroll area (e.g. a "load more" button).
  footer?: ReactNode;
}) {
  const { t } = useTranslation();
  const [query, setQuery] = useState("");
  const [actionTarget, setActionTarget] = useState<SessionRowItem | null>(null);
  // Rename sub-mode of the action sheet: swaps the close/retire buttons for
  // a prefilled title input.
  const [renaming, setRenaming] = useState(false);
  const [draftTitle, setDraftTitle] = useState("");
  const closeRemoteSession = useAppStore((s) => s.closeRemoteSession);
  const renameSession = useAppStore((s) => s.renameSession);
  const shutdownInstance = useAppStore((s) => s.shutdownInstance);
  const openTerminal = useAppStore((s) => s.openTerminal);
  const pinnedSessions = useAppStore((s) => s.pinnedSessions);
  const toggleSessionPin = useAppStore((s) => s.toggleSessionPin);
  const unreadBySession = useAppStore((s) => s.unreadBySession);

  const openActions = (s: SessionRowItem) => {
    setRenaming(false);
    setActionTarget(s);
  };
  const openRename = () => {
    setDraftTitle(actionTarget?.title ?? "");
    setRenaming(true);
  };
  const submitRename = () => {
    const target = actionTarget;
    if (!target) return;
    setActionTarget(null);
    void renameSession(target.instanceId, target.sessionId, draftTitle);
  };

  // Pinned rows float to the top (requirements 0.4); within each group the
  // input order (recency) stands.
  const filtered = useMemo(() => {
    const pinned = new Set(pinnedSessions);
    const q = query.trim().toLowerCase();
    const base = q
      ? sessions.filter(
          (s) =>
            (s.title ?? "").toLowerCase().includes(q) ||
            baseName(s.workspace).toLowerCase().includes(q),
        )
      : sessions;
    return [...base].sort(
      (a, b) =>
        Number(pinned.has(b.sessionId)) - Number(pinned.has(a.sessionId)),
    );
  }, [sessions, query, pinnedSessions]);

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="px-4 pb-2">
        <div className="flex items-center gap-2 rounded-xl bg-raised px-3 py-2 ring-1 ring-inset ring-hairline">
          <Search className="size-3.5 shrink-0 text-faint" />
          <input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={t("chat.searchPlaceholder")}
            autoCapitalize="none"
            autoCorrect="off"
            className="min-w-0 flex-1 bg-transparent text-sm text-ink placeholder:text-faint focus:outline-none"
          />
        </div>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto">
        {loading && sessions.length === 0 ? (
          // First paint of the picker: the discovery poll hasn't answered
          // yet — placeholder rows shaped like SessionRow (avatar dot +
          // two-line stack), never the "no sessions" hint.
          <div aria-hidden>
            {[0.72, 0.55, 0.8, 0.6, 0.45, 0.75, 0.5, 0.65].map((w, i) => (
              <div
                key={i}
                className="flex items-center gap-3 border-b border-hairline px-4 py-3.5"
              >
                <Skeleton shape="circle" className="size-9 shrink-0" />
                <div className="flex min-w-0 flex-1 flex-col gap-2">
                  <Skeleton className="h-3.5" style={{ width: `${w * 70}%` }} />
                  <Skeleton className="h-3" style={{ width: `${w * 45}%` }} />
                </div>
              </div>
            ))}
          </div>
        ) : (
          <>
            {filtered.length === 0 && (
              <p className="mt-8 text-center text-sm text-faint">
                {sessions.length === 0 ? emptyHint : t("chat.noMatches")}
              </p>
            )}
            {filtered.map((s) => (
              <SessionRow
                key={`${s.instanceId}:${s.sessionId}`}
                item={s}
                active={activeKey === `${s.instanceId}:${s.sessionId}`}
                disabled={connecting}
                pinned={pinnedSessions.includes(s.sessionId)}
                unread={unreadBySession[s.sessionId] ?? 0}
                onSelect={() => onSelect(s.instanceId, s.sessionId)}
                onLongPress={
                  readOnly && !onDelete
                    ? undefined
                    : () => openActions(s)
                }
              />
            ))}
          </>
        )}
        {footer}
      </div>

      {actionTarget && (
        <div
          className="fixed inset-0 z-50 flex items-end justify-center bg-black/60 p-3 pb-[max(var(--safe-bottom),1rem)]"
          onClick={() => setActionTarget(null)}
        >
          <div
            className="w-full max-w-md overflow-hidden rounded-2xl border border-hairline bg-surface"
            onClick={(e) => e.stopPropagation()}
          >
            <div className="px-4 pt-4">
              <h2 className="text-sm font-semibold text-ink">
                {actionTarget.title || t("chat.untitled")}
              </h2>
              <p className="mt-1 text-xs text-faint">
                {renaming
                  ? t("chat.renameSessionHint")
                  : readOnly
                    ? t("chat.deleteSessionHint")
                    : actionTarget.origin === "serve"
                      ? t("chat.closeServeSessionHint")
                      : t("chat.closeSessionHint")}
              </p>
            </div>
            {renaming ? (
              <div className="flex flex-col gap-2 px-4 pb-4 pt-3">
                <input
                  value={draftTitle}
                  onChange={(e) => setDraftTitle(e.target.value)}
                  onKeyDown={(e) => {
                    // isComposing: Enter confirms an IME composition, not
                    // the rename. Guard mirrors the disabled Save button.
                    if (
                      e.key === "Enter" &&
                      !e.nativeEvent.isComposing &&
                      draftTitle.trim()
                    )
                      submitRename();
                  }}
                  placeholder={t("chat.renamePlaceholder")}
                  maxLength={200}
                  autoFocus
                  enterKeyHint="done"
                  className="w-full rounded-xl bg-raised px-3 py-2.5 text-sm text-ink placeholder:text-faint ring-1 ring-inset ring-hairline focus:outline-none"
                />
                <button
                  onClick={submitRename}
                  disabled={!draftTitle.trim()}
                  className="rounded-xl bg-ink px-4 py-3 text-sm font-medium text-canvas active:brightness-90 disabled:opacity-50"
                >
                  {t("chat.save")}
                </button>
                <button
                  onClick={() => setActionTarget(null)}
                  className="rounded-xl bg-raised px-4 py-3 text-sm font-medium text-ink ring-1 ring-inset ring-hairline hover:bg-hover active:bg-active"
                >
                  {t("common.cancel")}
                </button>
              </div>
            ) : (
              <div className="flex flex-col gap-2 px-4 pb-4 pt-3">
                {!readOnly && (
                  <button
                    onClick={() => {
                      haptic();
                      toggleSessionPin(actionTarget.sessionId);
                    }}
                    className="flex items-center justify-center gap-2 rounded-xl bg-raised px-4 py-3 text-sm font-medium text-ink ring-1 ring-inset ring-hairline hover:bg-hover active:bg-active"
                  >
                    {pinnedSessions.includes(actionTarget.sessionId) ? (
                      <>
                        <PinOff className="size-4 text-faint" />
                        {t("chat.unpin")}
                      </>
                    ) : (
                      <>
                        <Pin className="size-4 text-faint" />
                        {t("chat.pin")}
                      </>
                    )}
                  </button>
                )}
                {!readOnly && (
                  <button
                    onClick={openRename}
                    className="rounded-xl bg-raised px-4 py-3 text-sm font-medium text-ink ring-1 ring-inset ring-hairline hover:bg-hover active:bg-active"
                  >
                    {t("chat.renameSession")}
                  </button>
                )}
                {/* Remote Terminal (ADR-0011): wire into the desktop
                    workbench's session terminal. Live rows only — a history
                    row's session may not be held by any bridge, and the
                    workbench answers unknown_session anyway. */}
                {!readOnly && (
                  <button
                    onClick={() => {
                      const { sessionId, title } = actionTarget;
                      setActionTarget(null);
                      openTerminal(sessionId, title);
                    }}
                    className="rounded-xl bg-raised px-4 py-3 text-sm font-medium text-ink ring-1 ring-inset ring-hairline hover:bg-hover active:bg-active"
                  >
                    {t("remoteTerminal.openAction")}
                  </button>
                )}
                {/* The close is a SESSION close for every origin: the bridge
                    retires the conversation, broadcasts session_closed (the
                    desktop drops its tab and tombstones it), the hub
                    tombstones it, and a serve bridge exits by itself once its
                    LAST conversation closes. The whole-bridge kill below is
                    not a close: a headless serve bridge hosts EVERY session of
                    its project (Desktop Presence), and the kill skips all of
                    that — no retirement, no broadcast, no tombstone — so other
                    clients read it as a crash and re-incubate the bridge (the
                    desktop's "closed tab came back", 2026-10-28). Delete
                    (when offered) is a separate, always-available tombstone —
                    it hides the conversation everywhere. */}
                {!readOnly && (
                  <button
                    onClick={() => {
                      const { instanceId, sessionId } = actionTarget;
                      setActionTarget(null);
                      void closeRemoteSession(instanceId, sessionId);
                    }}
                    className="rounded-xl bg-danger-wash px-4 py-3 text-sm font-medium text-danger ring-1 ring-inset ring-danger/40 active:brightness-125"
                  >
                    {t("chat.closeSession")}
                  </button>
                )}
                {!readOnly && actionTarget.origin === "serve" && (
                  <button
                    onClick={() => {
                      const { instanceId } = actionTarget;
                      setActionTarget(null);
                      void shutdownInstance(instanceId);
                    }}
                    className="rounded-xl bg-raised px-4 py-3 text-sm font-medium text-ink ring-1 ring-inset ring-hairline hover:bg-hover active:bg-active"
                  >
                    {t("chat.shutdownInstance")}
                  </button>
                )}
                {onDelete && (
                  <button
                    onClick={() => {
                      const target = actionTarget;
                      setActionTarget(null);
                      onDelete(target);
                    }}
                    className="rounded-xl bg-danger-wash px-4 py-3 text-sm font-medium text-danger ring-1 ring-inset ring-danger/40 active:brightness-125"
                  >
                    {t("chat.deleteSession")}
                  </button>
                )}
                <button
                  onClick={() => setActionTarget(null)}
                  className="rounded-xl bg-raised px-4 py-3 text-sm font-medium text-ink ring-1 ring-inset ring-hairline hover:bg-hover active:bg-active"
                >
                  {t("common.cancel")}
                </button>
              </div>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
