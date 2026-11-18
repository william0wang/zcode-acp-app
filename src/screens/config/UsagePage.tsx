import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { useAppStore } from "../../store/appStore";
import type { PlatformUsageHeatmapCell } from "../../lib/types";
import {
  ConfigBlock,
  ConfigEmpty,
  ConfigPageFrame,
  ConfigRow,
  fmtTokens,
} from "../../components/config/ConfigPage";

// Usage statistics (ADR-0009), in two tabs mirroring the desktop app's Usage
// page: Platform (the account-level monitor data — activity heatmap, credits,
// per-model/per-tool usage, bridge GET /settings/usage-platform) and Local
// (what this machine's agent database recorded, ADR-0027). The two answer
// different questions, so they stay visually distinct but one entry deep.
const RANGES = ["7d", "30d", "all"] as const;
type Range = (typeof RANGES)[number];

const PLATFORM_RANGES = ["today", "7d", "30d"] as const;
type PlatformRange = (typeof PLATFORM_RANGES)[number];

type UsageTab = "platform" | "local";

export function UsagePage() {
  const { t } = useTranslation();
  const supported = useAppStore((s) => s.configSupported);
  // The platform tab is the default: it is the account-level view the desktop
  // app leads with, and the local tab is the power-user detail.
  const [tab, setTab] = useState<UsageTab>("platform");
  const platform = useAppStore((s) => s.configPlatformUsage);
  const platformSupported = useAppStore((s) => s.configPlatformUsageSupported);
  const local = useAppStore((s) => s.configUsage);
  const hasLoaded =
    tab === "platform"
      ? platform !== null || !platformSupported
      : local !== null;

  return (
    <ConfigPageFrame
      title={t("zconfig.usage")}
      onBack={() => useAppStore.getState().openConfig(null)}
      unsupported={supported === false}
      hasLoaded={hasLoaded}
    >
      <div className="flex gap-2 px-4 py-3">
        {(["platform", "local"] as const).map((id) => (
          <button
            key={id}
            onClick={() => setTab(id)}
            className={`flex-1 rounded-lg px-3 py-1.5 text-xs transition ${
              id === tab
                ? "bg-active font-medium text-ink"
                : "text-faint hover:bg-hover active:bg-active"
            }`}
          >
            {t(
              id === "platform"
                ? "zconfig.usageTabPlatform"
                : "zconfig.usageTabLocal",
            )}
          </button>
        ))}
      </div>

      {tab === "platform" ? <PlatformUsageTab /> : <LocalUsageTab />}
    </ConfigPageFrame>
  );
}

// ---- platform tab -------------------------------------------------------------

