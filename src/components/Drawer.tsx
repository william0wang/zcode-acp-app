import { useMemo } from "react";
import { useTranslation } from "react-i18next";
import {
  ArrowLeft,
  FolderPlus,
  History,
  RefreshCw,
  Sliders,
  SlidersHorizontal,
} from "lucide-react";
import { useAppStore } from "../store/appStore";
import { useBackHandler } from "../lib/backNav";
import { SessionList } from "./SessionList";

// Left drawer = session switching plus the session-independent actions
// (create / resume / global settings) that used to be reachable only after
// leaving the session: they are entry-screen features, not session features,
// so there is no reason to force a trip back to the picker for them.
// Session-scoped config options and quota stay in the right-side
// SessionPanel; the ZCode configuration opens as its own full screen.
// The overlays themselves are owned by ChatScreen: this drawer unmounts on
// close, so any dialog mounted here would die with it.
export function Drawer({
  onClose,
  onCreate,
  onHistory,
  onSettings,
}: {
  onClose: () => void;
  onCreate: () => void;
  onHistory: () => void;
  onSettings: () => void;
}) {
  const { t } = useTranslation();
  const instances = useAppStore((s) => s.instances);
  const instancesLoaded = useAppStore((s) => s.instancesLoaded);
  const instanceId = useAppStore((s) => s.instanceId);
  const activeSessionId = useAppStore((s) => s.activeSessionId);
  const refreshInstances = useAppStore((s) => s.refreshInstances);
  const openSession = useAppStore((s) => s.openSession);
  const closeSession = useAppStore((s) => s.closeSession);
  const openConfig = useAppStore((s) => s.openConfig);
  const pendingRestart = useAppStore((s) => s.pendingRestart);

  useBackHandler(() => {
    onClose();
    return true;
  });

  const sessions = useMemo(
    () =>
      instances
        .flatMap((i) =>
          (i.sessions ?? []).map((s) => ({
            ...s,
            instanceId: i.id,
            workspace: i.workspace,
            origin: i.origin,
          })),
        )
        .sort((a, b) => (b.updatedAt ?? 0) - (a.updatedAt ?? 0)),
    [instances],
  );

  return (
    <div className="fixed inset-0 z-40 flex bg-black/50" onClick={onClose}>
      <aside
        className="flex h-full w-80 max-w-[85%] flex-col overflow-hidden border-r border-hairline bg-surface pt-[max(var(--safe-top),0.75rem)] text-ink"
        onClick={(e) => e.stopPropagation()}
      >
        {/* Header carries every drawer action as icons: back-to-list on the
            left, then the session-independent set the entry screen shows. No
            title (the list below is all sessions) and no footer rows — the
            icons get full-size touch targets and the list owns the rest of
            the height. Each action closes this drawer first so its overlay
            opens alone (single-overlay handoff, same as the session rows). */}
        <div className="flex shrink-0 items-center gap-1 px-2 pb-1">
          <button
            onClick={() => {
              onClose();
              closeSession();
            }}
            aria-label={t("chat.backToSessions")}
            className="flex size-9 shrink-0 items-center justify-center rounded-full text-dim hover:bg-hover active:bg-active"
          >
            <ArrowLeft className="size-4.5" />
          </button>
          <span className="flex-1" />
          <button
            onClick={() => {
              onClose();
              onHistory();
            }}
            aria-label={t("historyDialog.title")}
            className="flex size-9 shrink-0 items-center justify-center rounded-full text-dim hover:bg-hover active:bg-active"
          >
            <History className="size-4.5" />
          </button>
          <button
            onClick={() => {
              onClose();
              onCreate();
            }}
            aria-label={t("projectDialog.title")}
            className="flex size-9 shrink-0 items-center justify-center rounded-full text-dim hover:bg-hover active:bg-active"
          >
            <FolderPlus className="size-4.5" />
          </button>
          <button
            onClick={() => void refreshInstances({ probe: true })}
            aria-label={t("picker.refresh")}
            className="flex size-9 shrink-0 items-center justify-center rounded-full text-dim hover:bg-hover active:bg-active"
          >
            <RefreshCw className="size-4.5" />
          </button>
          {/* ZCode configuration — the in-session way in, mirroring the
              picker's header. The dot marks a needs-restart write waiting to
              be applied, the same signal that entry button carries. */}
          <button
            onClick={() => {
              onClose();
              openConfig(null);
            }}
            aria-label={t("zconfig.title")}
            className="relative flex size-9 shrink-0 items-center justify-center rounded-full text-dim hover:bg-hover active:bg-active"
          >
            <Sliders className="size-4.5" />
            {pendingRestart && (
              <span className="absolute top-1.5 right-1.5 size-2 rounded-full bg-warning" />
            )}
          </button>
          <button
            onClick={() => {
              onClose();
              onSettings();
            }}
            aria-label={t("panel.title")}
            className="flex size-9 shrink-0 items-center justify-center rounded-full text-dim hover:bg-hover active:bg-active"
          >
            <SlidersHorizontal className="size-4.5" />
          </button>
        </div>
        <SessionList
          sessions={sessions}
          activeKey={
            instanceId && activeSessionId
              ? `${instanceId}:${activeSessionId}`
              : null
          }
          loading={!instancesLoaded}
          onSelect={(instId, sessId) => {
            onClose();
            void openSession(instId, sessId);
          }}
          emptyHint={t("chat.noSessions")}
        />
      </aside>
    </div>
  );
}
