// Approval-card context resolution (ADR 0003): the matched tool_call part
// path, and the wire-only rebuild for AskUserQuestion popups whose part
// never arrived (ask re-delivered after a reconnect, or raised for a session
// whose messages are not loaded) — the fix for the degraded card that showed
// no question text and only a nameless "input" expander with raw JSON.
import { expect, test } from "vitest";
import { resolveApprovalContext } from "../src/store/appStore";
import type { ChatMessage } from "../src/lib/types";

const questions = [
  { question: "Which library?", options: [{ label: "A" }, { label: "B" }] },
  { question: "Which version?", options: [{ label: "1" }, { label: "2" }] },
];

test("matched part still wins: names and detail come from the part", () => {
  const msgs: ChatMessage[] = [
    {
      id: "m1",
      role: "assistant",
      createdAt: 0,
      parts: [
        {
          type: "tool-call",
          toolCallId: "tc_1",
          toolName: "AskUserQuestion",
          rawName: "AskUserQuestion",
          detail: "from the part",
          status: "pending",
        },
      ],
    },
  ];
  const ctx = resolveApprovalContext(
    { toolCall: { toolCallId: "tc_1", rawInput: { questions }, title: "wire title" } },
    msgs,
  );
  expect(ctx?.toolName).toBe("AskUserQuestion");
  expect(ctx?.detail).toBe("from the part");
  expect(ctx?.title).toBe("wire title");
});

test("wire-only AskUserQuestion: title is the question, shape classifies the card", () => {
  const ctx = resolveApprovalContext(
    {
      toolCall: {
        toolCallId: "tc_9_1",
        rawInput: { questions },
        title: "Which version?",
      },
    },
    [],
  );
  // toolName drives PermissionDialog's isQuestion branch; detail is the body.
  expect(ctx?.toolName).toBe("AskUserQuestion");
  expect(ctx?.detail).toBe("Which version?");
  expect(ctx?.rawInputText).toContain('"question"');
});

test("wire-only without a title: the toolCallId _N suffix picks the question", () => {
  const ctx = resolveApprovalContext(
    // Multi-select popups carry _N_M; the FIRST number is the question index.
    { toolCall: { toolCallId: "ask_1_0", rawInput: { questions } } },
    [],
  );
  expect(ctx?.toolName).toBe("AskUserQuestion");
  expect(ctx?.detail).toBe("Which version?");
});

test("non-question rawInput stays a plain tool card", () => {
  const ctx = resolveApprovalContext(
    { toolCall: { toolCallId: "call_1", rawInput: { command: "npm test" } } },
    [],
  );
  expect(ctx?.toolName).toBeUndefined();
  expect(ctx?.detail).toBeUndefined();
  expect(ctx?.rawInputText).toContain("npm test");
});
