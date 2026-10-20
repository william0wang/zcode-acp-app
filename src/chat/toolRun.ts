// Aggregate summary for a run of adjacent tool calls (the collapsed
// "N tool calls" card). The run itself is formed by adjacency in the chat
// view — this module only derives what the summary row shows, so both the
// React and the desktop ports share one definition of "running"/"failed".

export interface ToolRunEntry {
  /** Display name (raw backend tool name or the head of the wire title). */
  name: string;
  /** ACP status of the call: pending / in_progress / completed / failed. */
  status: string;
}

export interface ToolRunSummary {
  count: number;
  /** Distinct tool names in first-seen order (the summary preview). */
  names: string[];
  /** Any call still pending or in flight. */
  running: boolean;
  /** Any call ended in a failed state. */
  failed: boolean;
}

const RUNNING_STATUSES = new Set(["pending", "in_progress"]);

export function summarizeToolRun(
  entries: readonly ToolRunEntry[],
): ToolRunSummary {
  const names: string[] = [];
  let running = false;
  let failed = false;
  for (const e of entries) {
    if (!names.includes(e.name)) names.push(e.name);
    if (RUNNING_STATUSES.has(e.status)) running = true;
    else if (e.status === "failed") failed = true;
  }
  return { count: entries.length, names, running, failed };
}

// The card's name: the raw backend tool name, else the segment before the
// first colon of the wire title ("Bash: npm test" → "Bash"). Same rule as
// the single ToolCard, so a group reads like the rows it contains.
export function toolNameOf(
  args: { rawName?: string; title?: string } | undefined,
  fallback: string,
): string {
  return (
    args?.rawName ||
    (args?.title ?? fallback).split(":")[0]?.trim() ||
    fallback
  );
}

// A tool title is the raw command: a heredoc or `&&` chain carries real
// newlines, which turn a one-line summary row into a wall. Collapse
// whitespace runs and cap the length (the row truncates the rest).
const MAX_NAME_CHARS = 240;

export function oneLine(text: string): string {
  const flat = text.replace(/\s+/g, " ").trim();
  return flat.length > MAX_NAME_CHARS
    ? `${flat.slice(0, MAX_NAME_CHARS)}…`
    : flat;
}
