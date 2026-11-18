import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { ChevronRight, Plus, X } from "lucide-react";
import { useAppStore } from "../../store/appStore";
import {
  ConfigBlock,
  ConfigEmpty,
  ConfigPageFrame,
  fmtStamp,
  fmtTokens,
} from "../../components/config/ConfigPage";
import {
  ConfigField,
  ConfigFormSheet,
  configInputClass,
} from "../../components/config/ConfigFormSheet";
import { StatusBadge, WorkflowRunDetail } from "../../components/config/WorkflowRunDetail";
import { lookupLaunch } from "../../lib/workflow-launches";
import { argsDeclaration, parseArgsInput } from "../../lib/workflow-args";
import { WorkflowArgsForm, type ArgsFormState } from "../../components/config/WorkflowArgsForm";
import type {
  ConversationRunSummary,
  WorkflowDetailResponse,
  WorkflowHubGroup,
  WorkflowRunRow,
  WorkflowScope,
} from "../../lib/types";

// Dynamic workflows (bridge 0.48.0, server ADR-0029). Desktop hub parity:
// the list groups GLOBAL workflows plus every hub instance's PROJECT
// workflows with a per-name last-run badge — a workflow saved in another
// project's chat stays visible no matter which instance the app is
// connected to. Everything rides the per-instance settings routes, so the
// whole page is inert without a connected hub.
//
// Run rows split by what the app can KNOW: a run this app launched carries
// its ACP session in the local launch memory (open / inspect / resume), and
// an editor-launched run carries the bridge-joined acpSessionId (bridge
// 0.57.0 resolves the journal's backend session id to an attachable alias).
// A row with NEITHER — an old journal row, or one whose bridge lost every
// alias — stays read-only.

/** Journal status tokens for a run that is still flying (drives the poll cadence). */
const RUN_ACTIVE = new Set(["pending", "running"]);

/** Overview poll cadence: fast while anything is live anywhere, slow when idle. */
const POLL_LIVE_MS = 5000;
const POLL_IDLE_MS = 30_000;