function PlatformUsageTab() {
  const { t } = useTranslation();
  const usage = useAppStore((s) => s.configPlatformUsage);
  const supported = useAppStore((s) => s.configPlatformUsageSupported);
  const loadConfigSection = useAppStore((s) => s.loadConfigSection);
  // The store's record of the last ask is the authority (same reasoning as the
  // local tab): a late answer for a range nobody selected anymore is dropped
  // there, so a local mirror here could highlight the wrong pill.
  const storeRange = useAppStore((s) => s.configPlatformUsageRange);
  const range = (PLATFORM_RANGES as readonly string[]).includes(storeRange)
    ? (storeRange as PlatformRange)
    : "7d";

  useEffect(() => {
    void loadConfigSection("usagePlatform");
  }, [loadConfigSection]);

  // The range is a control over the upstream query, not a client-side filter
  // (the 365-day activity block ignores it; the credits/model/tool blocks do
  // not), so a tap refetches.
  async function pickRange(next: PlatformRange) {
    await loadConfigSection("usagePlatform", { range: next });
  }

  const models = usage?.models ?? [];
  const modelTotal = models.reduce((sum, m) => sum + (m.totalTokens ?? 0), 0);

  return (
    <>
      <div className="flex gap-2 px-4 pb-1">
        {PLATFORM_RANGES.map((r) => (
          <button
            key={r}
            onClick={() => void pickRange(r)}
            className={`flex-1 rounded-lg px-3 py-1.5 text-xs transition ${
              r === range
                ? "bg-active font-medium text-ink"
                : "text-faint hover:bg-hover active:bg-active"
            }`}
          >
            {t(r === "today" ? "zconfig.rangeToday" : `zconfig.range${r}`)}
          </button>
        ))}
      </div>

      {!supported ? (
        // The bridge predates the route. This is deliberately NOT the global
        // "unsupported hub" state: every other section still works.
        <ConfigEmpty text={t("zconfig.usagePlatformOldBridge")} />
      ) : !usage ? null : usage.kind !== "success" ? (
        <ConfigEmpty
          text={
            usage.kind === "auth_error"
              ? t("zconfig.usagePlatformAuth")
              : usage.kind === "rate_limited"
                ? t("zconfig.usagePlatformBusy")
                : t("quota.unavailable")
          }
        />
      ) : (
        <>
          {/* ---- activity (fixed trailing 365 days, range-independent) ---- */}
          {usage.activity && (
            <>
              <ConfigBlock>
                <ConfigRow
                  label={t("zconfig.usageTotal")}
                  value={fmtTokens(usage.activity.summary.totalTokens)}
                />
                <ConfigRow
                  label={t("zconfig.usagePeakDay")}
                  value={fmtTokens(usage.activity.summary.peakDailyTokens)}
                  hint={
                    usage.activity.summary.peakDailyTokensDate?.slice(5) ??
                    undefined
                  }
                />
                <ConfigRow
                  label={t("zconfig.usageDuration")}
                  value={fmtDuration(
                    usage.activity.summary.totalUsageDurationMs,
                    t,
                  )}
                />
                <ConfigRow
                  label={t("zconfig.usageStreak")}
                  value={t("zconfig.usageStreakValue", {
                    current: usage.activity.summary.currentStreakDays,
                    longest: usage.activity.summary.longestStreakDays,
                  })}
                />
              </ConfigBlock>

              {usage.activity.heatmap.weeks.length > 0 && (
                <ConfigBlock title={t("zconfig.usageActivity")} divided>
                  <Heatmap weeks={usage.activity.heatmap.weeks} />
                  {/* The caption dates the grid: a sparse-looking year with a
                      recent start beats an unexplained wall of squares. */}
                  <p className="px-4 pb-3 pt-1 text-2xs text-faint">
                    {usage.activity.heatmap.startDate} ~{" "}
                    {usage.activity.heatmap.endDate}
                  </p>
                </ConfigBlock>
              )}
            </>
          )}

          {/* ---- credits summary (model usage-detail; the tool variant of
                  these metrics answers the same question less usefully) ---- */}
          {usage.detail?.model && (
            <ConfigBlock title={t("zconfig.usageCredits")} divided>
              <MetricRow
                label={t("zconfig.usageCacheHit")}
                value={`${usage.detail.model.cacheHitRate?.toFixed(1) ?? "–"}%`}
                trend={usage.detail.model.cacheHitRateTrend}
              />
              <MetricRow
                label={t("zconfig.usageTotalCredits")}
                value={fmtTokens(usage.detail.model.totalCredits)}
                trend={usage.detail.model.totalCreditsTrend}
              />
              <MetricRow
                label={t("zconfig.usageAvgCredits")}
                value={fmtTokens(usage.detail.model.averageDailyCredits)}
                trend={usage.detail.model.averageDailyCreditsTrend}
              />
            </ConfigBlock>
          )}

          {/* ---- per-day (or per-hour today) aggregate ---- */}
          {usage.series && usage.series.totals.length > 0 && (
            <ConfigBlock
              title={
                usage.series.granularity === "hour"
                  ? t("zconfig.usageByHour")
                  : t("zconfig.usageByDay")
              }
              divided
            >
              <SeriesBars totals={usage.series.totals} />
              <p className="px-4 pb-3 pt-1.5 text-2xs text-faint">
                {usage.series.xTime[0]} ~ {usage.series.xTime.at(-1)}
              </p>
            </ConfigBlock>
          )}

          {/* ---- by model ---- */}
          <ConfigBlock title={t("zconfig.usageByModel")} divided>
            {models.length === 0 ? (
              <ConfigEmpty text={t("zconfig.usageNoData")} />
            ) : (
              models.map((m) => (
                <div key={m.name} className="px-4 py-2.5">
                  <div className="flex items-baseline justify-between gap-3">
                    <span className="min-w-0 truncate text-sm text-dim">
                      {m.name}
                    </span>
                    <span className="shrink-0 text-sm tabular-nums text-ink">
                      {fmtTokens(m.totalTokens)}
                      {m.totalCredits != null && (
                        <span className="pl-1.5 text-faint">
                          · {fmtTokens(m.totalCredits)} cr
                        </span>
                      )}
                    </span>
                  </div>
                  {/* Share of the window's tokens — models compared against
                      each other, like the local tab's bar. */}
                  <div className="mt-1 h-1 overflow-hidden rounded-full bg-selected">
                    <div
                      className="h-full rounded-full bg-faint"
                      style={{
                        width: `${
                          modelTotal > 0
                            ? Math.round((m.totalTokens / modelTotal) * 100)
                            : 0
                        }%`,
                      }}
                    />
                  </div>
                </div>
              ))
            )}
          </ConfigBlock>

          {/* ---- by tool ---- */}
          {usage.tools && usage.tools.length > 0 && (
            <ConfigBlock title={t("zconfig.usageByTool")} divided>
              {usage.tools.map((tool) => (
                <ConfigRow
                  key={tool.name}
                  label={tool.name}
                  value={
                    tool.totalCredits != null
                      ? `${tool.totalUsageCount} · ${fmtTokens(tool.totalCredits)} cr`
                      : String(tool.totalUsageCount)
                  }
                />
              ))}
            </ConfigBlock>
          )}
        </>
      )}
    </>
  );
}

