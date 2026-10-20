// The live stream rolls sub-agent/background tool cards into a session at
// arbitrary points of the host's streaming text (the backend event stream
// carries them; the bridge forwards them as message-id-less tool_calls). The
// apply rule: an id-less card NEVER splices a trailing message that is
// mid-text — it takes its own card message, and id'd text chunks keep
// appending to their own message, so the sentence stays whole.
import { expect, test } from "vitest";

import type { ChatMessage, SessionUpdate } from "../src/lib/types";
import { applyOne } from "../src/store/replay";

const text = (mid: string, t: string) =>
  ({
    sessionUpdate: "agent_message_chunk",
    content: { type: "text", text: t },
    messageId: mid,
  }) as unknown as SessionUpdate;

const card = (id: string) =>
  ({
    sessionUpdate: "tool_call",
    toolCallId: id,
    title: "Bash: research",
  }) as unknown as SessionUpdate;

const apply = (msgs: ChatMessage[], u: SessionUpdate): ChatMessage[] => {
  const r = applyOne(msgs, u);
  expect(r?.messages).toBeDefined();
  return r!.messages!;
};

test("an id-less tool card never splits a streaming text message", () => {
  let msgs: ChatMessage[] = [];
  msgs = apply(msgs, text("m_a", "we can "));
  msgs = apply(msgs, card("tc_1"));
  msgs = apply(msgs, card("tc_2"));
  // The text message stays whole; the cards share ONE card message.
  expect(msgs).toHaveLength(2);
  expect(msgs[0].id).toBe("m_a");
  expect(msgs[0].parts).toHaveLength(1);
  expect((msgs[0].parts[0] as { text: string }).text).toBe("we can ");
  expect(msgs[1].parts).toHaveLength(2);
  expect(msgs[1].parts.every((p) => p.type === "tool-call")).toBe(true);
  // The text continues into its own message — healed into one part.
  msgs = apply(msgs, text("m_a", "identify"));
  expect(msgs).toHaveLength(2);
  expect((msgs[0].parts[0] as { text: string }).text).toBe("we can identify");
});

test("an id-less tool card joins a trailing card run at turn start", () => {
  let msgs: ChatMessage[] = [];
  msgs = apply(msgs, card("tc_1"));
  msgs = apply(msgs, card("tc_2"));
  expect(msgs).toHaveLength(1);
  expect(msgs[0].parts).toHaveLength(2);
  msgs = apply(msgs, text("m_1", "done"));
  expect(msgs).toHaveLength(2);
  expect((msgs[1].parts[0] as { text: string }).text).toBe("done");
});
