// @vitest-environment node
// The typed-error mapping the failure toasts share (settings-alignment
// P1-7): the desktop HubApiError doctrine on mobile — network is an expected
// offline state, 401 is check-token, and everything else shows the error's
// own message VERBATIM (a refused write's body is the message).
import { expect, test } from "vitest";
import { describeHubError, notConnectedMessage } from "../src/lib/errors";
import { HubApiError } from "../src/lib/hub";

test("a network failure reads as the localized offline line, not the raw fetch text", () => {
  // "Failed to fetch" (or a deadline message) is transport noise; the line
  // the user can act on is that the hub is unreachable.
  const e = new HubApiError("Failed to fetch", undefined, true);
  expect(describeHubError(e)).toBe("Hub unreachable");
});

test("a 401 reads as the localized check-token line", () => {
  const e = new HubApiError("HTTP 401", 401);
  expect(describeHubError(e)).toBe("Unauthorized — check the token");
});

test("a refused write keeps its body verbatim — the body IS the message", () => {
  // Dropping it would leave "HTTP 409" with no way forward (the reset-card
  // refusal answers "nonce is stale — refresh the reset card status").
  const e = new HubApiError(
    "nonce is stale — refresh the reset card status",
    409,
  );
  expect(describeHubError(e)).toBe(
    "nonce is stale — refresh the reset card status",
  );
});

test("a non-HubApiError degrades to its message, never a blank", () => {
  expect(describeHubError(new Error("boom"))).toBe("boom");
  expect(describeHubError("plain string")).toBe("plain string");
});

test("the no-client verdict is its own line", () => {
  expect(notConnectedMessage()).toBe("not connected");
});