/** The 365-day activity grid: one column per (Sunday-start) week. */
function Heatmap({
  weeks,
}: {
  weeks: Array<{ days: Array<PlatformUsageHeatmapCell | null> }>;
}) {
  // Level 0 (a recorded zero) and a missing day both read as "nothing", but
  // only level 0 earns a square — missing days keep the week's shape.
  const LEVEL_BG = [
    "bg-hover",
    "bg-success/25",
    "bg-success/45",
    "bg-success/70",
    "bg-success/90",
  ] as const;
  return (
    <div className="overflow-x-auto px-4 pt-3">
      <div className="flex w-max gap-[2px]">
        {weeks.map((week, i) => (
          <div key={i} className="flex flex-col gap-[2px]">
            {week.days.map((day, j) => (
              <div
                key={j}
                title={
                  day ? `${day.date} · ${fmtTokens(day.tokens)}` : undefined
                }
                className={`size-[9px] rounded-[2px] ${
                  day
                    ? LEVEL_BG[Math.min(4, Math.max(0, day.level))]
                    : "bg-transparent"
                }`}
              />
            ))}
          </div>
        ))}
      </div>
    </div>
  );
}

/** One credits metric with its period-over-period trend. */
function MetricRow({
  label,
  value,
  trend,
}: {
  label: string;
  value: string;
  trend: number | null;
}) {
  return (
    <div className="px-4 py-2.5">
      <div className="flex items-baseline justify-between gap-3">
        <span className="text-sm text-dim">{label}</span>
        <span className="shrink-0 text-sm tabular-nums text-ink">
          {value}
          {trend != null && Number.isFinite(trend) && trend !== 0 && (
            <span
              className={`pl-1.5 text-2xs ${
                trend > 0 ? "text-success/80" : "text-info"
              }`}
            >
              {trend > 0 ? "↑" : "↓"} {Math.abs(trend).toFixed(1)}%
            </span>
          )}
        </span>
      </div>
    </div>
  );
}

/** Compact bars over the selected range — max-height relative, not to scale. */
function SeriesBars({ totals }: { totals: number[] }) {
  const max = Math.max(...totals, 0);
  return (
    <div className="flex h-16 items-end gap-[3px] px-4 pt-3">
      {totals.map((value, i) => (
        <div
          key={i}
          className="min-w-[3px] flex-1 rounded-sm bg-faint"
          style={{
            height: `${value > 0 && max > 0 ? Math.max(6, (value / max) * 100) : 2}%`,
          }}
        />
      ))}
    </div>
  );
}

/** `1h 23m` / `23m` — compact on purpose; the row is dense already. */
function fmtDuration(
  ms: number,
  t: (key: string, opts?: Record<string, unknown>) => string,
): string {
  const hours = Math.floor(ms / 3_600_000);
  const minutes = Math.round((ms % 3_600_000) / 60_000);
  if (hours <= 0) return t("zconfig.usageDurationMinutes", { m: minutes });
  return t("zconfig.usageDurationValue", { h: hours, m: minutes });
}

