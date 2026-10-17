// @vitest-environment node
// The journal rendering layer: the v4 wire contract does not restate the
// engine's payload shapes, so the app's own reducer must read the engine's
// field spellings (instance refs, outcome, instructionsHead) and fold node
// lifecycles per instance. These tests pin the exact spellings against
// upstream's RunEvent union — a drifted read collapses every node onto one
// key and resurrects the "old states pile up" view.
import { describe, expect, it } from "vitest";
import type { WorkflowRunEvent } from "../src/lib/types";
import {
  describeRunEvent,
  newestFirst,
  phasePaints,
  phaseRows,
  reduceSlice,
  refText,
  slicePhases,
  statusWord,
} from "../src/components/config/workflowRunEventText";

/**
 * Minimal i18next stand-in: a two-entry catalog so `defaultValue` fallbacks
 * (statusWord's unknown-token passthrough) behave like the real thing, and
 * every other key interpolates its own id so assertions read like the calls.
 */
const catalog: Record<string, string> = {
  "zconfig.wfStatusRunning": "运行中",
  "zconfig.wfStatusCompleted": "已完成",
};
const t = (key: string, values?: Record<string, unknown>) => {
  const template = catalog[key];
  if (template === undefined) {
    return values?.defaultValue !== undefined ? String(values.defaultValue) : key;
  }
  return template.replace(/\{\{(\w+)\}\}/g, (_, k) => String(values?.[k] ?? "?"));
};

function ev(sequence: number, type: string, payload: Record<string, unknown>): WorkflowRunEvent {
  return { sequence, type, payload };
}

const instance = (siteId: string, ordinal: number) => ({ siteId, ordinal });

describe("refText", () => {
  it("renders the engine's site@ordinal form", () => {
    expect(refText(instance("review#3", 2))).toBe("review#3@2");
  });

  it("degrades on anything that is not a ref", () => {
    expect(refText(undefined)).toBeUndefined();
    expect(refText("review#3")).toBeUndefined();
    expect(refText({ ordinal: 1 })).toBeUndefined();
  });
});

describe("describeRunEvent", () => {
  it("reads node identity from the instance ref, not a top-level siteId", () => {
    const desc = describeRunEvent(
      t,
      ev(1, "node-dispatched", { instance: instance("scan", 4), actorName: "scout" }),
    );
    expect(desc.detail).toContain("scout");
    expect(desc.detail).toContain("scan@4");
    expect(desc.monoLabel).toBeUndefined();
  });

  it("tints failed settlements and carries the error message", () => {
    const desc = describeRunEvent(
      t,
      ev(2, "node-settled", {
        instance: instance("scan", 4),
        outcome: "failed",
        error: { code: "EIO", message: "disk went away" },
      }),
    );
    expect(desc.tone).toBe("failed");
    expect(desc.detail).toContain("disk went away");
  });

  it("localizes run settlement status; stopped/errored read as failed", () => {
    const ok = describeRunEvent(t, ev(3, "run-settled", { status: "completed" }));
    expect(ok.tone).toBe("default");
    const stopped = describeRunEvent(
      t,
      ev(4, "run-settled", { status: "stopped", stopReason: "user" }),
    );
    expect(stopped.tone).toBe("failed");
    expect(stopped.detail).toContain("user");
  });

  it("unknown kinds keep the raw type name in monospace with a short preview", () => {
    const unknown = describeRunEvent(t, ev(6, "future-event", { blob: "x".repeat(400) }));
    expect(unknown.label).toBe("future-event");
    expect(unknown.monoLabel).toBe(true);
    expect(unknown.detail!.length).toBeLessThanOrEqual(200);
  });
});

describe("statusWord", () => {
  it("maps known tokens through the catalog and passes unknown ones through", () => {
    expect(statusWord(t, "running")).toBe("运行中");
    expect(statusWord(t, "completed")).toBe("已完成");
    expect(statusWord(t, "brand-new-state")).toBe("brand-new-state");
    expect(statusWord(t, undefined)).toBe("?");
  });
});

