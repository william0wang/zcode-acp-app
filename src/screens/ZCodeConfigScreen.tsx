import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  Archive,
  Bot,
  Box,
  ChartBar,
  ChevronRight,
  Download,
  Gauge,
  Package,
  Plug,
  RefreshCw,
  Search,
  Sparkles,
  Workflow,
} from "lucide-react";
import {
  filterConfigRows,
  normalizeSearchText,
} from "../lib/configSearch";
import { useAppStore, type ConfigSection } from "../store/appStore";
import { ConfigPageFrame } from "../components/config/ConfigPage";
import { gatePosition, WorkflowGateSheet } from "../components/config/WorkflowGateSheet";
import { ModelsPage } from "./config/ModelsPage";
import { SkillsPage } from "./config/SkillsPage";
import { McpPage } from "./config/McpPage";
import { HooksPage } from "./config/HooksPage";
import { AgentsPage } from "./config/AgentsPage";
import { QuotaPage } from "./config/QuotaPage";
import { UsagePage } from "./config/UsagePage";
import { BackupsPage } from "./config/BackupsPage";
import { AppUpdatePage } from "./config/AppUpdatePage";
import { WorkflowsPage } from "./config/WorkflowsPage";
import { SystemStatusPage } from "./config/SystemStatusPage";