export function WorkflowsPage() {
  const { t } = useTranslation();
  const hubGroups = useAppStore((s) => s.workflowHub);
  const activeRuns = useAppStore((s) => s.workflowActiveRuns);
  const loading = useAppStore((s) => s.workflowHubLoading);
  const supported = useAppStore((s) => s.configSupported);
  const error = useAppStore((s) => s.configError);
  const hubError = useAppStore((s) => s.workflowHubError);
  const instanceId = useAppStore((s) => s.instanceId);
  const loadWorkflowHub = useAppStore((s) => s.loadWorkflowHub);
  const workflowAction = useAppStore((s) => s.workflowAction);
  const setComposerPrefill = useAppStore((s) => s.setComposerPrefill);
  const closeConfig = useAppStore((s) => s.closeConfig);

  const [selected, setSelected] = useState<{
    scope: WorkflowScope;
    name: string;
    instanceId: string;
  } | null>(null);
  const [runDetail, setRunDetail] = useState<{
    sessionId: string;
    runId: string;
    name: string;
    instanceId: string;
  } | null>(null);

  // Deep link (a session-panel run row tap): open straight at that run's
  // detail. Consumed exactly once so a later plain visit starts at the list.
  useEffect(() => {
    const target = useAppStore.getState().workflowRunTarget;
    if (target) {
      setRunDetail(target);
      useAppStore.getState().clearWorkflowRunTarget();
    }
  }, []);

  // First paint is seeded by the ZCodeConfigScreen mount effect (the section
  // case in loadConfigSection → loadWorkflowHub); returning from a detail
  // page re-seeds here. No page-level mount effect — child effects run
  // before the parent's, so one here would double-fire the seed read.

  // Poll the overview for as long as the page is open — NOT gated on a
  // one-shot read: a journal row can be terminal-and-wrong (bridge-side
  // correction only happens per read), and a run started in another project
  // must appear without a manual refresh. The cadence is adaptive: 5s while
  // anything is live anywhere on the machine (badges and resume affordances
  // follow the run), 30s once everything is settled — a steady 5s idle poll
  // is pure hub churn for a page that rarely changes.
  const anyRunning =
    (hubGroups?.some((g) => Object.values(g.lastRuns).some((r) => RUN_ACTIVE.has(r.status))) ??
      false) ||
    (activeRuns?.some((r) => RUN_ACTIVE.has(r.status)) ?? false);
  useEffect(() => {
    const timer = setInterval(
      () => void loadWorkflowHub(),
      anyRunning ? POLL_LIVE_MS : POLL_IDLE_MS,
    );
    return () => clearInterval(timer);
  }, [anyRunning, loadWorkflowHub]);

  // "Create via conversation": the bridge hands back the desktop's prefilled
  // prompt; the app stages it as the composer draft and never auto-sends.
  async function createViaChat() {
    const res = await workflowAction("load create prompt", (c, iid) =>
      c.workflowCreatePrompt(iid, "project"),
    );
    if (!res) return;
    setComposerPrefill(res.prompt);
    closeConfig();
    if (!useAppStore.getState().activeSessionId) {
      useAppStore.getState().notify("prompt staged — open a session to review and send it");
    }
  }

  if (runDetail) {
    return (
      <WorkflowRunDetail
        // A supersede re-points the run id; the key FORCES a remount so no
        // poller state (afterSequence cursors, loaded tabs) survives from the
        // old run's journal — its sequence numbers do not carry over.
        key={runDetail.runId}
        instanceId={runDetail.instanceId}
        sessionId={runDetail.sessionId}
        runId={runDetail.runId}
        title={runDetail.name}
        onBack={() => setRunDetail(null)}
        onRepoint={(newRunId) =>
          setRunDetail((prev) => (prev ? { ...prev, runId: newRunId } : prev))
        }
      />
    );
  }

  if (selected) {
    return (
      <WorkflowDetail
        scope={selected.scope}
        name={selected.name}
        instanceId={selected.instanceId}
        onBack={() => {
          setSelected(null);
          void loadWorkflowHub();
        }}
        onOpenRun={(sessionId, runId, name, ownerInstanceId) =>
          setRunDetail({ sessionId, runId, name, instanceId: ownerInstanceId })
        }
      />
    );
  }

  const groups = hubGroups ?? [];
  const allEmpty =
    groups.length > 0 &&
    groups.every((g) => g.error === null && g.workflows.length === 0 && g.invalid.length === 0);

  return (
    <ConfigPageFrame
      title={t("zconfig.workflows")}
      // Deep-opened from the session panel → back returns to the chat, not to
      // the settings entry list the user never saw.
      onBack={() => useAppStore.getState().backFromConfigSection()}
      onRefresh={() => void loadWorkflowHub()}
      refreshing={loading}
      unsupported={supported === false}
      error={error ?? hubError}
      hasLoaded={hubGroups !== null}
    >
      {!instanceId && (
        <p className="mx-4 mt-2 rounded-xl bg-surface px-4 py-3 text-xs text-warning ring-1 ring-hairline">
          {t("zconfig.workflowNeedInstance")}
        </p>
      )}

      {/* The live zone: everything workflow-related lives INSIDE this page
          (the session panel keeps only the entry row) — in-flight runs get
          the big area up top, settled history stays in each workflow. Not
          gated on a connected instance: the run list is machine-wide (hub
          overview) and each row carries its owner instance, so runs started
          in projects this app is NOT attached to still show up. */}
      <ActiveRunsBlock
        instanceId={instanceId}
        onOpenRun={(sessionId, runId, name, ownerInstanceId) =>
          setRunDetail({ sessionId, runId, name, instanceId: ownerInstanceId })
        }
      />

      <RecentRunsBlock
        instanceId={instanceId}
        onOpenRun={(sessionId, runId, name, ownerInstanceId) =>
          setRunDetail({ sessionId, runId, name, instanceId: ownerInstanceId })
        }
      />

      {allEmpty ? (
        <ConfigEmpty text={t("zconfig.workflowsEmpty")} />
      ) : (
        groups.map((g) => (
          <GroupBlock
            key={g.scope === "global" ? "global" : g.instanceId}
            group={g}
            onOpen={(name) => setSelected({ scope: g.scope, name, instanceId: g.instanceId })}
          />
        ))
      )}

      <div className="px-4">
        <button
          onClick={() => void createViaChat()}
          className="mt-3 flex w-full items-center justify-center gap-2 rounded-xl bg-raised px-3 py-2.5 text-sm text-dim hover:bg-hover active:bg-active"
        >
          <Plus className="size-4" />
          {t("zconfig.workflowCreateChat")}
        </button>
      </div>
    </ConfigPageFrame>
  );
}

/**
 * The machine-wide in-flight runs — the live zone at the top of the page,
 * rendered STRAIGHT from the hub's overview (bridge 0.60.0): the journal is
 * shared across every project, so this list covers all of them, and each row
 * carries the hub-annotated `ownerInstanceId` (the instance whose live
 * session list holds the run's session) so stop/detail address the right
 * bridge even though they are not the connected one. Hidden entirely when
 * nothing flies; the page's overview poll (5s live / 30s idle) follows the
 * same store state.
 */
