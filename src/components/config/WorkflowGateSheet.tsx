import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Check } from "lucide-react";
import { useBackHandler } from "../../lib/backNav";
import { useAppStore } from "../../store/appStore";
import type { WorkflowGateBlock, WorkflowGateModeSetting } from "../../lib/types";

/**
 * The switch's current position, read off the gate verdict: the `override`
 * field names the position that produced it (absent = auto). "on" covers
 * both enabled override modes — an env-set onDemand reads as on too.
 */
export function gatePosition(
  gate: WorkflowGateBlock | null | undefined,
): "auto" | "on" | "off" {
  if (gate?.override === "disabled") return "off";
  return gate?.override ? "on" : "auto";
}

/**
 * Bottom sheet for the machine-level dynamic-workflow switch (bridge ≥0.53
 * PUT /settings/workflow-gate). Three positions: auto follows the remote
 * verdict; on/off are local overrides every bridge on the machine reads
 * live. "on" writes alwaysOn — the consumption-side fold makes it and
 * onDemand identical today, and an override means "make it available".
 */
export function WorkflowGateSheet({ onClose }: { onClose: () => void }) {
  const { t } = useTranslation();
  const gate = useAppStore((s) => s.configWorkflowGate);
  const setWorkflowGate = useAppStore((s) => s.setWorkflowGate);
  const [busy, setBusy] = useState(false);

  useBackHandler(() => {
    onClose();
    return true;
  });

  const position = gatePosition(gate);

  // The remote verdict is only readable while no override shadows it; an
  // unresolved fetch reports "unknown" (fail-closed) rather than "off".
  const remoteState = !gate
    ? t("zconfig.workflowGateStateUnknown")
    : gate.source === "override"
      ? t("zconfig.workflowGateStateUnknown")
      : gate.enabled
        ? t("zconfig.workflowGateStateOn")
        : gate.mode === "unknown"
          ? t("zconfig.workflowGateStateUnknown")
          : t("zconfig.workflowGateStateOff");

  const options: Array<{
    id: "auto" | "on" | "off";
    mode: WorkflowGateModeSetting;
    label: string;
    hint: string;
  }> = [
    {
      id: "auto",
      mode: "auto",
      label: t("zconfig.workflowGateAuto"),
      hint: t("zconfig.workflowGateRemote", { state: remoteState }),
    },
    {
      id: "on",
      mode: "alwaysOn",
      label: t("zconfig.workflowGateOn"),
      hint: t("zconfig.workflowGateScope"),
    },
    {
      id: "off",
      mode: "disabled",
      label: t("zconfig.workflowGateOff"),
      hint: t("zconfig.workflowGateScope"),
    },
  ];

  return (
    <div
      className="fixed inset-0 z-50 flex items-end bg-black/50"
      onClick={onClose}
    >
      <div
        className="max-h-[65%] w-full overflow-y-auto rounded-t-2xl border-t border-hairline bg-surface px-2 pb-[max(var(--safe-bottom),1rem)] pt-3"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="px-3 pb-2 text-sm font-medium text-ink">
          {t("zconfig.workflowGate")}
        </div>
        {options.map((o) => {
          const active = o.id === position;
          return (
            <button
              key={o.id}
              disabled={busy}
              onClick={() => {
                onClose();
                if (!active) {
                  setBusy(true);
                  void setWorkflowGate(o.mode).finally(() => setBusy(false));
                }
              }}
              className={`flex w-full items-center justify-between rounded-lg px-3 py-2.5 text-left text-sm ${
                active
                  ? "bg-selected font-medium text-ink"
                  : "text-dim hover:bg-hover active:bg-active"
              }`}
            >
              <span className="min-w-0 flex-1">
                <span className="block truncate">{o.label}</span>
                <span className="block truncate text-2xs font-normal text-faint">
                  {o.hint}
                </span>
              </span>
              {active && <Check className="ml-2 size-4 shrink-0 text-dim" />}
            </button>
          );
        })}
      </div>
    </div>
  );
}
