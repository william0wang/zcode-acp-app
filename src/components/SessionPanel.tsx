import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  ChevronRight,
  FolderOpen,
  SquareTerminal,
  Workflow,
} from "lucide-react";
import { useTerminalLive } from "../lib/terminalSession";
import { useAppStore } from "../store/appStore";
import { ConfigSheet } from "./ConfigSheet";
import { PanelShell } from "./SidePanel";
import { QuotaSection, SectionLabel } from "./QuotaSection";
import { bareModelIdFromConfigValue } from "../lib/modelValue";

// Session-scoped right panel shown in chat: the session's tools (terminal,
// files), config options (model / mode / thought) plus the shared account
// quota card. Global settings live in SettingsPanel.
export function SessionPanel({
  onClose,
  onOpenFiles,
}: {
  onClose: () => void;
  onOpenFiles: () => void;
}) {
  const { t } = useTranslation();
  const activeSessionId = useAppStore((s) => s.activeSessionId);
  const sessionTitle = useAppStore(
    (s) =>
      s.instances
        .find((i) => i.id === s.instanceId)
        ?.sessions?.find((x) => x.sessionId === s.activeSessionId)?.title,
  );
  const fsCapable = useAppStore((s) => s.fsCapable);
  const openTerminal = useAppStore((s) => s.openTerminal);
  const terminalLive = useTerminalLive();
  const configOptions = useAppStore((s) => s.configOptions);
  const usageStats = useAppStore((s) => s.usageStats);
  const quotaUnavailable = useAppStore((s) => s.quotaUnavailable);
  const workflowGate = useAppStore((s) => s.configWorkflowGate);
  const loadWorkflowGate = useAppStore((s) => s.loadWorkflowGate);
  const openConfig = useAppStore((s) => s.openConfig);
  const [configOpen, setConfigOpen] = useState<string | null>(null);

  // The workflows entry's visibility rides the per-instance gate verdict (the
  // same fail-closed rule the config screen applies), so the panel probes it
  // while it is still unknown.
  useEffect(() => {
    if (workflowGate === null) void loadWorkflowGate();
  }, [workflowGate, loadWorkflowGate]);

  const workflowEnabled = workflowGate?.enabled === true;

  return (
    <PanelShell title={t("panel.session")} onClose={onClose}>
      {/* The header keeps a single button, so the session's tools enter from
          here. Both open full-screen overlays above the chat, hence the panel
          closes first. */}
      {(activeSessionId != null || fsCapable) && (
        <>
          <SectionLabel title={t("panel.tools")} />
          {/* Remote Terminal (ADR-0011) is session-bound: this always
              re-enters THIS session's one terminal. The dot mirrors the
              header badge — amber pulse while a turn runs, blue while the
              socket is up. */}
          {activeSessionId != null && (
            <button
              onClick={() => {
                onClose();
                openTerminal(activeSessionId, sessionTitle);
              }}
              className="flex w-full items-center justify-between gap-3 px-4 py-2.5 text-left active:bg-white/[0.05]"
            >
              <span className="flex items-center gap-2 text-sm text-dim">
                <SquareTerminal className="size-3.5 shrink-0 text-faint" />
                {t("remoteTerminal.openAction")}
              </span>
              <span className="flex shrink-0 items-center gap-2">
                {terminalLive?.sessionId === activeSessionId &&
                  (terminalLive.terminal?.turn?.state === "running" ? (
                    <span className="size-1.5 animate-pulse rounded-full bg-amber-400" />
                  ) : terminalLive.conn === "open" ? (
                    <span className="size-1.5 rounded-full bg-blue-400/70" />
                  ) : null)}
                <ChevronRight className="size-4 shrink-0 text-faint" />
              </span>
            </button>
          )}
          {fsCapable && (
            <button
              onClick={() => {
                onClose();
                onOpenFiles();
              }}
              className="flex w-full items-center justify-between gap-3 px-4 py-2.5 text-left active:bg-white/[0.05]"
            >
              <span className="flex items-center gap-2 text-sm text-dim">
                <FolderOpen className="size-3.5 shrink-0 text-faint" />
                {t("files.title")}
              </span>
              <ChevronRight className="size-4 shrink-0 text-faint" />
            </button>
          )}
        </>
      )}

      {/* Session-scoped config — meaningless until a session is attached. */}
      {activeSessionId != null && configOptions.length > 0 && (
        <>
          <SectionLabel title={t("config.title")} />
          {configOptions.map((opt) => {
            const current = opt.options?.find(
              (v) => v.value === opt.currentValue,
            );
            return (
              <button
                key={opt.id}
                onClick={() => setConfigOpen(opt.id)}
                className="flex items-center justify-between gap-3 px-4 py-2.5 text-left active:bg-white/[0.05]"
              >
                <span className="shrink-0 text-sm text-dim">
                  {t(`config.${opt.id}`, { defaultValue: opt.name ?? opt.id })}
                </span>
                <span className="truncate text-xs text-faint">
                  {current?.name ??
                    (opt.currentValue
                      ? bareModelIdFromConfigValue(opt.currentValue)
                      : "—")}
                </span>
              </button>
            );
          })}
        </>
      )}

      {workflowEnabled && activeSessionId != null && (
        <>
          <SectionLabel title={t("panel.workflows")} />
          {/* The single workflow entry: launching, live runs, history and
              cleanup all live INSIDE the management page — the panel keeps
              nothing workflow-specific of its own. */}
          <button
            onClick={() => openConfig("workflows")}
            className="flex w-full items-center justify-between gap-3 px-4 py-2.5 text-left active:bg-white/[0.05]"
          >
            <span className="flex items-center gap-2 text-sm text-dim">
              <Workflow className="size-3.5 shrink-0 text-faint" />
              {t("panel.workflowManage")}
            </span>
            <ChevronRight className="size-4 shrink-0 text-faint" />
          </button>
        </>
      )}

      <QuotaSection />

      {activeSessionId == null && !usageStats && !quotaUnavailable && (
        <p className="px-4 py-2 text-xs text-faint">{t("chat.noSession")}</p>
      )}

      {configOpen && (
        <ConfigSheet
          optionId={configOpen}
          onClose={() => setConfigOpen(null)}
        />
      )}
    </PanelShell>
  );
}