// ---- local tab ------------------------------------------------------------------

function LocalUsageTab() {
  const { t } = useTranslation();
  const usage = useAppStore((s) => s.configUsage);
  const loadConfigSection = useAppStore((s) => s.loadConfigSection);
  const storeRange = useAppStore((s) => s.configUsageRange);
  const range = (RANGES as readonly string[]).includes(storeRange)
    ? (storeRange as Range)
    : "7d";

  useEffect(() => {
    void loadConfigSection("usage");
  }, [loadConfigSection]);

  // The range is a client-side control over the same endpoint; switching
  // refetches rather than filtering a wider window, so the numbers always
  // match what the bridge computed for that range.
  async function pickRange(next: Range) {
    await loadConfigSection("usage", { range: next });
  }

  const models = usage?.models ?? [];
  const daily = usage?.daily ?? [];

  return (
    <>
      <div className="flex gap-2 px-4 pb-1">
        {RANGES.map((r) => (
          <button
            key={r}
            onClick={() => void pickRange(r)}
            className={`flex-1 rounded-lg px-3 py-1.5 text-xs transition ${
              r === range
                ? "bg-active font-medium text-ink"
                : "text-faint hover:bg-hover active:bg-active"
            }`}
          >
            {t(`zconfig.range${r === "all" ? "All" : r}`)}
          </button>
        ))}
      </div>

      {usage && !usage.available ? (
        // No agent database is a normal state on a machine that never ran one,
        // not an error — say so plainly rather than showing an empty chart.
        <ConfigEmpty text={t("zconfig.usageNoData")} />
      ) : (
        <>
          <ConfigBlock>
            <ConfigRow
              label={t("zconfig.usageTotal")}
              value={fmtTokens(usage?.summary.totalTokens ?? 0)}
            />
            <ConfigRow
              label={t("zconfig.usageRequests")}
              value={String(usage?.summary.requestCount ?? 0)}
            />
          </ConfigBlock>

          <ConfigBlock title={t("zconfig.usageByModel")} divided>
            {models.length === 0 ? (
              <ConfigEmpty text={t("zconfig.usageNoData")} />
            ) : (
              models.map((m) => (
                <div key={m.modelId ?? "unknown"} className="px-4 py-2.5">
                  <div className="flex items-baseline justify-between gap-3">
                    <span className="min-w-0 truncate text-sm text-dim">
                      {/* The db column is nullable (older rows predate the
                          column), so a null id renders as a placeholder rather
                          than a blank line the user cannot attribute. */}
                      {m.modelId ?? t("zconfig.usageUnknownModel")}
                    </span>
                    <span className="shrink-0 text-sm tabular-nums text-ink">
                      {fmtTokens(m.totalTokens)}
                      {m.share != null && (
                        <span className="pl-1.5 text-faint">
                          {(m.share * 100).toFixed(0)}%
                        </span>
                      )}
                    </span>
                  </div>
                  {/* The share bar: same heat idea as the quota card, but here
                      it compares models against each other, not against a
                      limit — so it is a flat neutral fill. */}
                  <div className="mt-1 h-1 overflow-hidden rounded-full bg-selected">
                    <div
                      className="h-full rounded-full bg-faint"
                      style={{ width: `${Math.round((m.share ?? 0) * 100)}%` }}
                    />
                  </div>
                </div>
              ))
            )}
          </ConfigBlock>

          {daily.length > 0 && (
            <ConfigBlock title={t("zconfig.usageByDay")} divided>
              {daily.map((d) => (
                <div
                  key={d.date}
                  className="flex items-baseline justify-between gap-3 px-4 py-2"
                >
                  <span className="shrink-0 font-mono text-xs text-faint">
                    {d.date}
                  </span>
                  <span className="min-w-0 truncate text-right text-xs text-dim">
                    {d.models
                      .map((m) => `${m.modelId} ${fmtTokens(m.totalTokens)}`)
                      .join(" · ")}
                  </span>
                </div>
              ))}
            </ConfigBlock>
          )}
        </>
      )}
    </>
  );
}
