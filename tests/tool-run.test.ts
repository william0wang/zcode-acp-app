// The collapsed tool-run card's summary derivation (src/chat/toolRun.ts):
// adjacent tool calls coalesce into one row whose label must stay cheap and
// truthful — distinct names in first-seen order, with running/failed state
// folded over the whole run.
import { expect, test } from "vitest";

import { oneLine, summarizeToolRun, toolNameOf } from "../src/chat/toolRun";

test("names dedupe in first-seen order and count every call", () => {
  const s = summarizeToolRun([
    { name: "Bash", status: "completed" },
    { name: "Read", status: "completed" },
    { name: "Bash", status: "completed" },
    { name: "Grep", status: "completed" },
  ]);
  expect(s.count).toBe(4);
  expect(s.names).toEqual(["Bash", "Read", "Grep"]);
  expect(s.running).toBe(false);
  expect(s.failed).toBe(false);
});

test("a pending or in-flight call keeps the run running", () => {
  expect(
    summarizeToolRun([
      { name: "Bash", status: "completed" },
      { name: "Read", status: "pending" },
    ]).running,
  ).toBe(true);
  expect(
    summarizeToolRun([
      { name: "Bash", status: "completed" },
      { name: "Read", status: "in_progress" },
    ]).running,
  ).toBe(true);
});

test("a failed call marks the run failed without marking it running", () => {
  const s = summarizeToolRun([
    { name: "Bash", status: "completed" },
    { name: "Read", status: "failed" },
  ]);
  expect(s.failed).toBe(true);
  expect(s.running).toBe(false);
});

test("an empty run summarizes to zero and no names", () => {
  expect(summarizeToolRun([])).toEqual({
    count: 0,
    names: [],
    running: false,
    failed: false,
  });
});

test("toolNameOf prefers the raw backend name, then the title head", () => {
  expect(toolNameOf({ rawName: "Bash", title: "Bash: npm test" }, "tool")).toBe(
    "Bash",
  );
  expect(toolNameOf({ title: "Read: /tmp/a.ts" }, "tool")).toBe("Read");
  expect(toolNameOf({ title: "  " }, "tool")).toBe("tool");
  expect(toolNameOf(undefined, "tool")).toBe("tool");
});

test("oneLine flattens a heredoc title and caps long input", () => {
  // A raw command title carries real newlines — the summary row is one line.
  expect(oneLine("Bash: cat <<'EOF'\nimport json\nEOF")).toBe(
    "Bash: cat <<'EOF' import json EOF",
  );
  expect(oneLine("  spaced\n\ttabs  ")).toBe("spaced tabs");
  const long = oneLine("x".repeat(400));
  expect(long.length).toBe(241);
  expect(long.endsWith("…")).toBe(true);
});
