import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { ArrowLeft, ChevronRight, RefreshCw, SlidersHorizontal, Square } from "lucide-react";
import { useBackHandler } from "../../lib/backNav";
import { useAppStore } from "../../store/appStore";
import { MarkdownText } from "../Markdown";
import type {
  ConversationRunSummary,
  WorkflowArtifact,
  WorkflowNodeSummary,
  WorkflowRunEvent,
  WorkflowWorkspaceNode,
} from "../../lib/types";
import { ConfigBlock, ConfigEmpty, fmtStamp } from "./ConfigPage";
import { ConfigField, ConfigFormSheet, configInputClass } from "./ConfigFormSheet";
import {
  ArtifactMetaCard,
  BoardView,
  ChartView,
  MetricsTiles,
  RawData,
  TableView,
  cellText,
} from "./WorkflowArtifactViews";
import {
  describeRunEvent,
  newestFirst,
  nodeStateWord,
  phasePaints,
  phaseRows,
  reduceSlice,
  slicePhases,
  statusWord,
  type NodeAgg,
  type PhaseSlice,
} from "./workflowRunEventText";

// One workflow run's detail view (bridge 0.48.0, ADR-0029): the journal
// events, the artifacts, and the workspace nodes the backend's v4 queries
// expose — everything the chat progress card deliberately compresses.
//
// The journal contract does not restate the engine's payload shapes, so this
// view owns a per-kind rendering layer (workflowRunEventText for journal
// events, WorkflowArtifactViews for preset specs); raw JSON is an explicit
// toggle, never the default. Node lifecycles fold per instance into one line
// showing the LATEST state.
//
// Live-ness is polling, not push: the journal cursor (`afterSequence`) never
// invalidates, so a 3s tick while the run is active appends only the new
// lines, and a settled run costs exactly one fetch per tab. Appends are
// sequence-filtered regardless — a cursor that degenerates (a dropped
// sequence) makes the server answer with the FULL journal, which must never
// be re-appended onto the list.

/**
 * Desktop-parity status presentation (upstream run-status-presentation): a
 * colored dot ALWAYS pairs with the word. Semantic split — success emerald,
 * failure red with a halo, live sky (pulsing), pending an empty ring, and the
 * stopped family (stopped/cancelled/superseded/interrupted) deliberately
 * NEUTRAL: a stopped run is not an error. Unknown tokens fall back neutral.
 */
const STATUS_DOT: Record<string, string> = {
  completed: "bg-emerald-400",
  ok: "bg-emerald-400",
  settled: "bg-emerald-400",
  errored: "bg-red-400 ring-2 ring-red-400/25",
  failed: "bg-red-400 ring-2 ring-red-400/25",
  running: "bg-sky-400 animate-pulse",
  dispatched: "bg-sky-400 animate-pulse",
  executing: "bg-sky-400 animate-pulse",
  pending: "bg-transparent ring-1 ring-white/40",
  queued: "bg-transparent ring-1 ring-white/40",
  waiting: "bg-transparent ring-1 ring-white/40",
  stopped: "bg-white/35",
  cancelled: "bg-white/35",
  superseded: "bg-white/35",
  interrupted: "bg-white/35",
};

export function StatusBadge({ status }: { status?: string }) {
  const { t } = useTranslation();
  const token = status ?? "";
  return (
    <span className="flex shrink-0 items-center gap-1.5 rounded-md bg-white/[0.06] px-1.5 py-0.5 text-[10px] font-medium text-dim">
      <span className={`size-1.5 shrink-0 rounded-full ${STATUS_DOT[token] ?? "bg-white/35"}`} />
      {token ? statusWord(t, token) : "?"}
    </span>
  );
}

/**
 * Highest journal sequence seen — the safe resume cursor. NOT the last
 * element's sequence: a clipped journal page can end on an event without one,
 * and a degenerate cursor makes the server replay the whole journal.
 */
function maxSequence(events: WorkflowRunEvent[] | null): number {
  let max = 0;
  for (const e of events ?? []) {
    if (typeof e.sequence === "number" && e.sequence > max) max = e.sequence;
  }
  return max;
}

