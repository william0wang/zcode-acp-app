import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import { RefreshCw } from "lucide-react";
import { useAppStore } from "../store/appStore";
import type { GoWindowEntry } from "../lib/types";

// Local `MM-DD HH:MM` reset stamp — the same layout the zcode-quota CLI card
// uses, so the app reads identically to the terminal output.
function fmtResetTime(ms: number): string {
  const d = new Date(ms);
  if (Number.isNaN(d.getTime())) return "";
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

// Last-refresh stamp: `HH:MM` today, `MM-DD HH:MM` otherwise.
function fmtRefreshedAt(ms: number): string {
  const d = new Date(ms);
  if (Number.isNaN(d.getTime())) return "";
  if (d.toDateString() === new Date().toDateString()) {
    const p = (n: number) => String(n).padStart(2, "0");
    return `${p(d.getHours())}:${p(d.getMinutes())}`;
  }
  return fmtResetTime(ms);
}

function capitalise(s: string): string {
  return s ? s.charAt(0).toUpperCase() + s.slice(1) : s;
}

// CLI parity (zcode-quota's roundTenth): the hub ships raw percents (Ollama
// is `fraction * 100`, unrounded), so cap the display at one decimal — `12.3`,
// not `12.34567`; integral values print without the trailing `.0`.
function fmtPercent(pct: number): string {
  return String(Math.round(pct * 10) / 10);
}

// Status lines, verbatim from the CLI card's non-success prose.
const GLM_STATUS: Record<string, string> = {
  auth_error: "🔒 Quota auth expired — re-login in the ZCode app",
  rate_limited: "⏳ Quota service busy, try again shortly",
  unavailable: "⚠ Quota info unavailable",
};
const GO_STATUS: Record<string, string> = {
  auth_error: "auth expired — refresh your opencode.ai cookie",
  unavailable: "unavailable",
};
// Mirrors the CLI card's Ollama Cloud prose (cloud.ollama.ai keys).
const OC_STATUS: Record<string, string> = {
  auth_error: "auth expired — refresh your Ollama Cloud API key",
  unavailable: "unavailable",
};

// One bar line — CLI layout: label, bar, then `NN% · reset · count` trailing.
// The per-item MCP breakdown is deliberately dropped; the totals suffice.
// CLI-parity heat color (zcode-quota's heatColor): green → yellow → red,
// piecewise-linear at 0/50/100% — Tailwind green-500 / yellow-500 / red-500.
function heatColor(pct: number): string {
  const p = Math.max(0, Math.min(100, pct));
  const lerp = (a: number, b: number, t: number) => Math.round(a + (b - a) * t);
  const [r, g, b] =
    p < 50
      ? [lerp(34, 234, p / 50), lerp(197, 179, p / 50), lerp(94, 8, p / 50)]
      : [
          lerp(234, 239, (p - 50) / 50),
          lerp(179, 68, (p - 50) / 50),
          lerp(8, 68, (p - 50) / 50),
        ];
  return `rgb(${r}, ${g}, ${b})`;
}

// One quota bar line. Exported for the plan-quota configuration screen
// (ADR-0009), which renders the same allowance with spend controls under it —
// one row implementation, so the two surfaces cannot drift apart visually.
export function QuotaRow({
  label,
  percent,
  resetMs,
  used,
  total,
}: {
  label: string;
  percent: number;
  resetMs?: number;
  used?: number;
  total?: number;
}) {
  const clamped = Math.min(100, Math.max(0, percent));
  return (
    <div className="px-4 py-1.5">
      <div className="flex items-baseline justify-between gap-3">
        <span className="shrink-0 font-mono text-xs text-dim">{label}</span>
        {/* `used / total` when the API carries counts (GLM); the Go source
            only exposes a percentage, so those rows stay on `NN%`. */}
        <span className="shrink-0 truncate text-xs tabular-nums text-faint">
          {used != null && total != null
            ? `${used} / ${total}`
            : `${fmtPercent(percent)}%`}
          {resetMs != null && ` · ${fmtResetTime(resetMs)}`}
        </span>
      </div>
      <div className="mt-1 h-1 overflow-hidden rounded-full bg-white/[0.08]">
        <div
          className="h-full rounded-full"
          style={{ width: `${clamped}%`, backgroundColor: heatColor(clamped) }}
        />
      </div>
    </div>
  );
}

export function SectionLabel({
  title,
  divided,
}: {
  title: string;
  divided?: boolean;
}) {
  return (
    <h3
      className={`px-4 pb-0.5 pt-3 text-[11px] font-medium uppercase tracking-wide text-faint ${
        divided ? "mt-2 border-t border-hairline" : ""
      }`}
    >
      {title}
    </h3>
  );
}

// Account-level quota card (hub REST /api/quota, ADR-0005). Session-
// independent: rendered by both the chat-side SessionPanel and the global
// SettingsPanel. Mounting the card (a panel just opened) refreshes the data
// so it is current without a manual tap.
export function QuotaSection() {
  const { t } = useTranslation();
  const usageStats = useAppStore((s) => s.usageStats);
  const usageStatsAt = useAppStore((s) => s.usageStatsAt);
  const quotaUnavailable = useAppStore((s) => s.quotaUnavailable);
  const refreshUsageStats = useAppStore((s) => s.refreshUsageStats);

  useEffect(() => {
    void refreshUsageStats();
  }, [refreshUsageStats]);

  if (!quotaUnavailable && !usageStats) return null;

  return (
    <>
      <div className="mt-2 flex items-center justify-between border-t border-hairline px-4 pb-0.5 pt-3">
        <h3 className="text-[11px] font-medium uppercase tracking-wide text-faint">
          {t("quota.title")}
        </h3>
        <div className="flex items-center gap-1.5">
          {usageStatsAt != null && (
            <span className="text-[10px] tabular-nums text-faint">
              {t("quota.updated")} {fmtRefreshedAt(usageStatsAt)}
            </span>
          )}
          <button
            onClick={() => void refreshUsageStats()}
            aria-label={t("quota.refresh")}
            className="flex size-7 items-center justify-center rounded-full text-faint active:bg-white/[0.06]"
          >
            <RefreshCw className="size-3.5" />
          </button>
        </div>
      </div>
      {quotaUnavailable ? (
        <p className="px-4 py-1 text-xs text-faint">{t("quota.unavailable")}</p>
      ) : (
        usageStats && (
          <>
            <SectionLabel
              title={`GLM Coding Plan${
                usageStats.glm.level
                  ? ` · ${capitalise(usageStats.glm.level)}`
                  : ""
              }`}
            />
            {usageStats.glm.kind === "success" ? (
              usageStats.glm.items?.map((it) => (
                <QuotaRow
                  key={it.key}
                  label={it.label}
                  percent={it.usedPercent}
                  resetMs={it.nextResetTime}
                  // CLI parity: only the MCP limit renders its absolute
                  // used/total; every other row stays on `NN%`.
                  used={it.key === "mcp" ? it.usedCount : undefined}
                  total={it.key === "mcp" ? it.totalCount : undefined}
                />
              ))
            ) : (
              <p className="px-4 py-1 text-xs text-faint">
                {GLM_STATUS[usageStats.glm.kind] ?? GLM_STATUS.unavailable}
              </p>
            )}

            {usageStats.opencode.kind !== "not_configured" && (
              <>
                <SectionLabel title="Opencode Go" divided />
                {usageStats.opencode.kind === "success" ? (
                  usageStats.opencode.windows?.map((w: GoWindowEntry) => (
                    <QuotaRow
                      key={w.key}
                      label={w.label}
                      percent={w.usagePercent}
                      resetMs={w.resetsAt}
                    />
                  ))
                ) : (
                  <p className="px-4 py-1 text-xs text-faint">
                    {GO_STATUS[usageStats.opencode.kind] ??
                      GO_STATUS.unavailable}
                  </p>
                )}
              </>
            )}

            {usageStats.ollama && usageStats.ollama.kind !== "not_configured" && (
              <>
                <SectionLabel title="Ollama Cloud" divided />
                {usageStats.ollama.kind === "success" ? (
                  usageStats.ollama.windows?.map((w) => (
                    <QuotaRow
                      key={w.key}
                      label={w.label}
                      percent={w.usagePercent}
                      resetMs={w.resetsAt}
                    />
                  ))
                ) : (
                  <p className="px-4 py-1 text-xs text-faint">
                    {OC_STATUS[usageStats.ollama.kind] ?? OC_STATUS.unavailable}
                  </p>
                )}
              </>
            )}

            {/* CLIProxy pool (bridge 0.66): the combined per-window rows — the
                per-account breakdown stays with the CLI, like GLM's per-model
                detail. Parity with the desktop card's section. */}
            {usageStats.cliproxy &&
              usageStats.cliproxy.kind !== "not_configured" && (
                <>
                  <SectionLabel title="CLIProxy Gemini" divided />
                  {usageStats.cliproxy.kind === "success" ? (
                    usageStats.cliproxy.combined?.map((w) => (
                      <QuotaRow
                        key={w.window}
                        label={w.window}
                        percent={w.usedPercent}
                        resetMs={w.resetsAt}
                      />
                    ))
                  ) : (
                    <p className="px-4 py-1 text-xs text-faint">
                      {GO_STATUS[usageStats.cliproxy.kind] ??
                        GO_STATUS.unavailable}
                    </p>
                  )}
                </>
              )}
          </>
        )
      )}
    </>
  );
}