describe("reduceSlice", () => {
  it("folds one node's lifecycle into a single latest-state line", () => {
    const out = reduceSlice(t, [
      ev(1, "node-queued", { instance: instance("scan", 1), instructionsHead: "scan the tree" }),
      ev(2, "node-dispatched", {
        instance: instance("scan", 1),
        actorName: "scout",
        instructionsHead: "scan the tree",
      }),
      ev(3, "node-progress", {
        instance: instance("scan", 1),
        turn: 2,
        lastTool: { name: "Grep", target: "src/**" },
      }),
      ev(4, "node-settled", { instance: instance("scan", 1), outcome: "ok" }),
    ]);
    expect(out.nodes).toHaveLength(1);
    const node = out.nodes[0]!;
    expect(node.state).toBe("ok");
    expect(node.label).toBe("scout");
    expect(node.head).toBe("scan the tree");
  });

  it("keys nodes by instance ref — two instances stay two lines", () => {
    const out = reduceSlice(t, [
      ev(1, "node-queued", { instance: instance("ask", 1) }),
      ev(2, "node-queued", { instance: instance("ask", 2) }),
      ev(3, "node-settled", { instance: instance("ask", 1), outcome: "ok" }),
    ]);
    expect(out.nodes).toHaveLength(2);
    expect(out.nodes.map((n) => n.state).sort()).toEqual(["ok", "queued"]);
  });

  it("settled is final — replayed lifecycle noise must not reopen it", () => {
    const out = reduceSlice(t, [
      ev(1, "node-settled", { instance: instance("ask", 1), outcome: "failed" }),
      ev(2, "node-progress", { instance: instance("ask", 1), turn: 9 }),
    ]);
    expect(out.nodes[0]!.state).toBe("failed");
  });

  it("usage updates collapse to the newest total, actors keep names", () => {
    const out = reduceSlice(t, [
      ev(1, "actor-created", { actor: instance("a", 1), name: "scout" }),
      ev(2, "usage-updated", { spentTokens: 1000 }),
      ev(3, "usage-updated", { spentTokens: 2500 }),
      ev(4, "log", { message: "hello" }),
    ]);
    expect(out.actors).toEqual([{ key: "a@1", label: "scout" }]);
    expect(out.spentTokens).toBe(2500);
    expect(out.others).toHaveLength(1); // the log stays a readable row
  });

  it("leaving the wait state clears the wait activity", () => {
    const out = reduceSlice(t, [
      ev(1, "node-dispatched", { instance: instance("ask", 1) }),
      ev(2, "node-waiting", { instance: instance("ask", 1), cause: "slot" }),
      ev(3, "node-executing", { instance: instance("ask", 1) }),
    ]);
    expect(out.nodes[0]!.state).toBe("executing");
    expect(out.nodes[0]!.activity).toBeUndefined();
  });

  it("keeps run-settled as the section's closing event", () => {
    const out = reduceSlice(t, [ev(1, "run-settled", { status: "completed" })]);
    expect(out.settled?.type).toBe("run-settled");
  });

  it("keeps each step's own rows and nests the reports it files", () => {
    const out = reduceSlice(t, [
      ev(1, "node-queued", { instance: instance("scan", 1), instructionsHead: "scan the repo" }),
      ev(2, "node-dispatched", { instance: instance("scan", 1), actorName: "scout" }),
      ev(3, "node-progress", { instance: instance("scan", 1), turn: 2 }),
      ev(4, "report", { instance: instance("scan", 1), item: { found: 3 } }),
      ev(5, "report", { item: "script-level" }),
      ev(6, "log", { message: "narration" }),
      ev(7, "import-cache-closed", { instance: instance("scan", 1), actorName: "scout" }),
      ev(8, "node-settled", { instance: instance("scan", 1), outcome: "ok" }),
    ]);
    expect(out.nodes).toHaveLength(1);
    expect(out.nodes[0]!.events.map((e) => e.sequence)).toEqual([1, 2, 3, 4, 7, 8]);
    // A report without an instance ref belongs to the script, not a step.
    expect(out.others.map((e) => e.sequence)).toEqual([5, 6]);
  });

  it("a settled step keeps its history and ignores replayed rows", () => {
    const out = reduceSlice(t, [
      ev(1, "node-dispatched", { instance: instance("scan", 1) }),
      ev(2, "node-settled", { instance: instance("scan", 1), outcome: "ok" }),
      ev(3, "node-dispatched", { instance: instance("scan", 1) }),
    ]);
    expect(out.nodes[0]!.state).toBe("ok");
    expect(out.nodes[0]!.events.map((e) => e.sequence)).toEqual([1, 2]);
  });

  it("a report filed after the step settled still nests under it", () => {
    const out = reduceSlice(t, [
      ev(1, "node-settled", { instance: instance("scan", 1), outcome: "ok" }),
      ev(2, "report", { instance: instance("scan", 1), item: "late" }),
    ]);
    expect(out.nodes[0]!.events.map((e) => e.sequence)).toEqual([1, 2]);
    expect(out.others).toEqual([]);
  });
});