// The independent configuration entry (ADR-0009): a full-screen list of the
// machine's ZCode configuration sections, each opening its own full-screen
// page. Reaches state that belongs to no session, so it works from the
// settings panel (inside a session) and from the instance picker alike.
export function ZCodeConfigScreen() {
  const { t, i18n } = useTranslation();
  const section = useAppStore((s) => s.configSection);
  const supported = useAppStore((s) => s.configSupported);
  const workflowGate = useAppStore((s) => s.configWorkflowGate);
  const loading = useAppStore((s) => s.configLoading);
  const error = useAppStore((s) => s.configError);
  const instanceId = useAppStore((s) => s.instanceId);
  const openConfig = useAppStore((s) => s.openConfig);
  const closeConfig = useAppStore((s) => s.closeConfig);
  const loadConfigAll = useAppStore((s) => s.loadConfigAll);
  const loadConfigSection = useAppStore((s) => s.loadConfigSection);
  const loadWorkflowGate = useAppStore((s) => s.loadWorkflowGate);
  const [gateSheetOpen, setGateSheetOpen] = useState(false);
  // The list search (ADR-0014 §4): fixed rows only, filtered live. Both
  // locales' labels are indexed (the other language is the alias source),
  // so the translate closure resolves per locale rather than following the
  // active one.
  const [query, setQuery] = useState("");

  // The snapshot load resolves the machine-level `supported` verdict (and
  // caches the body other screens read), so it runs once when this screen
  // opens — not per row.
  useEffect(() => {
    if (supported === null) void loadConfigAll();
  }, [supported, loadConfigAll]);

  // The workflow gate lives only in the PER-INSTANCE snapshot (the machine-
  // level one has no backend to resolve a verdict), so the entry list probes
  // the connected bridge — no bridge, no entry.
  useEffect(() => {
    if (workflowGate === null) void loadWorkflowGate();
  }, [workflowGate, instanceId, loadWorkflowGate]);

  // A section page loads its own payload on mount; the snapshot alone would
  // compress the shapes these screens edit.
  useEffect(() => {
    if (section) void loadConfigSection(section);
    // loadConfigSection is a stable store action; section is the only input.
  }, [section, loadConfigSection]);

  if (section === "models") return <ModelsPage />;
  if (section === "skills") return <SkillsPage />;
  if (section === "mcp") return <McpPage />;
  if (section === "hooks") return <HooksPage />;
  if (section === "agents") return <AgentsPage />;
  if (section === "quota") return <QuotaPage />;
  if (section === "usage") return <UsagePage />;
  if (section === "backups") return <BackupsPage />;
  if (section === "appUpdate") return <AppUpdatePage />;
  if (section === "workflows") return <WorkflowsPage />;
  // No list entry above: machine health is not ZCode configuration — the
  // settings panel owns the entry (deep entry, so Back skips this list).
  if (section === "system") return <SystemStatusPage />;

  const entries: Array<{
    id: ConfigSection;
    icon: typeof Box;
    title: string;
    hint: string;
  }> = [
    {
      id: "quota",
      icon: Gauge,
      title: t("zconfig.quota"),
      hint: t("zconfig.quotaHint"),
    },
    {
      id: "models",
      icon: Box,
      title: t("zconfig.models"),
      hint: t("zconfig.modelsHint"),
    },
    {
      id: "skills",
      icon: Sparkles,
      title: t("zconfig.skills"),
      hint: t("zconfig.skillsHint"),
    },
    {
      id: "mcp",
      icon: Plug,
      title: t("zconfig.mcp"),
      hint: t("zconfig.mcpHint"),
    },
    {
      id: "hooks",
      icon: Package,
      title: t("zconfig.hooks"),
      hint: t("zconfig.hooksHint"),
    },
    {
      id: "agents",
      icon: Bot,
      title: t("zconfig.agents"),
      hint: t("zconfig.agentsHint"),
    },
    {
      id: "usage",
      icon: ChartBar,
      title: t("zconfig.usage"),
      hint: t("zconfig.usageHint"),
    },
    {
      id: "backups",
      icon: Archive,
      title: t("zconfig.backups"),
      hint: t("zconfig.backupsHint"),
    },
    {
      id: "appUpdate",
      icon: Download,
      title: t("zconfig.appUpdate"),
      hint: t("zconfig.appUpdateHint"),
    },
    // Hidden unless the gate verdict says the feature is on (server ADR-0029:
    // availability is the remote verdict; the settings UI only reads it).
    // `workflowGate` (per-instance probe) is the only honest source — the
    // machine-level snapshot's workflow block is always `{available:false}`.
    ...(workflowGate?.enabled === true
      ? [
          {
            id: "workflows" as ConfigSection,
            icon: Workflow,
            title: t("zconfig.workflows"),
            hint: t("zconfig.workflowsHint"),
          },
        ]
      : []),
  ];

  const gateLabelKey = {
    auto: "zconfig.workflowGateAuto",
    on: "zconfig.workflowGateOn",
    off: "zconfig.workflowGateOff",
  }[gatePosition(workflowGate)];

  // Filter while a query runs: the matcher owns the semantics, the screen
  // only hides non-matching rows (the gated workflows entry stays out here —
  // it was never in `entries` when the gate is off).
  const searching = normalizeSearchText(query).length > 0;
  const matchedIds = new Set(
    filterConfigRows(query, (key, locale) => i18n.t(key, { lng: locale })).map(
      (row) => row.id,
    ),
  );
  const visibleEntries = entries.filter((e) => !searching || matchedIds.has(e.id));
  const gateVisible = !searching || matchedIds.has("gate");

  return (
    <ConfigPageFrame
      title={t("zconfig.title")}
      onBack={closeConfig}
      onRefresh={() => void loadConfigAll()}
      refreshing={loading}
      unsupported={supported === false}
      error={error}
    >
      {/* The machine these screens configure, so a multi-hub user can tell
          which one they are about to change. */}
      <p className="px-4 pb-1 pt-1 text-2xs text-faint">
        {t("zconfig.scope")}
      </p>
      <div className="mx-4 mb-1 flex items-center gap-2 rounded-xl bg-raised px-3 ring-1 ring-inset ring-hairline focus-within:ring-2 focus-within:ring-info/50">
        <Search className="size-4 shrink-0 text-faint" />
        <input
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder={t("settings.searchPlaceholder")}
          autoCapitalize="none"
          autoCorrect="off"
          spellCheck={false}
          className="min-w-0 flex-1 bg-transparent py-2.5 text-sm text-ink placeholder:text-faint focus:outline-none"
        />
      </div>
      <div className="mt-1">
        {visibleEntries.map(({ id, icon: Icon, title, hint }) => (
          <button
            key={id}
            onClick={() => openConfig(id)}
            className="flex w-full items-center gap-3 px-4 py-3 text-left hover:bg-hover active:bg-active"
          >
            <Icon className="size-4.5 shrink-0 text-faint" />
            <span className="min-w-0 flex-1">
              <span className="block text-sm text-ink">{title}</span>
              <span className="block truncate text-2xs text-faint">
                {hint}
              </span>
            </span>
            <ChevronRight className="size-4 shrink-0 text-faint" />
          </button>
        ))}
        {/* Machine-level workflow switch (bridge ≥0.53): visible with no
            query — with the remote verdict off this override is the only
            way in, so unlike the gated workflows entry above it must not
            hide; while searching it rides the same filter as every row. On
            an older bridge the write 404s into the standard failure toast. */}
        {gateVisible && (
        <button
          onClick={() => setGateSheetOpen(true)}
          className="flex w-full items-center gap-3 px-4 py-3 text-left hover:bg-hover active:bg-active"
        >
          <Workflow className="size-4.5 shrink-0 text-faint" />
          <span className="min-w-0 flex-1">
            <span className="block text-sm text-ink">
              {t("zconfig.workflowGate")}
            </span>
            <span className="block truncate text-2xs text-faint">
              {t(gateLabelKey)}
            </span>
          </span>
          <ChevronRight className="size-4 shrink-0 text-faint" />
        </button>
        )}
        {searching && visibleEntries.length === 0 && !gateVisible && (
          <p className="px-4 py-8 text-center text-xs text-faint">
            {t("settings.searchEmpty")}
          </p>
        )}
      </div>

      {gateSheetOpen && (
        <WorkflowGateSheet onClose={() => setGateSheetOpen(false)} />
      )}

      {/* A needs-restart write landed somewhere in these screens and has not
          been applied yet — say so at the top level, where the user returns. */}
      <RestartNote />
    </ConfigPageFrame>
  );
}

