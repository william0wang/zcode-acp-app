// Rewind plumbing (backend spec `.zcode/docs/session-rewind-backend-requirements.md`
// §4.3): replayed addressable chunks carry `_meta.zcode.mid`; live echoes
// and local optimistic bubbles never do. The mid doubles as the UI's
// "addressable for edit/resend" gate next to `rewindCapable`.
import { describe, expect, it } from "vitest";
import { applyOne } from "../src/store/replay";

const chunk = (messageId: string, text: string) => ({
  sessionUpdate: "user_message_chunk",
  messageId,
  content: { type: "text", text },
});

describe("replay mid stamping", () => {
  it("stamps a notification-level _meta.zcode.mid on replayed chunks", () => {
    const r = applyOne([], chunk("u-1", "hello"), {
      zcode: { mid: "m-1" },
    });
    expect(r?.messages[0]?.mid).toBe("m-1");
  });

  it("stamps an update-level _meta.zcode.mid too", () => {
    const r = applyOne(
      [],
      { ...chunk("u-1", "hi"), _meta: { zcode: { mid: "m-2" } } },
      undefined,
    );
    expect(r?.messages[0]?.mid).toBe("m-2");
  });

  it("keeps stamping when the message already exists (second chunk)", () => {
    const first = applyOne([], chunk("u-1", "hel"), {
      zcode: { mid: "m-1" },
    });
    const second = applyOne(first!.messages!, chunk("u-1", "lo"), {
      zcode: { mid: "m-1" },
    });
    const msgs = second?.messages ?? [];
    expect(msgs).toHaveLength(1);
    expect(msgs[0]?.mid).toBe("m-1");
    expect((msgs[0]?.parts[0] as { text: string }).text).toBe("hello");
  });

  it("leaves live echoes (no mid meta) address-free", () => {
    const r = applyOne([], chunk("uprompt_1", "live"), undefined);
    expect(r?.messages[0]?.mid).toBeUndefined();
  });

  it("never invents a mid when only one side of the pair is present", () => {
    // A mid without the chunk's own messageId groups nowhere — nothing is
    // stamped (the wire pair arrives together on replay, by contract).
    const r = applyOne(
      [],
      { sessionUpdate: "user_message_chunk", content: { type: "text", text: "x" } },
      { zcode: { mid: "m-3" } },
    );
    expect(r?.messages[0]?.mid).toBeUndefined();
  });
});