describe("phaseRows", () => {
  it("lists unowned rows newest-first and frees the reports of steps homed elsewhere", () => {
    const fold = reduceSlice(t, [
      ev(1, "log", { message: "first" }),
      ev(2, "node-dispatched", { instance: instance("scan", 1) }),
      ev(3, "report", { instance: instance("scan", 1), item: "filed here" }),
      ev(4, "node-settled", { instance: instance("scan", 1), outcome: "ok" }),
      ev(5, "log", { message: "last" }),
    ]);
    // The step is owned by this slice: its report stays inside the step.
    expect(phaseRows(fold, () => true).map((e) => e.sequence)).toEqual([5, 1]);
    // Homed in a later slice (resume replay): the report rejoins the phase,
    // the lifecycle rows do not.
    expect(phaseRows(fold, () => false).map((e) => e.sequence)).toEqual([5, 3, 1]);
  });
});

describe("phasePaints", () => {
  it("an empty just-entered phase paints nothing and cannot be the head", () => {
    const fold = reduceSlice(t, [
      ev(1, "log", { message: "x" }),
      ev(2, "node-dispatched", { instance: instance("scan", 1) }),
    ]);
    const owns = (key: string) => key === "scan@1";
    expect(phasePaints(fold, owns, () => false)).toBe(true);
    expect(phasePaints(reduceSlice(t, []), owns, () => false)).toBe(false);
  });
});

describe("describeRunEvent omitSite", () => {
  it("drops the instance ref from rows nested under their step", () => {
    const e = ev(3, "node-progress", {
      instance: instance("scan", 4),
      turn: 2,
      lastTool: { name: "Read", target: "a.ts" },
    });
    expect(describeRunEvent(t, e).detail).toContain("scan@4");
    const nested = describeRunEvent(t, e, { omitSite: true }).detail;
    expect(nested).not.toContain("scan@4");
    expect(nested).toContain("Read");
  });
});

describe("newestFirst", () => {
  it("sorts by sequence descending and sinks a row without one", () => {
    const bare = { type: "log", payload: {} } as unknown as WorkflowRunEvent;
    const rows = [ev(2, "log", {}), bare, ev(5, "log", {}), ev(1, "log", {})];
    expect([...rows].sort(newestFirst).map((e) => e.sequence)).toEqual([5, 2, 1, undefined]);
  });
});

describe("slicePhases", () => {
  it("slices at phase markers, keeps the leading slice, carries the ordinal", () => {
    const out = slicePhases([
      ev(1, "run-started", {}),
      ev(2, "actor-created", { name: "scout", actor: instance("a", 0) }),
      ev(3, "phase-entered", { name: "scan", ordinal: 1 }),
      ev(4, "node-dispatched", { instance: instance("scan", 1) }),
      ev(5, "phase-entered", { name: "scan", ordinal: 2 }),
      ev(6, "node-progress", { instance: instance("scan", 1), turn: 1 }),
    ]);
    expect(out.slices.map((s) => s.phase)).toEqual([null, "scan", "scan"]);
    expect(out.slices.map((s) => s.ordinal)).toEqual([undefined, 1, 2]);
    expect(out.slices.map((s) => s.events.length)).toEqual([2, 1, 1]);
    // A node renders only where its LAST event fell; an actor where it was
    // FIRST created — a resumed run must not show either twice.
    expect(out.nodeHome.get("scan@1")).toBe(2);
    expect(out.actorHome.get("a@0")).toBe(0);
  });

  it("prefers phaseName over name and never emits an empty leading slice", () => {
    const out = slicePhases([ev(1, "phase-entered", { phaseName: "build", name: "x" })]);
    expect(out.slices.map((s) => s.phase)).toEqual(["build"]);
  });
});
