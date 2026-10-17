import type { WorkflowRunEvent } from "../../lib/types";

/**
 * Human-readable rendering for one dynamic-workflow journal event.
 *
 * The v4 journal contract deliberately does not restate the engine's payload
 * shapes ("readers interpret per event kind", upstream transport.ts), so the
 * app owes every event kind a line of its own. The vocabulary mirrors the
 * desktop run panel (workflowRunPanel.ts): a localized label plus a detail
 * that carries identity and data (site refs, actor names, message previews)
 * and never needs localization. Unknown kinds degrade to the raw type name
 * with a short payload preview — readable first, raw JSON only in the row
 * expander. Every payload read is defensive: a journal that arrives clipped
 * or from a newer engine must never take the page down.
 */

/** One-line detail cap — the row expander carries the full payload. */
const DETAIL_MAX = 200;

export interface RunEventDesc {
  /** Localized main label ("任务完成 · 成功"). */
  label: string;
  /** Identity + data (site@ordinal, actor name, message preview). */
  detail?: string;
  /** Failure/cancel tint; everything else stays neutral. */
  tone: "default" | "failed";
  /** Set for unknown kinds: the UI renders the label in monospace. */
  monoLabel?: boolean;
}

export type Translate = (key: string, values?: Record<string, unknown>) => string;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function str(p: Record<string, unknown>, k: string): string | undefined {
  const v = p[k];
  return typeof v === "string" && v.length > 0 ? v : undefined;
}

function num(p: Record<string, unknown>, k: string): number | undefined {
  const v = p[k];
  return typeof v === "number" ? v : undefined;
}

/**
 * `{siteId, ordinal}` → `site@ordinal` — the engine's own refToString form,
 * so a row can be cross-read against backend logs.
 */
export function refText(value: unknown): string | undefined {
  if (!isRecord(value)) return undefined;
  const site = typeof value.siteId === "string" ? value.siteId : undefined;
  if (!site) return undefined;
  const ord = typeof value.ordinal === "number" ? value.ordinal : undefined;
  return ord !== undefined ? `${site}@${ord}` : site;
}

/** Stable fold key for a node-* event's instance ref. */
export function instanceKeyOf(p: Record<string, unknown>): string {
  return refText(p.instance) ?? "?";
}

/** Stable fold key for an actor across replayed prefixes (resume re-hires). */
export function actorKeyOf(p: Record<string, unknown>): string {
  return refText(p.actor) ?? `name:${str(p, "name") ?? "?"}`;
}

function joinDetail(...parts: Array<string | undefined>): string | undefined {
  const kept = parts.filter((v): v is string => v !== undefined && v.length > 0);
  return kept.length > 0 ? kept.join(" · ") : undefined;
}

function truncate(text: string, max: number): string {
  return text.length <= max ? text : `${text.slice(0, max - 1)}…`;
}

function messageOf(value: unknown): string | undefined {
  return isRecord(value) ? str(value, "message") : undefined;
}

/** Compact one-line preview of a report item / unknown payload. */
export function itemSummary(item: unknown): string | undefined {
  if (item === undefined || item === null) return undefined;
  if (typeof item === "string") {
    return item.length > 0 ? truncate(item, DETAIL_MAX) : undefined;
  }
  try {
    const s = JSON.stringify(item);
    return s === undefined ? undefined : truncate(s, DETAIL_MAX);
  } catch {
    return undefined;
  }
}

export function outcomeWord(t: Translate, outcome: unknown): string {
  if (outcome === "ok") return t("zconfig.wfOutcomeOk");
  if (outcome === "failed") return t("zconfig.wfOutcomeFailed");
  if (outcome === "cancelled") return t("zconfig.wfOutcomeCancelled");
  return String(outcome ?? "?");
}

/**
 * Status token → localized word; unknown tokens stay verbatim. The catalog
 * keys are FLAT (`wfStatusRunning`), so a dot in the key would make i18next
 * resolve a nested object that does not exist and fall back forever.
 */
export function statusWord(t: Translate, token: string | undefined): string {
  if (!token) return "?";
  const key = "zconfig.wfStatus" + token.charAt(0).toUpperCase() + token.slice(1);
  return t(key, { defaultValue: token });
}

/** Short qid tag — the tail is the only part that tells two asks apart. */
function qidTag(p: Record<string, unknown>): string | undefined {
  const qid = str(p, "qid");
  return qid ? `…${qid.slice(-6)}` : undefined;
}