/**
 * The restart affordance, shown only when there is something to apply.
 *
 * Restarting cancels in-flight turns, so it is never automatic: the user taps
 * it, and the button is absent (not disabled) when nothing is pending.
 *
 * The restart is per-instance — the hub's own settings mount has no backend —
 * so with no session attached there is no bridge to name and the affordance
 * says so instead of failing silently on tap.
 */
function RestartNote() {
  const { t } = useTranslation();
  const pendingRestart = useAppStore((s) => s.pendingRestart);
  const instanceId = useAppStore((s) => s.instanceId);
  const restartConfigBackend = useAppStore((s) => s.restartConfigBackend);
  if (!pendingRestart) return null;
  return (
    <div className="mx-4 mt-4 rounded-xl bg-surface px-4 py-3 ring-1 ring-hairline">
      <p className="text-xs text-dim">{t("zconfig.restartPending")}</p>
      <p className="pt-1 text-2xs text-faint">
        {t("zconfig.restartWarning")}
      </p>
      {instanceId ? (
        <button
          onClick={() => void restartConfigBackend()}
          className="mt-2 flex w-full items-center justify-center gap-2 rounded-lg bg-selected px-3 py-2 text-xs font-medium text-ink hover:bg-hover active:bg-active"
        >
          <RefreshCw className="size-3.5" />
          {t("zconfig.restart")}
        </button>
      ) : (
        <p className="pt-2 text-2xs text-warning">
          {t("zconfig.restartNoInstance")}
        </p>
      )}
    </div>
  );
}

// Re-exported so the mounting screens can name the type without reaching into
// the store module for it.
export type { ConfigSection };