function ActiveRunsBlock({
  instanceId,
  onOpenRun,
}: {
  /** The connected instance — LAST-RESORT action address; null when the app
   *  is not attached to any session (the hub's owner annotation still routes
   *  stop/detail correctly). */
  instanceId: string | null;
  onOpenRun: (sessionId: string, runId: string, name: string, ownerInstanceId: string) => void;
}) {
  const { t } = useTranslation();
  const workflowAction = useAppStore((s) => s.workflowAction);
  const stopWorkflowRun = useAppStore((s) => s.stopWorkflowRun);
  const activeSessionId = useAppStore((s) => s.activeSessionId);
  const rows = useAppStore((s) => s.workflowActiveRuns);
  // Last-resort session join: runId → the CURRENT ACP session, filled from
  // conversationRuns. The hub's `acpSessionId` join covers editor-launched
  // runs and launch memory covers this app's own, but a run started in the
  // open session before this app's data survived (or that the bridge cannot
  // alias-resolve) is only reachable through this map.
  const [sessionRuns, setSessionRuns] = useState<Map<string, string>>(new Map());
  // Epoch guard: an in-flight read from a PREVIOUS instance (or a closed
  // page) must not land — its rows would misattribute stop actions to the
  // wrong instance for up to a poll cycle.
  const epoch = useRef(0);

  // The fallback join only matters while live rows are on screen; with an
  // empty list this poll would be a second idle 5s loop for nothing.
  const hasRows = rows !== null && rows.length > 0;

  useEffect(() => {
    epoch.current += 1;
    setSessionRuns(new Map());
    if (!activeSessionId || !instanceId || !hasRows) return;
    // Silent by design: a background poll — a transient failure just keeps
    // the previous map. Stale keys are runId-exact, so a kept entry can only
    // act on a run that was in this session.
    const iid = instanceId;
    const sid = activeSessionId;
    async function load() {
      const at = epoch.current;
      const conv = await workflowAction(
        "load session runs",
        (c) => c.conversationRuns(iid, sid),
        true,
      );
      if (at !== epoch.current) return;
      if (conv) setSessionRuns(new Map(conv.runs.map((r) => [r.runId, sid])));
    }
    void load();
    const timer = setInterval(() => void load(), 5000);
    return () => {
      epoch.current += 1;
      clearInterval(timer);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [instanceId, activeSessionId, hasRows]);

  if (rows === null || rows.length === 0) return null;

  async function stop(row: WorkflowRunRow) {
    const launch = lookupLaunch(row.runId);
    const sessionId = launch?.acpSessionId ?? row.acpSessionId ?? sessionRuns.get(row.runId);
    if (!sessionId) return;
    // The hub's owner annotation is the live truth (that instance currently
    // lists the session); launch memory is the fallback for rows the hub
    // could not place.
    const ownerInstanceId = row.ownerInstanceId ?? launch?.instanceId ?? instanceId ?? undefined;
    const ok = await stopWorkflowRun(row.runId, sessionId, ownerInstanceId);
    if (ok) void useAppStore.getState().loadWorkflowHub();
  }

  return (
    <ConfigBlock title={t("zconfig.workflowActiveTitle")} divided>
      {rows.map((r) => {
        const launch = lookupLaunch(r.runId);
        const rowSessionId = launch?.acpSessionId ?? r.acpSessionId ?? sessionRuns.get(r.runId);
        const rowInstanceId = r.ownerInstanceId ?? launch?.instanceId ?? instanceId ?? "";
        const label = r.name ?? r.runId.slice(0, 12);
        return (
          <div key={r.runId} className="px-4 py-3">
            <div className="flex items-center gap-2">
              <span className="min-w-0 flex-1 truncate text-sm text-ink">{label}</span>
              {r.updatedAt ? (
                <span className="shrink-0 text-2xs text-faint">{fmtStamp(r.updatedAt)}</span>
              ) : null}
              <StatusBadge status={r.status} />
            </div>
            {rowSessionId && rowInstanceId && (
              <div className="mt-2 flex flex-wrap gap-1.5">
                <button
                  onClick={() => onOpenRun(rowSessionId, r.runId, r.name ?? label, rowInstanceId)}
                  className="rounded-lg bg-hover px-2.5 py-1.5 text-2xs text-dim active:bg-active"
                >
                  {t("zconfig.workflowRunDetail")}
                </button>
                <button
                  onClick={() => void stop(r)}
                  className="rounded-lg bg-danger-wash px-2.5 py-1.5 text-2xs font-medium text-danger active:brightness-125"
                >
                  {t("zconfig.workflowStopRun")}
                </button>
              </div>
            )}
          </div>
        );
      })}
    </ConfigBlock>
  );
}

/**
 * The finished complement of the live zone: the overview's terminal rows,
 * newest first (hub-capped) — the top-level answer to "where did my finished
 * runs go" (before this, a settled run only resurfaced two taps deep inside
 * its workflow's history). Rows stay VISIBLE even when no session address is
 * known (an old journal row the bridge can no longer alias-join) — visibility
 * is the point; the detail button simply hides for those. Dismissal rides
 * the same hide list the per-workflow history uses.
 */
function RecentRunsBlock({
  instanceId,
  onOpenRun,
}: {
  /** The connected instance — LAST-RESORT action address; null when the app
   *  is not attached to any session (the hub's owner annotation still routes
   *  the detail view correctly). */
  instanceId: string | null;
  onOpenRun: (sessionId: string, runId: string, name: string, ownerInstanceId: string) => void;
}) {
  const { t } = useTranslation();
  const rows = useAppStore((s) => s.workflowRecentRuns);
  const workflowAction = useAppStore((s) => s.workflowAction);
  const loadWorkflowHub = useAppStore((s) => s.loadWorkflowHub);

  if (rows === null || rows.length === 0) return null;

  async function dismiss(row: WorkflowRunRow) {
    const iid = row.ownerInstanceId ?? lookupLaunch(row.runId)?.instanceId ?? instanceId;
    if (!iid) return;
    const ok = await workflowAction(
      "dismiss run",
      (c) => c.dismissWorkflowRun(iid, row.runId),
      false,
      undefined,
      iid,
    );
    if (ok) void loadWorkflowHub();
  }

  return (
    <ConfigBlock title={t("zconfig.workflowRecentTitle")} divided>
      {rows.map((r) => {
        const launch = lookupLaunch(r.runId);
        const rowSessionId = launch?.acpSessionId ?? r.acpSessionId;
        const rowInstanceId = r.ownerInstanceId ?? launch?.instanceId ?? instanceId ?? "";
        const label = r.name ?? r.runId.slice(0, 12);
        return (
          <div key={r.runId} className="px-4 py-3">
            <div className="flex items-baseline gap-2">
              <span className="min-w-0 flex-1">
                <span className="block truncate text-sm text-ink">{label}</span>
                <span className="block text-2xs text-faint">
                  {r.updatedAt ? `${fmtStamp(r.updatedAt)} · ` : ""}
                  {r.spentTokens ? `${fmtTokens(r.spentTokens)} · ` : ""}
                  {r.artifacts?.length ? `${r.artifacts.length}◆` : ""}
                </span>
              </span>
              <StatusBadge status={r.status} />
              <button
                onClick={() => void dismiss(r)}
                aria-label={t("zconfig.workflowDismissRun")}
                title={t("zconfig.workflowDismissRun")}
                className="flex size-6 shrink-0 items-center justify-center rounded-md text-faint active:bg-active"
              >
                <X className="size-3.5" />
              </button>
            </div>
            {rowSessionId && rowInstanceId && (
              <div className="mt-2">
                <button
                  onClick={() => onOpenRun(rowSessionId, r.runId, r.name ?? label, rowInstanceId)}
                  className="rounded-lg bg-hover px-2.5 py-1.5 text-2xs text-dim active:bg-active"
                >
                  {t("zconfig.workflowRunDetail")}
                </button>
              </div>
            )}
          </div>
        );
      })}
    </ConfigBlock>
  );
}

/** One group: the global workflows, or one instance's project workflows. */
function GroupBlock({
  group,
  onOpen,
}: {
  group: WorkflowHubGroup;
  onOpen: (name: string) => void;
}) {
  const { t } = useTranslation();
  const label =
    group.scope === "global"
      ? t("zconfig.workflowScope_global")
      : group.workspace.split("/").pop() || group.workspace;
  if (group.error) {
    return (
      <ConfigBlock title={label} divided>
        <p className="px-4 py-2.5 text-2xs text-faint">
          {t("zconfig.workflowInstanceUnavailable")}
        </p>
      </ConfigBlock>
    );
  }
  // An empty project group renders nothing — the noise-free form of "this
  // project has no saved workflows" (the all-empty card covers the rest).
  if (group.workflows.length === 0 && group.invalid.length === 0) return null;
  return (
    <ConfigBlock title={label} divided>
      {group.workflows.map((w) => {
        const last = group.lastRuns[w.name];
        // Desktop-parity card content: name + status, two-line description,
        // and a footer of last-run time plus the last run's arg names.
        const argNames = Object.keys(last?.args ?? {});
        return (
          <button
            key={w.name}
            onClick={() => onOpen(w.name)}
            className="flex w-full flex-col gap-1 px-4 py-3 text-left hover:bg-hover active:bg-active"
          >
            <span className="flex w-full items-center gap-2">
              <span className="min-w-0 flex-1 truncate text-sm text-ink">{w.name}</span>
              {last ? <StatusBadge status={last.status} /> : null}
              <ChevronRight className="size-4 shrink-0 text-faint" />
            </span>
            {w.description && (
              <span className="line-clamp-2 text-2xs leading-snug text-faint">
                {w.description}
              </span>
            )}
            {(last?.updatedAt || argNames.length > 0) && (
              <span className="mt-0.5 flex w-full flex-wrap items-center gap-1.5">
                {last?.updatedAt ? (
                  <span className="text-2xs text-faint">{fmtStamp(last.updatedAt)}</span>
                ) : null}
                {argNames.slice(0, 3).map((a) => (
                  <span
                    key={a}
                    className="rounded bg-hover px-1.5 py-0.5 font-mono text-2xs text-dim"
                  >
                    {a}
                  </span>
                ))}
                {argNames.length > 3 && (
                  <span className="text-2xs text-faint">+{argNames.length - 3}</span>
                )}
              </span>
            )}
          </button>
        );
      })}
      {group.invalid.length > 0 &&
        group.invalid.map((e, i) => (
          <div key={i} className="px-4 py-2.5">
            <p className="truncate font-mono text-2xs text-faint">{e.path}</p>
            {e.reason && <p className="truncate text-2xs text-warning/80">{e.reason}</p>}
          </div>
        ))}
    </ConfigBlock>
  );
}

// ---------- L1: one saved workflow ----------

function WorkflowDetail({
  scope,
  name,
  instanceId,
  onBack,
  onOpenRun,
}: {
  scope: WorkflowScope;
  name: string;
  /** The instance whose group opened this page — actions address it. */
  instanceId: string;
  onBack: () => void;
  onOpenRun: (sessionId: string, runId: string, name: string, ownerInstance: string) => void;
}) {
  const { t } = useTranslation();
  const workflowAction = useAppStore((s) => s.workflowAction);
  const loadRunSummaries = useAppStore((s) => s.loadRunSummaries);
  const startWorkflow = useAppStore((s) => s.startWorkflow);
  const resumeWorkflowRun = useAppStore((s) => s.resumeWorkflowRun);
  const openSession = useAppStore((s) => s.openSession);

  const [detail, setDetail] = useState<WorkflowDetailResponse | null>(null);
  const [runs, setRuns] = useState<WorkflowRunRow[] | null>(null);
  const [summaries, setSummaries] = useState<Record<string, ConversationRunSummary[]>>({});
  const [scriptOpen, setScriptOpen] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [editingDesc, setEditingDesc] = useState(false);
  const [starting, setStarting] = useState(false);
  const [loading, setLoading] = useState(false);

  // skipDetail serves the 5s poll: the workflow's meta (description, script
  // path) is static between runs, so only history + summaries refresh — and
  // the loading gate stays untouched to keep the poll invisible.
  async function load(skipDetail = false) {
    if (!skipDetail) setLoading(true);
    const [res, history] = await Promise.all([
      skipDetail
        ? Promise.resolve(null)
        : workflowAction(
            "load workflow",
            (c, iid) => c.workflowGet(iid, scope, name),
            false,
            undefined,
            instanceId,
          ),
      workflowAction(
        "load runs",
        (c, iid) => c.workflowRunsHistory(iid, { scope, name }),
        false,
        undefined,
        instanceId,
      ),
    ]);
    if (res) setDetail(res);
    // Only a landed answer updates the list: a failed read keeps `null`, and
    // the render below shows nothing rather than dressing the error up as
    // "no runs recorded yet" (the toast already said what went wrong).
    if (history) setRuns(history.runs);
    // Refresh the resumable verdicts for every session a row can act on —
    // this app's launches AND the bridge-joined aliases of editor-launched
    // runs — silent per design (loadRunSummaries), so dead sessions simply
    // leave their rows without a resume button.
    const sessionIds = Array.from(
      new Set(
        (history?.runs ?? [])
          .flatMap((r) => [lookupLaunch(r.runId)?.acpSessionId, r.acpSessionId])
          .filter((sid): sid is string => Boolean(sid)),
      ),
    );
    if (sessionIds.length > 0) setSummaries(await loadRunSummaries(sessionIds, instanceId));
    if (!skipDetail) setLoading(false);
  }

  useEffect(() => {
    void load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // While this workflow's newest run is active, refresh the history rows so
  // the badge and the resume affordance follow the run (bridge 5s poll).
  const runActive = runs?.some((r) => RUN_ACTIVE.has(r.status)) ?? false;
  useEffect(() => {
    if (!runActive) return;
    const timer = setInterval(() => void load(true), 5000);
    return () => clearInterval(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [runActive]);

  const description = detail?.meta?.["description"];
  const descText = typeof description === "string" ? description : "";
  const whenToUseRaw = detail?.meta?.["whenToUse"];
  const whenToUse = typeof whenToUseRaw === "string" ? whenToUseRaw : "";

  async function saveMeta(next: { description: string; whenToUse: string }) {
    setEditingDesc(false);
    // updateMeta REPLACES the whole meta — without a loaded detail the merge
    // base is unknown and a bare {description} would wipe whenToUse/args.
    if (!detail) {
      setLoading(false);
      return;
    }
    setLoading(true);
    // Empty whenToUse = drop the key entirely (upstream wants non-empty
    // strings or absence, never "") — strip the OLD value before merging, or
    // the spread below would silently carry it back in.
    const { whenToUse: _old, ...rest } = detail.meta ?? {};
    const res = await workflowAction(
      "save description",
      (c, iid) =>
        c.workflowUpdateMeta(iid, scope, name, {
          ...rest,
          description: next.description,
          ...(next.whenToUse.trim() ? { whenToUse: next.whenToUse.trim() } : {}),
        }),
      false,
      undefined,
      instanceId,
    );
    setLoading(false);
    if (res) void load();
  }

  // Promote-to-global rides the same chat-prefill path as creation — the
  // desktop's launcher does exactly this (a saved-workflow launch prompt);
  // there is no dedicated REST for it.
  function promoteViaChat() {
    useAppStore
      .getState()
      .setComposerPrefill(
        `Please promote the saved workflow "${name}" from this project's scope to the global scope: move the file under ~/.zcode/workflows/ keeping its script, description, whenToUse and args declaration unchanged, then remove the project copy. If it is inherently bound to this project and cannot be generalized meaningfully, say why and stop without saving.`,
      );
    useAppStore.getState().closeConfig();
    if (!useAppStore.getState().activeSessionId) {
      useAppStore.getState().notify("prompt staged — open a session to review and send it");
    }
  }

  async function start(args?: Record<string, unknown>) {
    setStarting(false);
    const res = await startWorkflow({
      scope,
      name,
      args,
      instanceId,
    });
    if (!res) return;
    useAppStore.getState().closeConfig();
    void openSession(instanceId, res.acpSessionId);
  }

  async function resume(row: WorkflowRunRow) {
    const launch = lookupLaunch(row.runId);
    const sessionId = launch?.acpSessionId ?? row.acpSessionId;
    if (!sessionId) return;
    const ownerInstance = launch?.instanceId ?? instanceId;
    const ok = await resumeWorkflowRun({
      runId: row.runId,
      sessionId,
      ...(row.name ? { name: row.name } : {}),
      instanceId: ownerInstance,
    });
    if (ok) {
      useAppStore.getState().closeConfig();
      void openSession(ownerInstance, sessionId);
    }
  }

  // Dismissal (bridge 0.58.0 hide list): the journal has no delete — a
  // dismissed settled run disappears from every list the bridge serves,
  // this page's history included.
  async function dismissRun(row: WorkflowRunRow) {
    const ok = await workflowAction(
      "dismiss run",
      (c, iid) => c.dismissWorkflowRun(iid, row.runId),
      false,
      undefined,
      instanceId,
    );
    if (ok) void load();
  }

  async function clearFinished() {
    const ids = (runs ?? []).filter((r) => !RUN_ACTIVE.has(r.status)).map((r) => r.runId);
    if (ids.length === 0) return;
    const res = await workflowAction(
      "clear finished runs",
      (c, iid) => c.dismissWorkflowRuns(iid, ids),
      false,
      undefined,
      instanceId,
    );
    if (res) void load();
  }

  async function remove() {
    setConfirmDelete(false);
    const res = await workflowAction(
      "delete workflow",
      (c, iid) => c.workflowDelete(iid, scope, name),
      false,
      undefined,
      instanceId,
    );
    if (!res) return;
    onBack();
  }

  async function moveToProject() {
    const res = await workflowAction(
      "move workflow",
      (c, iid) => c.workflowMove(iid, scope, name),
      false,
      undefined,
      instanceId,
    );
    if (!res) return;
    onBack();
  }

  return (
    <ConfigPageFrame
      title={name}
      onBack={onBack}
      onRefresh={() => void load()}
      refreshing={loading}
      loading={loading}
      hasLoaded={detail !== null}
    >
      <ConfigBlock>
        <div className="flex items-center gap-2 px-4 pt-3">
          <span className="rounded-md bg-selected px-1.5 py-0.5 text-2xs text-dim">
            {t(`zconfig.workflowScope_${scope}`)}
          </span>
          {scope === "project" && (
            <span className="truncate text-2xs text-faint">{instanceId}</span>
          )}
        </div>
        {descText ? (
          <div className="px-4 py-2.5">
            <p className="text-xs text-dim">{descText}</p>
          </div>
        ) : null}
        {whenToUse ? (
          <div className="px-4 pb-2.5">
            <p className="text-2xs text-faint">{whenToUse}</p>
          </div>
        ) : null}
        {detail?.path && (
          <div className="px-4 pb-2.5">
            <p className="break-all font-mono text-2xs text-faint">{detail.path}</p>
          </div>
        )}
        {detail && (
          <div className="px-4 py-2.5">
            <button
              onClick={() => setEditingDesc(true)}
              className="rounded-lg bg-hover px-2.5 py-1.5 text-2xs text-dim active:bg-active"
            >
              {t("zconfig.workflowEditDescription")}
            </button>
          </div>
        )}
      </ConfigBlock>

      <div className="px-4">
        <button
          onClick={() => setStarting(true)}
          className="mt-3 w-full rounded-xl bg-white px-3 py-2.5 text-sm font-semibold text-black transition active:scale-[0.99]"
        >
          {t("zconfig.workflowStart")}
        </button>
        <div className="mt-2 flex gap-2">
          {scope === "global" ? (
            <button
              onClick={() => void moveToProject()}
              className="flex-1 rounded-xl bg-raised px-3 py-2 text-xs text-dim hover:bg-hover active:bg-active"
            >
              {t("zconfig.workflowMoveToProject")}
            </button>
          ) : (
            <button
              onClick={promoteViaChat}
              className="flex-1 rounded-xl bg-raised px-3 py-2 text-xs text-dim hover:bg-hover active:bg-active"
            >
              {t("zconfig.workflowPromoteGlobal")}
            </button>
          )}
          {confirmDelete ? (
            <span className="flex flex-1 items-center justify-end gap-1.5">
              <button
                onClick={() => setConfirmDelete(false)}
                className="rounded-lg px-2 py-1 text-2xs text-faint"
              >
                {t("common.cancel")}
              </button>
              <button
                onClick={() => void remove()}
                className="rounded-lg bg-danger-wash px-2.5 py-1.5 text-2xs font-medium text-danger"
              >
                {t("zconfig.workflowConfirmDelete")}
              </button>
            </span>
          ) : (
            <button
              onClick={() => setConfirmDelete(true)}
              className="flex-1 rounded-xl bg-raised px-3 py-2 text-xs text-dim hover:bg-hover active:bg-active"
            >
              {t("zconfig.workflowDelete")}
            </button>
          )}
        </div>
      </div>

      {detail?.script && (
        <ConfigBlock title={t("zconfig.workflowScript")} divided>
          <button
            onClick={() => setScriptOpen(!scriptOpen)}
            className="w-full px-4 py-2.5 text-left text-xs text-dim hover:bg-hover active:bg-active"
          >
            {scriptOpen ? t("zconfig.workflowHideScript") : t("zconfig.workflowShowScript")}
          </button>
          {scriptOpen && (
            <pre className="max-h-80 overflow-auto px-4 pb-3 font-mono text-2xs leading-relaxed text-faint">
              {detail.script}
            </pre>
          )}
        </ConfigBlock>
      )}

      <ConfigBlock title={t("zconfig.workflowRunsTitle")} divided>
        {runs !== null && runs.some((r) => !RUN_ACTIVE.has(r.status)) && (
          <button
            onClick={() => void clearFinished()}
            className="w-full px-4 py-2 text-left text-2xs text-faint hover:bg-hover active:bg-active"
          >
            {t("zconfig.workflowClearFinished")}
          </button>
        )}
        {runs === null ? (
          // Loading only while a read is in flight; a FAILED read keeps
          // `runs` null with no spinner — the toast owns that story.
          loading ? (
            <p className="px-4 py-4 text-xs text-faint">{t("zconfig.loading")}</p>
          ) : null
        ) : runs.length === 0 ? (
          <ConfigEmpty text={t("zconfig.workflowRunsEmpty")} />
        ) : (
          runs.map((r) => {
            const launch = lookupLaunch(r.runId);
            // The launch memory wins for runs this app started (it knows the
            // owning instance); the bridge-joined alias (bridge 0.57.0) makes
            // editor-launched runs actionable too. Neither = read-only row.
            const rowSessionId = launch?.acpSessionId ?? r.acpSessionId;
            const rowInstanceId = launch?.instanceId ?? instanceId;
            const summary = rowSessionId
              ? summaries[rowSessionId]?.find((s) => s.runId === r.runId)
              : undefined;
            return (
              <div key={r.runId} className="px-4 py-3">
                <div className="flex items-baseline gap-2">
                  <span className="min-w-0 flex-1">
                    <span className="block text-sm text-ink">{r.name ?? r.runId.slice(0, 12)}</span>
                    <span className="block text-2xs text-faint">
                      {r.updatedAt ? `${fmtStamp(r.updatedAt)} · ` : ""}
                      {r.spentTokens ? `${fmtTokens(r.spentTokens)} · ` : ""}
                      {r.artifacts?.length ? `${r.artifacts.length}◆` : ""}
                    </span>
                    {(summary?.resumedFrom || r.resumedFrom) && (
                      <span className="block truncate text-2xs text-faint">
                        {t("zconfig.workflowLineageFrom")} …
                        {(summary?.resumedFrom ?? r.resumedFrom)!.slice(-8)}
                      </span>
                    )}
                    {(summary?.supersededBy || r.supersededBy) && (
                      <span className="block truncate text-2xs text-faint">
                        {t("zconfig.workflowLineageBy")} …
                        {(summary?.supersededBy ?? r.supersededBy)!.slice(-8)}
                      </span>
                    )}
                  </span>
                  <StatusBadge status={summary?.status ?? r.status} />
                  {!RUN_ACTIVE.has(summary?.status ?? r.status) && (
                    <button
                      onClick={() => void dismissRun(r)}
                      aria-label={t("zconfig.workflowDismissRun")}
                      title={t("zconfig.workflowDismissRun")}
                      className="flex size-6 shrink-0 items-center justify-center rounded-md text-faint active:bg-active"
                    >
                      <X className="size-3.5" />
                    </button>
                  )}
                </div>
                {rowSessionId ? (
                  <div className="mt-2 flex flex-wrap gap-1.5">
                    <button
                      onClick={() =>
                        onOpenRun(rowSessionId, r.runId, r.name ?? name, rowInstanceId)
                      }
                      className="rounded-lg bg-hover px-2.5 py-1.5 text-2xs text-dim active:bg-active"
                    >
                      {t("zconfig.workflowRunDetail")}
                    </button>
                    <button
                      onClick={() => void openSession(rowInstanceId, rowSessionId)}
                      className="rounded-lg bg-hover px-2.5 py-1.5 text-2xs text-dim active:bg-active"
                    >
                      {t("zconfig.workflowOpenSession")}
                    </button>
                    {summary?.resumable && (
                      <button
                        onClick={() => void resume(r)}
                        className="rounded-lg bg-info-wash px-2.5 py-1.5 text-2xs font-medium text-info"
                      >
                        {t("zconfig.workflowResume")}
                      </button>
                    )}
                  </div>
                ) : (
                  <p className="pt-1 text-2xs text-faint">
                    {t("zconfig.workflowRunUnknownSession")}
                  </p>
                )}
              </div>
            );
          })
        )}
      </ConfigBlock>

      {starting && (
        <StartSheet
          meta={detail?.meta}
          onClose={() => setStarting(false)}
          onStart={(args) => void start(args)}
        />
      )}
      {editingDesc && (
        <MetaSheet
          initial={{ description: descText, whenToUse }}
          onClose={() => setEditingDesc(false)}
          onSave={(next) => void saveMeta(next)}
        />
      )}
    </ConfigPageFrame>
  );
}

// ---------- sheets ----------

/**
 * Launch args: the typed form when the workflow declares them, the free-form
 * JSON textarea otherwise (validated client-side before any request).
 */
function StartSheet({
  meta,
  onClose,
  onStart,
}: {
  meta: Record<string, unknown> | undefined;
  onClose: () => void;
  onStart: (args?: Record<string, unknown>) => void;
}) {
  const { t } = useTranslation();
  const typed = argsDeclaration(meta) !== null;
  const [form, setForm] = useState<ArgsFormState>({});
  const [text, setText] = useState("");
  const parsed = parseArgsInput(text);
  return (
    <ConfigFormSheet
      title={t("zconfig.workflowStart")}
      submitDisabled={typed ? form.error !== undefined : parsed.error !== undefined}
      onClose={onClose}
      onSubmit={() => onStart(typed ? form.args : parsed.args)}
    >
      {typed ? (
        <WorkflowArgsForm meta={meta} onChange={setForm} />
      ) : (
        <ConfigField
          label={t("zconfig.workflowStartArgs")}
          hint={parsed.error ? parsed.error : t("zconfig.workflowStartArgsHint")}
        >
          <textarea
            value={text}
            onChange={(e) => setText(e.target.value)}
            rows={5}
            spellCheck={false}
            className={`${configInputClass} font-mono`}
            placeholder={'{"target": "src/"}'}
          />
        </ConfigField>
      )}
      {typed && form.error ? <p className="px-1 text-2xs text-danger">{form.error}</p> : null}
    </ConfigFormSheet>
  );
}

/**
 * Edit the meta's two prose fields. description is required non-empty
 * upstream; whenToUse is optional (empty = drop the key, never sent as "").
 */
function MetaSheet({
  initial,
  onClose,
  onSave,
}: {
  initial: { description: string; whenToUse: string };
  onClose: () => void;
  onSave: (next: { description: string; whenToUse: string }) => void;
}) {
  const { t } = useTranslation();
  const [description, setDescription] = useState(initial.description);
  const [whenToUse, setWhenToUse] = useState(initial.whenToUse);
  return (
    <ConfigFormSheet
      title={t("zconfig.workflowEditDescription")}
      submitDisabled={description.trim() === ""}
      onClose={onClose}
      onSubmit={() => onSave({ description: description.trim(), whenToUse: whenToUse.trim() })}
    >
      <ConfigField label={t("zconfig.workflowDescription")}>
        <textarea
          value={description}
          onChange={(e) => setDescription(e.target.value)}
          rows={3}
          className={configInputClass}
        />
      </ConfigField>
      <ConfigField label={t("zconfig.workflowWhenToUse")}>
        <textarea
          value={whenToUse}
          onChange={(e) => setWhenToUse(e.target.value)}
          rows={3}
          className={configInputClass}
        />
      </ConfigField>
    </ConfigFormSheet>
  );
}