export interface DescribeOptions {
  /**
   * Drop the `site@ordinal` ref from the detail — rows nested under their own
   * step already say which instance they belong to.
   */
  omitSite?: boolean;
}

/**
 * One journal event → one readable line. The field names per kind come from
 * the engine's RunEvent union (dynamic-workflow/src/engine/types.ts).
 */
export function describeRunEvent(
  t: Translate,
  ev: WorkflowRunEvent,
  opts?: DescribeOptions,
): RunEventDesc {
  const p = ev.payload ?? {};
  const site = opts?.omitSite ? undefined : refText(p.instance);
  switch (ev.type) {
    case "run-started":
      return { label: t("zconfig.wfEvRunStarted"), tone: "default" };

    case "actor-created":
      return {
        label: t("zconfig.wfEvActorCreated"),
        detail: joinDetail(str(p, "name"), refText(p.actor)),
        tone: "default",
      };

    case "node-queued":
      return {
        label: t("zconfig.wfEvNodeQueued"),
        detail: joinDetail(str(p, "actorName"), site, str(p, "kind")),
        tone: "default",
      };

    case "node-dispatched":
      return {
        label: t("zconfig.wfEvNodeDispatched"),
        detail: joinDetail(str(p, "actorName"), site),
        tone: "default",
      };

    case "node-executing":
      return {
        label: t("zconfig.wfEvNodeExecuting"),
        detail: site,
        tone: "default",
      };

    case "node-waiting":
      return {
        label:
          str(p, "cause") === "backoff"
            ? t("zconfig.wfEvNodeWaitingBackoff", {
                reason: str(p, "reason") ?? "?",
              })
            : t("zconfig.wfEvNodeWaiting"),
        detail: site,
        tone: "default",
      };

    case "node-nudged":
      return {
        label: t("zconfig.wfEvNodeNudged"),
        detail: site,
        tone: "default",
      };

    case "node-repairing":
      return {
        label: t("zconfig.wfEvNodeRepairing", { n: num(p, "attempt") ?? "?" }),
        detail: site,
        tone: "default",
      };

    case "node-progress": {
      const tool = isRecord(p.lastTool)
        ? joinDetail(
            str(p.lastTool, "name"),
            str(p.lastTool, "target") === undefined
              ? undefined
              : truncate(String(p.lastTool.target), 80),
          )
        : undefined;
      return {
        label: t("zconfig.wfEvNodeProgress", { turn: num(p, "turn") ?? "?" }),
        detail: joinDetail(site, tool),
        tone: "default",
      };
    }

    case "node-settled": {
      const outcome =
        p.outcome === "ok" || p.outcome === "failed" || p.outcome === "cancelled"
          ? p.outcome
          : undefined;
      return {
        label: t(p.cached === true ? "zconfig.wfEvNodeSettledCached" : "zconfig.wfEvNodeSettled", {
          outcome: outcome ? outcomeWord(t, outcome) : "?",
        }),
        detail: joinDetail(site, messageOf(p.error)),
        tone: outcome === "failed" || outcome === "cancelled" ? "failed" : "default",
      };
    }

    case "usage-updated": {
      const spent = num(p, "spentTokens");
      return {
        label: t("zconfig.wfEvUsageUpdated"),
        detail: spent !== undefined ? `${spent.toLocaleString()} tokens` : undefined,
        tone: "default",
      };
    }

    case "log": {
      const message = str(p, "message");
      return {
        label: t("zconfig.wfEvLog"),
        detail: message === undefined ? undefined : truncate(message, DETAIL_MAX),
        tone: "default",
      };
    }

    case "phase-entered": {
      const name = str(p, "name");
      const ordinal = num(p, "ordinal");
      return {
        label: t("zconfig.wfEvPhaseEntered"),
        detail: joinDetail(name, ordinal !== undefined && ordinal >= 2 ? `${ordinal}` : undefined),
        tone: "default",
      };
    }

    case "report":
      return {
        label: t("zconfig.wfEvReport"),
        detail: joinDetail(site, itemSummary(p.item)),
        tone: "default",
      };

    case "artifact-published": {
      const art = isRecord(p.artifact) ? p.artifact : undefined;
      const version =
        art !== undefined && typeof art.version === "number" ? `v${art.version}` : undefined;
      return {
        label: t("zconfig.wfEvArtifactPublished"),
        detail: art !== undefined ? joinDetail(str(art, "id"), version) : str(p, "id"),
        tone: "default",
      };
    }

    case "artifact-failed":
      return {
        label: t("zconfig.wfEvArtifactFailed"),
        detail: joinDetail(str(p, "id"), messageOf(p.error)),
        tone: "failed",
      };

    case "compaction":
      return {
        label: t("zconfig.wfEvCompaction"),
        detail: refText(p.actor),
        tone: "default",
      };

    case "import-cache-closed":
      return {
        label: t("zconfig.wfEvImportCacheClosed"),
        detail: joinDetail(str(p, "actorName"), site),
        tone: "default",
      };

    case "escalation-raised": {
      const question = str(p, "question");
      return {
        label: t("zconfig.wfEvEscalationRaised"),
        detail: joinDetail(
          qidTag(p),
          str(p, "actorName") ?? refText(p.actor),
          question === undefined ? undefined : truncate(question, DETAIL_MAX),
        ),
        tone: "default",
      };
    }

    case "escalation-resolved": {
      const answer = str(p, "answer");
      return {
        label: t("zconfig.wfEvEscalationResolved"),
        detail: joinDetail(
          qidTag(p),
          answer === undefined ? undefined : truncate(answer, DETAIL_MAX),
        ),
        tone: "default",
      };
    }

    case "concurrency-changed":
      return {
        label: t("zconfig.wfEvConcurrencyChanged"),
        detail: joinDetail(
          str(p, "key"),
          [num(p, "previous"), num(p, "next")]
            .map((v) => (v === undefined ? "?" : String(v)))
            .join(" → "),
          str(p, "reason"),
        ),
        tone: "default",
      };

    case "run-caps-changed": {
      const cap = (v: unknown): string | undefined =>
        isRecord(v) && typeof v.maxConcurrency === "number" ? String(v.maxConcurrency) : undefined;
      const prev = cap(p.previous);
      const next = cap(p.caps);
      return {
        label: t("zconfig.wfEvRunCapsChanged"),
        detail: prev !== undefined && next !== undefined ? `${prev} → ${next}` : next,
        tone: "default",
      };
    }

    case "run-stalled":
      return {
        label: t("zconfig.wfEvRunStalled"),
        detail: itemSummary(p),
        tone: "default",
      };

    case "run-settled": {
      const status = str(p, "status");
      const supersededBy = str(p, "supersededBy");
      return {
        label: t("zconfig.wfEvRunSettled", {
          status: status ? statusWord(t, status) : "?",
        }),
        detail: joinDetail(
          str(p, "stopReason"),
          messageOf(p.error),
          supersededBy ? `→ …${supersededBy.slice(-8)}` : undefined,
        ),
        tone: status === "errored" || status === "stopped" ? "failed" : "default",
      };
    }

    default:
      return {
        label: ev.type,
        detail: itemSummary(p),
        tone: "default",
        monoLabel: true,
      };
  }
}