/** The newest version number an artifact advertises (top level or versions[]). */
function latestVersion(a: WorkflowArtifact): number {
  const vs = a.versions ?? [];
  for (let i = vs.length - 1; i >= 0; i--) {
    const v = Number(vs[i]?.["version"]);
    if (Number.isInteger(v)) return v;
  }
  const top = Number(a.version);
  return Number.isInteger(top) ? top : 0;
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

/**
 * Desktop-parity op tile (upstream WorkflowWorkspaceCard's kind tile): a
 * small colored square keyed by the op family — read/search/git/exec/write —
 * so a long node list scans by color before it reads by text.
 */
const OP_TILE: Array<[RegExp, string]> = [
  [/read|file|cat|list/, "bg-sky-500/15 text-sky-300"],
  [/search|grep|glob|find|scan/, "bg-violet-500/15 text-violet-300"],
  [/git/, "bg-amber-500/15 text-amber-300"],
  [/shell|bash|exec|spawn|terminal|run/, "bg-emerald-500/15 text-emerald-300"],
  [/write|edit|patch|apply|create|delete|move/, "bg-rose-500/15 text-rose-300"],
];

function opTileClass(op?: string): string {
  if (op) for (const [re, cls] of OP_TILE) if (re.test(op)) return cls;
  return "bg-white/[0.07] text-dim";
}

/** Wall time between two epoch-ms stamps, compact ("12s" / "3m41s"). */
function fmtDuration(from?: number, to?: number): string | null {
  if (!from || !to || to < from) return null;
  const s = Math.round((to - from) / 1000);
  if (s < 60) return `${s}s`;
  return `${Math.floor(s / 60)}m${s % 60}s`;
}

/**
 * The node summary line. `summary` is a strict object upstream — rendering it
 * as a React child would throw "Objects are not valid as a React child" and
 * take the whole app down, so it must be flattened to text here.
 */
function nodeSummaryText(s: WorkflowNodeSummary): string {
  const parts: string[] = [];
  if (s.resultCount !== undefined) parts.push(`${s.resultCount}`);
  if (s.exitCode !== undefined) parts.push(`exit ${s.exitCode}`);
  if (s.stdoutBytes !== undefined) parts.push(`out ${formatBytes(s.stdoutBytes)}`);
  if (s.stderrBytes !== undefined) parts.push(`err ${formatBytes(s.stderrBytes)}`);
  if (s.resultBytes !== undefined) parts.push(formatBytes(s.resultBytes));
  return parts.join(" · ");
}

/**
 * The node's WHAT — its call arguments flattened to one line. `args` is the
 * only field carrying a human-readable description of the operation (the
 * command run, the path read); strings join as-is, objects compact to
 * single-line JSON so the row stays a row.
 */
function argsText(n: WorkflowWorkspaceNode): string | undefined {
  const one = (v: unknown): string | undefined => {
    if (typeof v === "string") return v.trim() || undefined;
    if (typeof v === "number" || typeof v === "boolean") return String(v);
    if (typeof v === "object" && v !== null) {
      try {
        const s = JSON.stringify(v);
        return s && s !== "{}" && s !== "[]" ? s : undefined;
      } catch {
        return undefined;
      }
    }
    return undefined;
  };
  const args = n.args;
  if (Array.isArray(args)) {
    const parts = args.map(one).filter((v): v is string => v !== undefined);
    return parts.length > 0 ? parts.join(" ").slice(0, 160) : undefined;
  }
  const single = one(args);
  return single !== undefined ? single.slice(0, 160) : undefined;
}

/** Cap the chunk ask: ≤512KiB is the upstream per-read maximum anyway. */
const READ_CHUNK = 256 * 1024;

/**
 * Journal pages drained per pull. The view reads newest-first, so a long
 * settled journal must not open on its OLDEST page with the ending hidden
 * behind a button; the bound keeps one pull from walking a huge journal.
 */
const DRAIN_PAGES = 8;

type Tab = "events" | "artifacts" | "workspace";

export function WorkflowRunDetail({
  instanceId,
  sessionId,
  runId,
  title,
  onBack,
  onRepoint,
}: {
  instanceId: string;
  sessionId: string;
  runId: string;
  title: string;
  onBack: () => void;
  /** Supersede re-point: the amendment returned a NEW run id. */
  onRepoint: (runId: string) => void;
}) {
  const { t } = useTranslation();
  const workflowAction = useAppStore((s) => s.workflowAction);
  const resumeWorkflowRun = useAppStore((s) => s.resumeWorkflowRun);
  const stopWorkflowRun = useAppStore((s) => s.stopWorkflowRun);
  const amendWorkflowRun = useAppStore((s) => s.amendWorkflowRun);
  const [tab, setTab] = useState<Tab>("events");
  const [summary, setSummary] = useState<ConversationRunSummary | null>(null);
  const [amending, setAmending] = useState(false);
  const [stopping, setStopping] = useState(false);
  const [events, setEvents] = useState<WorkflowRunEvent[] | null>(null);
  const [eventsBusy, setEventsBusy] = useState(false);
  const [hasMore, setHasMore] = useState(false);
  const [artifacts, setArtifacts] = useState<WorkflowArtifact[] | null>(null);
  const [nodes, setNodes] = useState<WorkflowWorkspaceNode[] | null>(null);
  const [openArtifact, setOpenArtifact] = useState<WorkflowArtifact | null>(null);
  const [nodeResult, setNodeResult] = useState<{
    key: string;
    status?: string;
    body: unknown;
    truncated?: boolean;
  } | null>(null);
  // Set when the polls stop believing the session is reachable: the launch
  // memory went stale (bridge restart, closed session) and every tick would
  // fail. The view degrades to a read-only notice instead of erroring forever.
  const [sessionDead, setSessionDead] = useState(false);
  // One fetch at a time: a manual "Load newer" racing the 3s tick would read
  // the SAME afterSequence from two closures and append the batch twice.
  const eventsInFlight = useRef(false);
  const failCount = useRef(0);
  // Mirror of `events` for the pollers: the drain loop and the tick read the
  // cursor from here so neither closure re-binds on every append.
  const eventsRef = useRef<WorkflowRunEvent[] | null>(null);
  // Explicit phase folds by slice index; absent = only the newest phase open.
  const [phaseOpen, setPhaseOpen] = useState<Record<number, boolean>>({});

  useBackHandler(() => {
    if (openArtifact || nodeResult) {
      setOpenArtifact(null);
      setNodeResult(null);
      return true;
    }
    onBack();
    return true;
  });

  const refreshSummary = useCallback(
    async (silent = false) => {
      const res = await workflowAction(
        "load run status",
        (c) => c.conversationRuns(instanceId, sessionId),
        silent,
      );
      if (res) setSummary(res.runs.find((r) => r.runId === runId) ?? null);
      return res !== null;
    },
    [workflowAction, instanceId, sessionId, runId],
  );

  const fetchEvents = useCallback(
    async (initial: boolean, silent = false) => {
      if (eventsInFlight.current) return;
      eventsInFlight.current = true;
      setEventsBusy(true);
      let held: WorkflowRunEvent[] | null = initial ? null : eventsRef.current;
      let more: boolean | null = null;
      for (let page = 0; page < DRAIN_PAGES; page++) {
        const after = held === null ? undefined : maxSequence(held);
        const res = await workflowAction(
          "load run events",
          (c) => c.runEvents(instanceId, sessionId, runId, after),
          silent,
        );
        if (!res) break;
        // Defensive dedupe: an `after` the server could not honor answers
        // with the FULL journal — keep only sequences past what we hold.
        const floor = maxSequence(held);
        const fresh =
          held === null
            ? res.events
            : res.events.filter((e) => typeof e.sequence === "number" && e.sequence > floor);
        held = [...(held ?? []), ...fresh];
        eventsRef.current = held;
        setEvents(held);
        more = res.hasMore === true;
        if (!more) break;
      }
      // A failed first page leaves the gate as it was; otherwise it reads
      // true only when the bound cut the drain short.
      if (more !== null) setHasMore(more);
      eventsInFlight.current = false;
      setEventsBusy(false);
    },
    [workflowAction, instanceId, sessionId, runId],
  );

  // First paint: summary + the journal's head. (The mount effect in
  // ZCodeConfigScreen does not reach this component — it is not a section.)
  useEffect(() => {
    void refreshSummary();
    void fetchEvents(true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Poll while the run is unsettled. `summary === null` also polls: an
  // unknown status is not proof of completion. The tick is silent — a dead
  // session announces itself once below, not once per tick — and two
  // consecutive failed ticks retire the poller entirely (read-only view).
  const active =
    !sessionDead &&
    (summary === null || summary.status === "running" || summary.status === "pending");
  useEffect(() => {
    if (!active) return;
    const id = setInterval(async () => {
      const ok = await refreshSummary(true);
      // A null mirror means the first pull failed — pull from the head
      // again instead of skipping forever (eventsInFlight makes the overlap
      // with a still-running earlier pull safe).
      await fetchEvents(eventsRef.current === null, true);
      if (ok) {
        failCount.current = 0;
      } else if (++failCount.current >= 2) {
        setSessionDead(true);
      }
    }, 3000);
    return () => clearInterval(id);
  }, [active, refreshSummary, fetchEvents]);

  // Tabs load lazily — the journal is the default view, artifacts and the
  // workspace only cost a request once they are opened.
  useEffect(() => {
    if (tab === "artifacts" && artifacts === null) {
      void workflowAction("load artifacts", (c) =>
        c.runArtifacts(instanceId, sessionId, runId),
      ).then((res) => setArtifacts((res?.artifacts as WorkflowArtifact[]) ?? []));
    }
    if (tab === "workspace" && nodes === null) {
      void workflowAction("load workspace", (c) =>
        c.runWorkspace(instanceId, sessionId, runId),
      ).then((res) => setNodes(res?.nodes ?? []));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab]);

  async function resume() {
    const ok = await resumeWorkflowRun({ runId, sessionId, instanceId });
    if (ok) void refreshSummary();
  }

  async function stop() {
    setStopping(true);
    const ok = await stopWorkflowRun(runId, sessionId, instanceId);
    setStopping(false);
    if (ok) void refreshSummary();
  }

  // Phase cards read newest-first: the newest slice is where the run stands
  // and sits on top; older phases fold beneath it (slicePhases documents the
  // replay-safe node/actor homing). The visible head is the newest slice
  // that actually paints — a just-entered phase owns nothing yet and must
  // not leave the content beneath it folded shut behind a card that never
  // renders.
  const phaseSlices = useMemo(() => slicePhases(events ?? []), [events]);
  const headSlice = useMemo(() => {
    for (let i = phaseSlices.slices.length - 1; i >= 0; i--) {
      const fold = reduceSlice(t, phaseSlices.slices[i].events);
      if (
        phasePaints(
          fold,
          (key) => phaseSlices.nodeHome.get(key) === i,
          (key) => phaseSlices.actorHome.get(key) === i,
        )
      ) {
        return i;
      }
    }
    return -1;
  }, [t, phaseSlices]);
  const phaseCards = phaseSlices.slices.map((slice, i) => ({ slice, i })).reverse();

  const tabs: Array<{ id: Tab; label: string }> = [
    { id: "events", label: t("zconfig.workflowEvents") },
    { id: "artifacts", label: t("zconfig.workflowArtifacts") },
    { id: "workspace", label: t("zconfig.workflowWorkspace") },
  ];

  return (
    <div className="flex h-full flex-col bg-canvas text-ink">
      <header className="flex shrink-0 items-center gap-1 px-3 pb-2 pt-[max(var(--safe-top),0.75rem)]">
        <button
          onClick={() => {
            setOpenArtifact(null);
            setNodeResult(null);
            onBack();
          }}
          aria-label={t("common.close")}
          className="flex size-9 shrink-0 items-center justify-center rounded-full text-dim active:bg-white/[0.06]"
        >
          <ArrowLeft className="size-4.5" />
        </button>
        <span className="min-w-0 flex-1">
          <h1 className="truncate text-base font-semibold">{title}</h1>
          <p className="flex items-center gap-1.5 text-[11px] text-faint">
            <StatusBadge status={summary?.status} />
            <span className="min-w-0 flex-1 truncate">
              {[
                summary?.updatedAt ? fmtStamp(summary.updatedAt) : "",
                summary?.stopReason ?? "",
                summary?.failureMessage ?? "",
              ]
                .filter(Boolean)
                .join(" · ")}
            </span>
          </p>
          {(summary?.resumedFrom || summary?.supersededBy) && (
            <p className="mt-0.5 truncate text-[10px] text-faint">
              {summary?.resumedFrom &&
                ` ${t("zconfig.workflowLineageFrom")} …${summary.resumedFrom.slice(-8)}`}
              {summary?.supersededBy &&
                ` ${t("zconfig.workflowLineageBy")} …${summary.supersededBy.slice(-8)}`}
            </p>
          )}
        </span>
        <div className="flex shrink-0 items-center gap-1.5">
          {active && !sessionDead && (
            <>
              <button
                onClick={() => setAmending(true)}
                aria-label={t("zconfig.workflowConfigure")}
                className="flex size-8 items-center justify-center rounded-full text-dim active:bg-white/[0.06]"
              >
                <SlidersHorizontal className="size-4" />
              </button>
              <button
                onClick={() => void stop()}
                disabled={stopping}
                aria-label={t("zconfig.workflowStop")}
                className="flex size-8 items-center justify-center rounded-full bg-red-500/15 text-red-300 active:bg-red-500/25 disabled:opacity-40"
              >
                <Square className="size-3.5 fill-current" />
              </button>
            </>
          )}
          {summary?.resumable && (
            <button
              onClick={() => void resume()}
              className="rounded-lg bg-sky-500/20 px-2.5 py-1.5 text-[11px] font-medium text-sky-300"
            >
              {t("zconfig.workflowResume")}
            </button>
          )}
        </div>
      </header>

      <div className="flex shrink-0 gap-1 px-4 pb-2">
        {tabs.map(({ id, label }) => (
          <button
            key={id}
            onClick={() => setTab(id)}
            className={`flex-1 rounded-lg px-2 py-1.5 text-xs ${
              tab === id ? "bg-white/[0.1] font-medium text-ink" : "text-dim active:bg-white/[0.05]"
            }`}
          >
            {label}
          </button>
        ))}
      </div>

      <div className="flex-1 overflow-y-auto pb-[max(var(--safe-bottom),1rem)]">
        {sessionDead && (
          <p className="mx-4 mb-2 rounded-xl bg-surface px-4 py-3 text-xs text-amber-300 ring-1 ring-hairline">
            {t("zconfig.workflowSessionLost")}
          </p>
        )}
        {tab === "events" &&
          (events !== null && events.length === 0 ? (
            <ConfigEmpty text={t("zconfig.workflowEventsEmpty")} />
          ) : (
            <div className="px-4">
              {hasMore && events !== null && (
                <button
                  onClick={() => void fetchEvents(false)}
                  disabled={eventsBusy}
                  className="mb-3 w-full rounded-xl bg-raised px-3 py-2.5 text-sm text-dim active:bg-white/[0.07]"
                >
                  {t("zconfig.workflowLoadNewer")}
                </button>
              )}
              {events === null && !sessionDead && !active && (
                <button
                  onClick={() => void fetchEvents(true)}
                  disabled={eventsBusy}
                  className="mb-4 w-full rounded-xl bg-raised px-3 py-2.5 text-sm text-dim active:bg-white/[0.07]"
                >
                  {t("zconfig.workflowRetry")}
                </button>
              )}
              {events === null && !sessionDead && active && (
                <div className="flex justify-center py-8">
                  <RefreshCw className="size-5 animate-spin text-faint" />
                </div>
              )}
              {phaseCards.map(({ slice, i }) => (
                <PhaseSection
                  key={`${i}-${slice.phase ?? "pre"}`}
                  slice={slice}
                  sliceIndex={i}
                  nodeHome={phaseSlices.nodeHome}
                  actorHome={phaseSlices.actorHome}
                  head={i === headSlice}
                  running={active}
                  bare={phaseSlices.slices.length === 1 && slice.phase === null}
                  open={phaseOpen[i] ?? i === headSlice}
                  onToggle={() =>
                    setPhaseOpen((s) => ({ ...s, [i]: !(s[i] ?? i === headSlice) }))
                  }
                />
              ))}
            </div>
          ))}

        {tab === "artifacts" && (
          <>
            {artifacts !== null && artifacts.length === 0 ? (
              <ConfigEmpty text={t("zconfig.workflowArtifactsEmpty")} />
            ) : (
              <ConfigBlock>
                {(artifacts ?? []).map((a) => (
                  <button
                    key={a.id}
                    onClick={() => setOpenArtifact(a)}
                    className="flex w-full items-start gap-3 px-4 py-3 text-left active:bg-white/[0.05]"
                  >
                    <span className="min-w-0 flex-1">
                      <span className="block text-sm text-ink">{a.title ?? a.id}</span>
                      <span className="block text-[11px] text-faint">
                        {a.kind}
                        {a.versions?.length ? ` · v${latestVersion(a)}` : ""}
                        {a.itemCount !== undefined ? ` · ${a.itemCount}` : ""}
                      </span>
                    </span>
                  </button>
                ))}
              </ConfigBlock>
            )}
          </>
        )}

        {tab === "workspace" && (
          <>
            {nodes !== null && nodes.length === 0 ? (
              <ConfigEmpty text={t("zconfig.workflowWorkspaceEmpty")} />
            ) : (
              <ConfigBlock>
                {(nodes ?? []).map((n, i) => {
                  const key = `${n.siteId ?? "?"}:${n.ordinal ?? i}`;
                  const dur = fmtDuration(n.createdAt, n.updatedAt);
                  const exitBad = n.summary?.exitCode !== undefined && n.summary.exitCode !== 0;
                  const ref = [
                    n.siteId !== undefined
                      ? `${n.siteId}${n.ordinal !== undefined ? `@${n.ordinal}` : ""}`
                      : null,
                    n.op ?? null,
                  ]
                    .filter(Boolean)
                    .join(" · ");
                  return (
                    <div key={key} className="px-4 py-2.5">
                      <button
                        onClick={() => void loadNode(key, n)}
                        className="flex w-full items-center gap-2.5 text-left"
                      >
                        <span
                          className={`flex size-7 shrink-0 items-center justify-center rounded-lg font-mono text-[10px] font-semibold uppercase ${opTileClass(n.op)}`}
                        >
                          {(n.op ?? "?").slice(0, 2)}
                        </span>
                        <span className="min-w-0 flex-1">
                          {/* The WHAT (command / path / task) leads; the
                              internal ref and byte counts demote to a meta
                              line — they identify the row, they are not it. */}
                          <span className="block truncate text-xs text-ink">
                            {argsText(n) ?? n.op ?? key}
                          </span>
                          <span
                            className={`block truncate font-mono text-[10px] ${
                              exitBad ? "text-red-300/90" : "text-faint"
                            }`}
                          >
                            {[ref, n.summary ? nodeSummaryText(n.summary) : null]
                              .filter(Boolean)
                              .join(" · ")}
                          </span>
                        </span>
                        {dur && (
                          <span className="shrink-0 text-[10px] tabular-nums text-faint">
                            +{dur}
                          </span>
                        )}
                        <StatusBadge status={n.status} />
                      </button>
                      {nodeResult?.key === key && (
                        <NodeResultView
                          status={nodeResult.status}
                          body={nodeResult.body}
                          truncated={nodeResult.truncated}
                        />
                      )}
                    </div>
                  );
                })}
                {nodes?.length || sessionDead ? null : (
                  <div className="flex justify-center py-8">
                    <RefreshCw className="size-5 animate-spin text-faint" />
                  </div>
                )}
              </ConfigBlock>
            )}
          </>
        )}
      </div>

      {openArtifact && (
        <ArtifactViewer
          instanceId={instanceId}
          sessionId={sessionId}
          runId={runId}
          artifact={openArtifact}
          onBack={() => setOpenArtifact(null)}
        />
      )}
      {amending && (
        <AmendSheet
          onClose={() => setAmending(false)}
          onAmend={async (body) => {
            setAmending(false);
            const res = await amendWorkflowRun({ runId, sessionId, body, instanceId });
            if (!res) return;
            // A supersede answers with the NEW run's id — re-point the whole
            // view so the poller and every tab follow the continuation.
            if (res.runId !== runId) onRepoint(res.runId);
            else void refreshSummary();
          }}
        />
      )}
    </div>
  );

  async function loadNode(key: string, n: WorkflowWorkspaceNode) {
    if (nodeResult?.key === key) {
      setNodeResult(null);
      return;
    }
    if (n.siteId === undefined || n.ordinal === undefined) return;
    const res = await workflowAction("load node result", (c) =>
      c.runNodeResult(instanceId, sessionId, runId, n.siteId!, n.ordinal!),
    );
    if (!res) return;
    setNodeResult({
      key,
      status: res.status,
      body: res.error !== undefined && res.error !== null ? res.error : res.result,
      truncated: res.truncated,
    });
  }
}

/**
 * Submit-only settings amendment (bridge 0.49.0): the run summary carries no
 * current values until the upstream schema extension lands, so nothing is
 * prefilled. Three-state per field: empty = keep, "clear" checked = send
 * `null` (revert to default), value = set. The submit is disabled until at
 * least one field contributes a change (an empty delta is a 422 by contract).
 */
function AmendSheet({
  onClose,
  onAmend,
}: {
  onClose: () => void;
  onAmend: (body: {
    subagentModel?: string | null;
    maxConcurrency?: number | null;
  }) => Promise<void>;
}) {
  const { t } = useTranslation();
  const [model, setModel] = useState("");
  const [clearModel, setClearModel] = useState(false);
  const [concurrency, setConcurrency] = useState("");
  const [clearConcurrency, setClearConcurrency] = useState(false);
  const [busy, setBusy] = useState(false);

  const modelValue = clearModel ? null : model.trim() || undefined;
  const parsedConcurrency = concurrency.trim();
  const concurrencyNumber =
    !clearConcurrency && parsedConcurrency ? Number(parsedConcurrency) : undefined;
  const concurrencyValue: number | null | undefined = clearConcurrency
    ? null
    : concurrencyNumber !== undefined &&
        Number.isInteger(concurrencyNumber) &&
        concurrencyNumber >= 1
      ? concurrencyNumber
      : undefined;
  const concurrencyInvalid =
    !clearConcurrency && parsedConcurrency !== "" && concurrencyValue === undefined;
  const empty = modelValue === undefined && concurrencyValue === undefined && !clearConcurrency;

  return (
    <ConfigFormSheet
      title={t("zconfig.workflowConfigure")}
      submitDisabled={busy || empty || concurrencyInvalid}
      onClose={onClose}
      onSubmit={() => {
        setBusy(true);
        void onAmend({
          ...(modelValue !== undefined ? { subagentModel: modelValue } : {}),
          ...(concurrencyValue !== undefined || clearConcurrency
            ? { maxConcurrency: clearConcurrency ? null : concurrencyValue }
            : {}),
        });
      }}
    >
      <ConfigField
        label={t("zconfig.workflowSubagentModel")}
        hint={t("zconfig.workflowSubagentModelHint")}
      >
        <input
          value={model}
          onChange={(e) => {
            setModel(e.target.value);
            if (e.target.value) setClearModel(false);
          }}
          disabled={clearModel}
          className={`${configInputClass} font-mono`}
          placeholder="zhipu/glm-4.7"
        />
        <label className="mt-1.5 flex items-center gap-2 text-[11px] text-dim">
          <input
            type="checkbox"
            checked={clearModel}
            onChange={(e) => {
              setClearModel(e.target.checked);
              if (e.target.checked) setModel("");
            }}
            className="size-3.5 accent-white"
          />
          {t("zconfig.workflowClearModel")}
        </label>
      </ConfigField>
      <ConfigField
        label={t("zconfig.workflowMaxConcurrency")}
        hint={
          concurrencyInvalid
            ? t("zconfig.workflowConcurrencyInvalid")
            : t("zconfig.workflowConcurrencyHint")
        }
      >
        <input
          value={concurrency}
          onChange={(e) => {
            setConcurrency(e.target.value);
            if (e.target.value) setClearConcurrency(false);
          }}
          disabled={clearConcurrency}
          inputMode="numeric"
          className={configInputClass}
          placeholder="4"
        />
        <label className="mt-1.5 flex items-center gap-2 text-[11px] text-dim">
          <input
            type="checkbox"
            checked={clearConcurrency}
            onChange={(e) => {
              setClearConcurrency(e.target.checked);
              if (e.target.checked) setConcurrency("");
            }}
            className="size-3.5 accent-white"
          />
          {t("zconfig.workflowClearConcurrency")}
        </label>
      </ConfigField>
      <p className="px-1 text-[10px] leading-relaxed text-faint">
        {t("zconfig.workflowAmendHint")}
      </p>
    </ConfigFormSheet>
  );
}

/**
 * One step (a dispatched node) folded to a line: state dot + name +
 * task/activity + outcome word. Tapping opens the step's own detail — the
 * instruction head, the error, and its journal rows newest-first — so the
 * phase list stays a list of steps rather than a dump of every row.
 */
function StepRow({ n }: { n: NodeAgg }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const dot =
    n.state === "ok"
      ? "bg-emerald-400"
      : n.state === "failed"
        ? "bg-red-400"
        : n.state === "cancelled"
          ? "bg-white/35"
          : n.state === "waiting" || n.state === "repairing"
            ? "bg-amber-400"
            : "animate-pulse bg-sky-400";
  const word = nodeStateWord(t, n.state);
  const wordCls =
    n.state === "failed" ? "text-red-300" : n.state === "ok" ? "text-emerald-300/80" : "text-faint";
  const title = n.label ?? `${n.site || "?"}${n.ordinal !== undefined ? `@${n.ordinal}` : ""}`;
  const sub = n.activity ?? n.head;
  const rows = open ? [...n.events].sort(newestFirst) : [];
  return (
    <div className="py-1">
      <button onClick={() => setOpen(!open)} className="flex w-full items-center gap-2 text-left">
        <span className={`size-1.5 shrink-0 rounded-full ${dot}`} />
        <span className="min-w-0 flex-1 truncate text-[11px] text-dim">
          {title}
          {sub ? <span className="text-faint"> — {sub}</span> : null}
        </span>
        {word !== null && <span className={`shrink-0 text-[10px] ${wordCls}`}>{word}</span>}
        <ChevronRight
          className={`size-3 shrink-0 text-faint transition-transform ${open ? "rotate-90" : ""}`}
        />
      </button>
      {!open && n.state === "failed" && n.error ? (
        <p className="mt-0.5 truncate pl-3.5 text-[10px] text-red-300/90">{n.error}</p>
      ) : null}
      {open && (
        <div className="ml-[3px] mt-1 border-l border-hairline pl-3">
          {n.head ? (
            <p className="break-words pb-1 text-[11px] leading-snug text-dim">{n.head}</p>
          ) : null}
          {n.error ? (
            <p className="break-words pb-1 text-[11px] leading-snug text-red-300/90">{n.error}</p>
          ) : null}
          {rows.map((ev) => (
            <EventRow key={ev.sequence} ev={ev} omitSite />
          ))}
        </div>
      )}
    </div>
  );
}

/**
 * One phase card. The header carries the phase's state (live while it is the
 * run's head, done or failed otherwise) and folds the body; only the newest
 * phase opens by default. The body reads newest-first — the settlement, then
 * the phase's journal rows, then its steps (each a fold of its own), with the
 * hiring/usage footer last — so the latest message is the first thing seen.
 * Nodes/actors are filtered to the ones this slice OWNS (their last/first
 * event fell here) — see slicePhases for why replayed prefixes must not
 * double-render.
 */
function PhaseSection({
  slice,
  sliceIndex,
  nodeHome,
  actorHome,
  head,
  running,
  bare,
  open,
  onToggle,
}: {
  slice: PhaseSlice;
  sliceIndex: number;
  nodeHome: Map<string, number>;
  actorHome: Map<string, number>;
  /** The newest slice — where the run currently stands. */
  head: boolean;
  running: boolean;
  /** The only slice and it has no phase: no card chrome, just the rows. */
  bare: boolean;
  open: boolean;
  onToggle: () => void;
}) {
  const { t } = useTranslation();
  const fold = useMemo(() => reduceSlice(t, slice.events), [t, slice]);
  const { actors, nodes, settled, spentTokens } = fold;
  const isOwn = (key: string) => nodeHome.get(key) === sliceIndex;
  const ownNodes = nodes.filter((n) => isOwn(n.key));
  const ownActors = actors.filter((a) => actorHome.get(a.key) === sliceIndex);
  const journal = phaseRows(fold, isOwn);
  if (
    ownActors.length === 0 &&
    ownNodes.length === 0 &&
    journal.length === 0 &&
    settled === null &&
    spentTokens === null
  )
    return null;
  const failed =
    ownNodes.some((n) => n.state === "failed") ||
    (settled !== null && describeRunEvent(t, settled).tone === "failed");
  const dot = failed
    ? "bg-red-400"
    : head && running
      ? "animate-pulse bg-sky-400"
      : "bg-emerald-400/80";
  const footer = [
    ownActors.length > 0
      ? `${t("zconfig.wfEvActorCreated")} · ${ownActors.map((a) => a.label).join(" · ")}`
      : null,
    spentTokens !== null
      ? `${t("zconfig.wfEvUsageUpdated")} · ${spentTokens.toLocaleString()} tokens`
      : null,
  ].filter((v): v is string => v !== null);
  const body = (
    <>
      {settled !== null && <EventRow ev={settled} />}
      {journal.map((ev) => (
        <EventRow key={ev.sequence} ev={ev} />
      ))}
      {ownNodes.map((n) => (
        <StepRow key={n.key} n={n} />
      ))}
      {footer.map((line) => (
        <p key={line} className="truncate py-1 text-[10px] tabular-nums text-faint">
          {line}
        </p>
      ))}
    </>
  );
  if (bare) return <section className="mb-3">{body}</section>;
  return (
    <section className="mb-2 rounded-xl bg-surface ring-1 ring-hairline">
      <button onClick={onToggle} className="flex w-full items-center gap-2 px-3 py-2 text-left">
        <span className={`size-2 shrink-0 rounded-full ${dot}`} />
        <span className="min-w-0 flex-1 truncate font-mono text-xs font-medium text-ink">
          {slice.phase ?? t("zconfig.workflowPhasePrelude")}
          {slice.ordinal !== undefined && slice.ordinal >= 2 ? (
            <span className="font-normal text-faint"> ×{slice.ordinal}</span>
          ) : null}
        </span>
        {ownNodes.length > 0 && (
          <span className="shrink-0 text-[10px] text-faint">
            {t("zconfig.workflowPhaseNodes", { n: ownNodes.length })}
          </span>
        )}
        <ChevronRight
          className={`size-3.5 shrink-0 text-faint transition-transform ${open ? "rotate-90" : ""}`}
        />
      </button>
      {open && <div className="px-3 pb-2">{body}</div>}
    </section>
  );
}

/**
 * One journal row: a readable label + detail line (describeRunEvent), payload
 * reachable behind an explicit raw toggle. Sequence numbers stay out of the
 * default view — they are journal coordinates, not information. `omitSite`
 * drops the instance ref for rows already nested under their step.
 */
function EventRow({ ev, omitSite }: { ev: WorkflowRunEvent; omitSite?: boolean }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const desc = describeRunEvent(t, ev, omitSite ? { omitSite } : undefined);
  return (
    <div className="border-b border-hairline py-2 last:border-0">
      <button onClick={() => setOpen(!open)} className="flex w-full items-baseline gap-2 text-left">
        <span
          className={`max-w-[45%] shrink-0 truncate text-xs ${
            desc.monoLabel
              ? "font-mono text-faint"
              : desc.tone === "failed"
                ? "text-red-300"
                : "text-dim"
          }`}
        >
          {desc.label}
        </span>
        {desc.detail ? (
          <span className="min-w-0 flex-1 truncate text-[11px] text-faint">{desc.detail}</span>
        ) : null}
      </button>
      {open && <RawData value={ev.payload} />}
    </div>
  );
}

/**
 * A workspace node's expanded body, structured before raw: strings render as
 * text, world.run-shaped results split into exit code + output streams, and
 * plain objects become key-value rows. The full JSON stays one toggle away.
 */
function NodeResultView({
  status,
  body,
  truncated,
}: {
  status?: string;
  body: unknown;
  truncated?: boolean;
}) {
  const { t } = useTranslation();
  const isRecord = typeof body === "object" && body !== null && !Array.isArray(body);
  const rec = isRecord ? (body as Record<string, unknown>) : null;
  const run =
    rec !== null && ("stdout" in rec || "stderr" in rec || "exitCode" in rec) ? rec : null;
  const stdout = run !== null && typeof run.stdout === "string" ? run.stdout : null;
  const stderr = run !== null && typeof run.stderr === "string" ? run.stderr : null;
  const exitCode = run !== null && typeof run.exitCode === "number" ? run.exitCode : null;
  const rest =
    run !== null
      ? Object.entries(run).filter(([k]) => !["stdout", "stderr", "exitCode"].includes(k))
      : rec !== null
        ? Object.entries(rec)
        : [];
  return (
    <div className="mt-2 rounded-xl bg-raised p-3 ring-1 ring-hairline">
      {/* v4 node status enum: running | completed | failed — red is for
          failed only; anything else (incl. unknown tokens) stays quiet. */}
      {status === "failed" ? <p className="mb-1.5 text-[11px] text-red-300">{status}</p> : null}
      {typeof body === "string" ? (
        <pre className="max-h-64 overflow-auto whitespace-pre-wrap break-all font-mono text-[11px] leading-relaxed text-dim">
          {body}
        </pre>
      ) : run !== null ? (
        <>
          {exitCode !== null && (
            <p
              className={`mb-1.5 font-mono text-[11px] ${
                exitCode !== 0 ? "text-red-300" : "text-faint"
              }`}
            >
              exit {exitCode}
            </p>
          )}
          {stdout && (
            <pre className="max-h-40 overflow-auto whitespace-pre-wrap break-all font-mono text-[11px] leading-relaxed text-dim">
              {stdout}
            </pre>
          )}
          {stderr && (
            <pre className="mt-1 max-h-24 overflow-auto whitespace-pre-wrap break-all font-mono text-[11px] leading-relaxed text-red-300/90">
              {stderr}
            </pre>
          )}
        </>
      ) : rec !== null ? (
        <div className="flex flex-col gap-0.5">
          {rest.map(([k, v]) => (
            <p key={k} className="flex items-baseline justify-between gap-3">
              <span className="min-w-0 shrink-0 truncate text-[11px] text-faint">{k}</span>
              <span className="min-w-0 break-all text-right font-mono text-[11px] text-dim">
                {cellText(v)}
              </span>
            </p>
          ))}
        </div>
      ) : (
        <p className="break-all font-mono text-[11px] text-dim">{cellText(body)}</p>
      )}
      {truncated && <p className="mt-1 text-[10px] text-faint">{t("zconfig.workflowTruncated")}</p>}
      <RawData value={body} />
    </div>
  );
}

/** Read-only full-screen shell for one artifact (back gesture included). */
function ViewerShell({
  title,
  onBack,
  children,
}: {
  title: string;
  onBack: () => void;
  children: ReactNode;
}) {
  const { t } = useTranslation();
  useBackHandler(() => {
    onBack();
    return true;
  });
  return (
    <div className="fixed inset-0 z-50 flex flex-col bg-canvas text-ink">
      <header className="flex shrink-0 items-center gap-1 px-3 pb-2 pt-[max(var(--safe-top),0.75rem)]">
        <button
          onClick={onBack}
          aria-label={t("common.close")}
          className="flex size-9 shrink-0 items-center justify-center rounded-full text-dim active:bg-white/[0.06]"
        >
          <ArrowLeft className="size-4.5" />
        </button>
        <h1 className="min-w-0 flex-1 truncate text-base font-semibold">{title}</h1>
      </header>
      <div className="flex-1 overflow-y-auto px-4 pb-[max(var(--safe-bottom),1rem)]">
        {children}
      </div>
    </div>
  );
}

/**
 * One artifact, by kind: markdown renders (the shared renderer), text files
 * stream in chunks, and the four preset kinds draw their spec-declared views
 * (WorkflowArtifactViews), degrading to key-value cards when a spec cannot
 * be parsed. Unknown kinds show a metadata card; raw JSON is always one
 * toggle away, never the default view.
 */
function ArtifactViewer({
  instanceId,
  sessionId,
  runId,
  artifact,
  onBack,
}: {
  instanceId: string;
  sessionId: string;
  runId: string;
  artifact: WorkflowArtifact;
  onBack: () => void;
}) {
  const { t } = useTranslation();
  const workflowAction = useAppStore((s) => s.workflowAction);
  const [busy, setBusy] = useState(true);
  const [markdown, setMarkdown] = useState<string | null>(null);
  const [text, setText] = useState<string | null>(null);
  const [binary, setBinary] = useState(false);
  const [totalBytes, setTotalBytes] = useState<number | null>(null);
  const [nextOffset, setNextOffset] = useState<number | null>(null);
  // Board/table/metrics share one paginated data channel (`artifactData`):
  // raw items stay structured so each kind renders its own view.
  const [dataItems, setDataItems] = useState<Array<{
    sequence: number;
    item: unknown;
  }> | null>(null);
  const [dataHasMore, setDataHasMore] = useState(false);
  const [dataCursor, setDataCursor] = useState<number | null>(null);
  // One decoder per artifact, in streaming mode: a chunk boundary can split
  // a multi-byte UTF-8 character, and per-chunk decoding would turn each
  // split into U+FFFD on both sides.
  const decoderRef = useRef<TextDecoder | null>(null);

  const kind = artifact.kind;

  const loadChunk = useCallback(
    async (offset: number, append: boolean) => {
      setBusy(true);
      const res = await workflowAction("read artifact", (c) =>
        c.runArtifactRead(
          instanceId,
          sessionId,
          runId,
          artifact.id,
          latestVersion(artifact),
          offset,
          READ_CHUNK,
        ),
      );
      setBusy(false);
      if (!res) return;
      setTotalBytes(res.totalBytes ?? null);
      setNextOffset(res.nextOffset ?? null);
      try {
        const bin = atob(res.dataBase64);
        const bytes = new Uint8Array(bin.length);
        for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
        decoderRef.current ??= new TextDecoder("utf-8");
        const chunk = decoderRef.current.decode(bytes, { stream: true });
        setText((prev) => (append ? (prev ?? "") + chunk : chunk));
      } catch {
        setBinary(true);
        setText(null);
      }
    },
    [workflowAction, instanceId, sessionId, runId, artifact],
  );

  // The binary verdict reads the WHOLE accumulated text once the chunks
  // stop coming (a split codepoint mid-stream must not read as binary).
  useEffect(() => {
    if (nextOffset !== null || text === null) return;
    let bad = 0;
    for (let i = 0; i < text.length; i++) if (text.charCodeAt(i) === 0xfffd) bad++;
    if (text.length > 0 && bad / text.length > 0.01) setBinary(true);
  }, [nextOffset, text]);

  const loadData = useCallback(
    async (initial: boolean) => {
      setBusy(true);
      const after = initial ? undefined : (dataCursor ?? undefined);
      const res = await workflowAction("load artifact items", (c) =>
        c.runArtifactData(instanceId, sessionId, runId, artifact.id, after),
      );
      setBusy(false);
      if (!res) return;
      const lastSeq = res.items[res.items.length - 1]?.sequence;
      if (lastSeq !== undefined) setDataCursor(lastSeq);
      setDataItems((prev) => {
        const mapped = res.items.map((it) => ({
          sequence: it.sequence ?? 0,
          item: it.item,
        }));
        if (initial || prev === null) return mapped;
        // Same defensive dedupe as the journal poll: an unadvancing cursor
        // answers with the same page — keep only items past what we hold.
        const floor = prev.reduce((m, it) => Math.max(m, it.sequence), 0);
        return [...prev, ...mapped.filter((it) => it.sequence > floor)];
      });
      setDataHasMore(res.hasMore === true);
    },
    [workflowAction, instanceId, sessionId, runId, artifact, dataCursor],
  );

  useEffect(() => {
    if (kind === "markdown" || kind === "file") {
      void loadChunk(0, false);
    } else if (kind === "board" || kind === "table" || kind === "metrics" || kind === "chart") {
      void loadData(true);
    } else {
      setBusy(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // markdown: decode finished when text landed
  useEffect(() => {
    if (kind === "markdown" && text !== null && !binary) setMarkdown(text);
  }, [kind, text, binary]);

  // Preset kinds carry their declaration spec on the artifact (passed through
  // untouched by the bridge); every renderer degrades to KeyValueCards when
  // it cannot parse it, and the raw items stay behind one toggle.
  const spec = (artifact as { spec?: unknown }).spec;
  const isPreset = kind === "board" || kind === "table" || kind === "metrics" || kind === "chart";

  return (
    <ViewerShell title={artifact.title ?? artifact.id} onBack={onBack}>
      {busy && text === null && markdown === null && dataItems === null ? (
        <div className="flex justify-center py-10">
          <RefreshCw className="size-5 animate-spin text-faint" />
        </div>
      ) : kind === "markdown" && markdown !== null ? (
        <div className="pt-2">
          <MarkdownText text={markdown} />
        </div>
      ) : kind === "file" ? (
        <>
          {binary ? (
            <p className="py-8 text-center text-sm text-faint">
              {t("zconfig.workflowBinary", {
                bytes: totalBytes ?? text?.length ?? 0,
              })}
            </p>
          ) : (
            <pre className="whitespace-pre-wrap break-all pt-2 font-mono text-[11px] leading-relaxed text-dim">
              {text}
            </pre>
          )}
          {nextOffset !== null && (
            <button
              onClick={() => void loadChunk(nextOffset, true)}
              disabled={busy}
              className="mt-4 w-full rounded-xl bg-raised px-3 py-2.5 text-sm text-dim active:bg-white/[0.07]"
            >
              {t("zconfig.workflowLoadMore")}
            </button>
          )}
        </>
      ) : kind === "table" ? (
        <TableView items={dataItems} spec={spec} />
      ) : kind === "metrics" ? (
        <MetricsTiles items={dataItems} spec={spec} />
      ) : kind === "board" ? (
        <BoardView items={dataItems} spec={spec} />
      ) : kind === "chart" ? (
        <ChartView items={dataItems} spec={spec} />
      ) : (
        <>
          <ArtifactMetaCard artifact={artifact} />
          <RawData value={artifact} />
        </>
      )}
      {isPreset && <RawData value={(dataItems ?? []).map((it) => it.item)} />}
      {isPreset && dataHasMore && (
        <button
          onClick={() => void loadData(false)}
          disabled={busy}
          className="mt-4 w-full rounded-xl bg-raised px-3 py-2.5 text-sm text-dim active:bg-white/[0.07]"
        >
          {t("zconfig.workflowLoadMore")}
        </button>
      )}
    </ViewerShell>
  );
}
