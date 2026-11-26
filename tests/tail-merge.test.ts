// Session re-entry (ADR-0015): a captured tail replay merges into the painted
// messages at the Seam instead of reloading. These pin mergeTailReplay's
// contract — what stays painted, what the replay replaces, when in-flight
// output survives, and when the view is Far Behind.
import { describe, expect, it } from "vitest";
import { mergeTailReplay } from "../src/store/replay";
import type { ChatMessage } from "../src/lib/types";

const text = (
  id: string,
  role: "user" | "assistant",
  t: string,
  extra: Partial<ChatMessage> = {},
): ChatMessage => ({
  id,
  role,
  parts: [{ type: "text", text: t }],
  createdAt: 0,
  ...extra,
});

const card = (
  id: string,
  toolCallId: string,
  status = "pending",
): ChatMessage => ({
  id,
  role: "assistant",
  parts: [{ type: "tool-call", toolCallId, toolName: "Bash", detail: "", status }],
  createdAt: 0,
});

const texts = (msgs: ChatMessage[]) =>
  msgs.map((m) =>
    m.parts.map((p) => (p.type === "text" ? p.text : p.type)).join("|"),
  );

describe("mergeTailReplay", () => {
  it("returns the painted array itself when the replay confirms it", () => {
    const painted = [text("u1", "user", "Q"), text("a1", "assistant", "A")];
    const replay = [text("u1", "user", "Q"), text("a1", "assistant", "A")];
    expect(mergeTailReplay(painted, replay, false)).toBe(painted);
  });

  it("appends what is new and keeps every confirmed message's identity", () => {
    const painted = [text("u1", "user", "Q1"), text("a1", "assistant", "A1")];
    const replay = [
      text("u1", "user", "Q1"),
      text("a1", "assistant", "A1"),
      text("u2", "user", "Q2"),
      text("a2", "assistant", "A2"),
    ];
    const out = mergeTailReplay(painted, replay, false)!;
    expect(texts(out)).toEqual(["Q1", "A1", "Q2", "A2"]);
    expect(out[0]).toBe(painted[0]);
    expect(out[1]).toBe(painted[1]);
  });

  it("takes the replay's spelling from the first message that differs", () => {
    // a1 grew under the same id while we were away.
    const painted = [text("u1", "user", "Q"), text("a1", "assistant", "A be")];
    const replay = [
      text("u1", "user", "Q"),
      text("a1", "assistant", "A bef", { mid: "a1" }),
      text("a2", "assistant", "A2"),
    ];
    const out = mergeTailReplay(painted, replay, false)!;
    expect(texts(out)).toEqual(["Q", "A bef", "A2"]);
    expect(out[0]).toBe(painted[0]);
    expect(out[1]).toBe(replay[1]);
  });

  it("lends the replay-only rewind handle without re-rendering the content", () => {
    const painted = [text("a1", "assistant", "A1")];
    const replay = [text("a1", "assistant", "A1", { mid: "a1" })];
    const out = mergeTailReplay(painted, replay, false)!;
    expect(out[0]).toEqual({ ...painted[0], mid: "a1" });
    // Stable from now on.
    expect(mergeTailReplay(out, replay, false)).toBe(out);
  });

  it("keeps a locally sent prompt as its bubble and makes it editable", () => {
    // Live: our own bubble (m9) and the streamed answer under the backend id.
    const painted = [
      text("u1", "user", "Q1"),
      text("a1", "assistant", "A1"),
      text("m9", "user", "Q2"),
      text("a2", "assistant", "A2"),
    ];
    const replay = [
      text("u1", "user", "Q1", { mid: "u1" }),
      text("a1", "assistant", "A1", { mid: "a1" }),
      text("u2", "user", "Q2", { mid: "u2" }),
      text("a2", "assistant", "A2", { mid: "a2" }),
    ];
    const out = mergeTailReplay(painted, replay, false)!;
    // Nothing changed on the wire: the bubble (and everything else) stays.
    expect(texts(out)).toEqual(["Q1", "A1", "Q2", "A2"]);
    expect(out[2].id).toBe("m9");
    expect(out[2].mid).toBe("u2");
    expect(out[3]).toEqual({ ...painted[3], mid: "a2" });
    expect(out[0].mid).toBe("u1");
    expect(out[1].mid).toBe("a1");
    // And it settles: the next replay confirms everything, unchanged.
    expect(mergeTailReplay(out, replay, false)).toBe(out);
  });

  it("anchors on a tool card's call id", () => {
    const painted = [
      text("u1", "user", "Q"),
      text("a1", "assistant", "A1"),
      card("m3", "call_1", "in_progress"),
    ];
    const replay = [
      text("u1", "user", "Q"),
      text("a1", "assistant", "A1"),
      card("m40", "call_1", "completed"),
      text("a2", "assistant", "A2"),
    ];
    const out = mergeTailReplay(painted, replay, false)!;
    expect(out.slice(0, 2)).toEqual(painted.slice(0, 2));
    expect(out[2].parts[0]).toMatchObject({ toolCallId: "call_1", status: "completed" });
    expect(texts(out.slice(3))).toEqual(["A2"]);
  });

  it("refreshes a card above the Seam that changed state while away", () => {
    const painted = [
      card("m3", "call_bg", "in_progress"),
      text("a1", "assistant", "A1"),
    ];
    const replay = [
      card("m40", "call_bg", "completed"),
      text("a1", "assistant", "A1"),
    ];
    const out = mergeTailReplay(painted, replay, false)!;
    expect(out[1]).toBe(painted[1]);
    expect(out[0].parts[0]).toMatchObject({ toolCallId: "call_bg", status: "completed" });
  });

  it("carries in-flight output only when asked and only past the replay", () => {
    // The step streaming at the disconnect: its text is not in the journal
    // yet, so the replay stops at the prompt.
    const painted = [
      text("a1", "assistant", "A1"),
      text("m9", "user", "Q2"),
      text("a2", "assistant", "A2 par"),
    ];
    const replay = [
      text("a1", "assistant", "A1"),
      text("u2", "user", "Q2", { mid: "u2" }),
    ];
    const carried = mergeTailReplay(painted, replay, true)!;
    expect(texts(carried)).toEqual(["A1", "Q2", "A2 par"]);
    expect(carried[1].id).toBe("u2"); // the stored prompt replaces the bubble
    expect(carried[2]).toBe(painted[2]); // the streamed text is kept as is
    // No turn running: the replay is the whole truth.
    expect(texts(mergeTailReplay(painted, replay, false)!)).toEqual(["A1", "Q2"]);
  });

  it("drops in-flight-looking leftovers once the replay is ahead of them", () => {
    // The step landed under a different id and a new one started elsewhere.
    const painted = [
      text("a1", "assistant", "A1"),
      text("m9", "user", "Q2"),
      text("chunk_x", "assistant", "A2 partial"),
    ];
    const replay = [
      text("a1", "assistant", "A1"),
      text("u2", "user", "Q2"),
      text("a2", "assistant", "A2 complete"),
      text("a3", "assistant", "A3"),
    ];
    const out = mergeTailReplay(painted, replay, true)!;
    expect(texts(out)).toEqual(["A1", "Q2", "A2 complete", "A3"]);
  });

  it("never repeats a card the replay folded into its text", () => {
    // Live, a card splits off the text it followed; replay groups it back.
    const painted = [
      text("u1", "user", "Q"),
      text("a1", "assistant", "A1"),
      card("m5", "call_1", "completed"),
    ];
    const replay = [
      text("u1", "user", "Q"),
      {
        id: "a1",
        role: "assistant",
        createdAt: 0,
        parts: [
          { type: "text", text: "A1" },
          { type: "tool-call", toolCallId: "call_1", toolName: "Bash", detail: "", status: "completed" },
        ],
      },
    ];
    const out = mergeTailReplay(painted, replay, true)!;
    expect(out).toHaveLength(2);
    expect(out[1]).toBe(replay[1]);
  });

  it("lets the replay's order win when a thought and its text traded places", () => {
    const painted = [
      text("u1", "user", "Q"),
      text("a1", "assistant", "A"),
      text("thought_a1", "assistant", "T"),
    ];
    const replay = [
      text("u1", "user", "Q"),
      text("thought_a1", "assistant", "T"),
      text("a1", "assistant", "A"),
    ];
    const out = mergeTailReplay(painted, replay, false)!;
    expect(out.map((m) => m.id)).toEqual(["u1", "thought_a1", "a1"]);
  });

  it("is Far Behind (null) when nothing painted anchors in the replay", () => {
    const painted = [text("u1", "user", "Q1"), text("a1", "assistant", "A1")];
    const replay = [text("u9", "user", "Q9"), text("a9", "assistant", "A9")];
    expect(mergeTailReplay(painted, replay, true)).toBeNull();
    // A lone optimistic bubble never anchors either.
    expect(mergeTailReplay([text("m9", "user", "Q")], replay, true)).toBeNull();
    expect(mergeTailReplay(painted, [], true)).toBeNull();
  });
});