// ─── Slice folding (the progress tab's reducer) ─────────────────────────────

export type NodeState =
  "queued" | "executing" | "waiting" | "repairing" | "ok" | "failed" | "cancelled";

/** Settled outcomes are final — replayed lifecycle noise must not reopen one. */
const SETTLED: ReadonlySet<NodeState> = new Set(["ok", "failed", "cancelled"]);

/** A node's folded lifecycle: one line per instance, LATEST state wins. */
export interface NodeAgg {
  key: string;
  site: string;
  ordinal?: number;
  state: NodeState;
  /** Subagent display name (node-dispatched carries actorName). */
  label?: string;
  /** Author instruction head (node-queued / node-dispatched carry it). */
  head?: string;
  /** Latest observed activity (progress tool, wait cause, repair attempt). */
  activity?: string;
  error?: string;
  /**
   * The step's own journal rows (its lifecycle plus the reports it filed),
   * oldest first — the detail a collapsed step row expands into.
   */
  events: WorkflowRunEvent[];
}

export function nodeStateWord(t: Translate, state: NodeState): string | null {
  switch (state) {
    case "ok":
      return t("zconfig.wfOutcomeOk");
    case "failed":
      return t("zconfig.wfOutcomeFailed");
    case "cancelled":
      return t("zconfig.wfOutcomeCancelled");
    case "queued":
      return t("zconfig.wfNodeStateQueued");
    case "waiting":
      return t("zconfig.wfNodeStateWaiting");
    case "repairing":
      return t("zconfig.wfNodeStateRepairing");
    default:
      return null; // executing: the pulse dot already says "live"
  }
}

/**
 * Fold one slice's events (the span between two phase-entered markers). A
 * node's identity is its `instance` ref (`{siteId, ordinal}` — the engine's
 * own InstanceRef), NOT a top-level siteId field: reading the wrong spelling
 * collapses every node onto one empty key. node-* lifecycles fold per
 * instance into a single line (latest state; settled is final) and keep their
 * own rows for the step's expandable detail, reports and cache closes filed
 * by a known instance nest under that step, actor-created becomes a hiring
 * line, usage updates collapse to the newest total, run-settled closes the
 * section, and the rest stay as phase-level journal rows for describeRunEvent.
 */
export function reduceSlice(t: Translate, events: WorkflowRunEvent[]) {
  const nodes = new Map<string, NodeAgg>();
  const actors: Array<{ key: string; label: string }> = [];
  const others: WorkflowRunEvent[] = [];
  let settled: WorkflowRunEvent | null = null;
  let spentTokens: number | null = null;
  for (const ev of events) {
    const p = ev.payload ?? {};
    if (ev.type === "actor-created") {
      actors.push({
        key: actorKeyOf(p),
        label: str(p, "name") ?? refText(p.actor) ?? "?",
      });
      continue;
    }
    if (ev.type === "run-settled") {
      settled = ev;
      continue;
    }
    if (ev.type === "usage-updated") {
      if (typeof p.spentTokens === "number") spentTokens = p.spentTokens;
      continue;
    }
    if (ev.type.startsWith("node-")) {
      const key = instanceKeyOf(p);
      const ref =
        typeof p.instance === "object" && p.instance !== null
          ? (p.instance as Record<string, unknown>)
          : {};
      const prev = nodes.get(key);
      if (prev && SETTLED.has(prev.state)) continue;
      let state: NodeState = prev?.state ?? "queued";
      let label = prev?.label;
      let head = prev?.head;
      let activity = prev?.activity;
      let error = prev?.error;
      const own = prev?.events ?? [];
      own.push(ev);
      switch (ev.type) {
        case "node-queued":
          state = "queued";
          head = head ?? str(p, "instructionsHead");
          break;
        case "node-dispatched":
          state = "executing";
          label = label ?? str(p, "actorName");
          head = head ?? str(p, "instructionsHead");
          activity = undefined;
          break;
        case "node-executing":
          // waiting ⇄ executing is a self-loop; leaving the wait must clear
          // the wait activity or the rest of the generation reads "waiting".
          if (state === "queued" || state === "waiting") {
            state = "executing";
            activity = undefined;
          }
          break;
        case "node-waiting":
          state = "waiting";
          activity =
            p.cause === "backoff"
              ? t("zconfig.wfWaitBackoff", { reason: str(p, "reason") ?? "?" })
              : t("zconfig.wfWaitSlot");
          break;
        case "node-progress": {
          state = "executing";
          const tool =
            typeof p.lastTool === "object" && p.lastTool !== null
              ? (p.lastTool as Record<string, unknown>)
              : null;
          const toolName = tool !== null ? str(tool, "name") : undefined;
          const toolTarget = tool !== null ? str(tool, "target") : undefined;
          const turn = typeof p.turn === "number" ? p.turn : undefined;
          activity =
            toolName !== undefined
              ? [toolName, toolTarget?.slice(0, 60)].filter(Boolean).join(" ")
              : turn !== undefined
                ? t("zconfig.wfNodeTurn", { n: turn })
                : activity;
          break;
        }
        case "node-repairing":
          state = "repairing";
          activity = t("zconfig.wfNodeRepairAttempt", {
            n: typeof p.attempt === "number" ? p.attempt : "?",
          });
          break;
        case "node-nudged":
          activity = t("zconfig.wfNodeNudgedShort");
          break;
        case "node-settled": {
          const outcome = p.outcome;
          state =
            outcome === "ok" || outcome === "failed" || outcome === "cancelled" ? outcome : "ok";
          activity = p.cached === true ? t("zconfig.wfNodeCached") : undefined;
          error =
            typeof p.error === "object" && p.error !== null
              ? str(p.error as Record<string, unknown>, "message")
              : undefined;
          break;
        }
      }
      nodes.set(key, {
        key,
        site: typeof ref.siteId === "string" && ref.siteId ? ref.siteId : key,
        ordinal: typeof ref.ordinal === "number" ? ref.ordinal : undefined,
        state,
        ...(label !== undefined ? { label } : {}),
        ...(head !== undefined ? { head } : {}),
        ...(activity !== undefined ? { activity } : {}),
        ...(error !== undefined ? { error } : {}),
        events: own,
      });
      continue;
    }
    if (ev.type === "report" || ev.type === "import-cache-closed") {
      // Filed by a step this slice knows → the step's own detail. A script-
      // level report (no instance ref) stays a phase row.
      const site = refText(p.instance);
      const owner = site === undefined ? undefined : nodes.get(site);
      if (owner !== undefined) {
        owner.events.push(ev);
        continue;
      }
    }
    others.push(ev);
  }
  return { actors, nodes: [...nodes.values()], others, settled, spentTokens };
}

/** Sort comparator: highest sequence first (a clipped row without one sinks). */
export function newestFirst(a: WorkflowRunEvent, b: WorkflowRunEvent): number {
  const sa = typeof a.sequence === "number" ? a.sequence : 0;
  const sb = typeof b.sequence === "number" ? b.sequence : 0;
  return sb - sa;
}

/**
 * The rows a phase card lists at its own level, newest first: everything no
 * step owns, plus the non-lifecycle rows (reports, cache closes) of steps
 * homed in ANOTHER slice — a resume replay moves a step to its last slice,
 * and the reports it filed earlier must not vanish with it.
 */
export function phaseRows(
  fold: { nodes: NodeAgg[]; others: WorkflowRunEvent[] },
  isOwn: (key: string) => boolean,
): WorkflowRunEvent[] {
  const orphans = fold.nodes
    .filter((n) => !isOwn(n.key))
    .flatMap((n) => n.events.filter((e) => !e.type.startsWith("node-")));
  return [...fold.others, ...orphans].sort(newestFirst);
}

// ─── Phase slicing ──────────────────────────────────────────────────────────

/** One phase's slice of the journal, bounded by phase-entered events. */
export interface PhaseSlice {
  /** null = the leading slice before the first phase-entered. */
  phase: string | null;
  /** The marker's re-entry count (resume replays); ≥2 is worth showing. */
  ordinal?: number;
  events: WorkflowRunEvent[];
}

export interface PhaseSlicing {
  slices: PhaseSlice[];
  /** Node key → index of the slice holding its LAST event. */
  nodeHome: Map<string, number>;
  /** Actor key → index of the slice holding its FIRST creation. */
  actorHome: Map<string, number>;
}

/**
 * Events sliced at every phase-entered boundary (the journal has no
 * phase-exited — entering N implies N-1 done): each slice renders as one
 * phase section. A leading slice (phase: null) holds whatever preceded the
 * first phase. Resume REPLAYS the script prefix — the same instances and
 * actors re-emit into later slices — so each node renders only in the slice
 * holding its LAST event and each actor only in the slice of its FIRST
 * creation (nodeHome/actorHome); without that, a resumed run shows every
 * node twice.
 */
export function slicePhases(events: WorkflowRunEvent[]): PhaseSlicing {
  const slices: PhaseSlice[] = [];
  const nodeHome = new Map<string, number>();
  const actorHome = new Map<string, number>();
  let cur: PhaseSlice = { phase: null, events: [] };
  const pushIfUsed = () => {
    if (cur.events.length > 0 || cur.phase !== null) slices.push(cur);
  };
  for (const e of events) {
    if (e.type === "phase-entered") {
      const p = e.payload ?? {};
      const name = str(p, "phaseName") ?? str(p, "name") ?? "?";
      const ordinal = num(p, "ordinal");
      pushIfUsed();
      cur = { phase: name, events: [], ...(ordinal !== undefined ? { ordinal } : {}) };
      continue;
    }
    cur.events.push(e);
    // `slices.length` is cur's own index — cur is not pushed yet.
    const idx = slices.length;
    if (e.type.startsWith("node-")) {
      nodeHome.set(instanceKeyOf(e.payload ?? {}), idx);
    } else if (e.type === "actor-created") {
      const key = actorKeyOf(e.payload ?? {});
      if (!actorHome.has(key)) actorHome.set(key, idx);
    }
  }
  pushIfUsed();
  return { slices, nodeHome, actorHome };
}
