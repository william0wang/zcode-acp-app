// @vitest-environment node
// Battery spec 2026-11: the discovery poll now rides a one-shot timer chain
// with a progressive backoff ladder (4/8/15/30s) while the list keeps
// arriving unchanged, stops entirely while the page is hidden, and skips the
// store write (and therefore every list re-render) when a poll re-delivers an
// unchanged list. These tests pin exactly those three behaviors and the
// resets that must climb the ladder back down: a changed list, a manual
// probe, a network failure (so hubOffline detection keeps its 4s pace), and
// every foreground return.
import { afterEach, beforeEach, expect, test, vi } from "vitest";

const LADDER = [4000, 8000, 15000, 30000];

let rows: unknown[] = [];
let getCalls = 0;
let networkFail = false;

function okJson(body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { "Content-Type": "application/json" },
  });
}

// The WebView's visibility contract, in the minimal shape the store reads.
// defineProperty (not assignment) because the node environment's own
// document is read-only, and the store's module-level listener only ever
// registers — the tests drive applyVisibility directly.
function setVisibility(state: string) {
  Object.defineProperty(globalThis, "document", {
    configurable: true,
    writable: true,
    value: { visibilityState: state, addEventListener: () => {} },
  });
}

const INSTANCE = { id: "i1", workspace: "/w", sessions: [] };
const INSTANCE_CHANGED = { id: "i2", workspace: "/w", sessions: [] };

beforeEach(() => {
  getCalls = 0;
  networkFail = false;
  rows = [INSTANCE];
  const backing = new Map<string, string>();
  (globalThis as Record<string, unknown>).localStorage = {
    getItem: (k: string) => backing.get(k) ?? null,
    setItem: (k: string, v: string) => backing.set(k, String(v)),
    removeItem: (k: string) => backing.delete(k),
    clear: () => backing.clear(),
  };
  setVisibility("visible");
  (globalThis as Record<string, unknown>).fetch = async (
    input: unknown,
  ): Promise<Response> => {
    const url = String(input);
    if (url.includes("/api/instances")) {
      getCalls++;
      if (networkFail) throw new TypeError("fetch failed");
      return okJson(rows);
    }
    if (url.includes("/api/quota")) return okJson([]);
    throw new Error(`test bug: no scripted response for ${url}`);
  };
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
  delete (globalThis as Record<string, unknown>).document;
});

async function freshStore() {
  vi.resetModules();
  const mod = await import("../src/store/appStore");
  return mod.useAppStore;
}

// Connect, let the immediate first refresh land, and return with the ladder
// armed at its bottom rung (the next poll fires at LADDER[0]).
async function connected() {
  const useAppStore = await freshStore();
  useAppStore.getState().connectToHub({ hubUrl: "http://hub", token: "t" });
  await vi.advanceTimersByTimeAsync(0);
  return useAppStore;
}

test("an unchanged list backs off up the ladder and publishes nothing", async () => {
  const useAppStore = await connected();
  expect(getCalls).toBe(1);
  const published = useAppStore.getState().instances;

  for (const rung of LADDER) {
    // Nothing fires before this rung's delay.
    await vi.advanceTimersByTimeAsync(rung - 1);
    expect(getCalls).toBe(1 + LADDER.indexOf(rung));
    await vi.advanceTimersByTimeAsync(1);
    expect(getCalls).toBe(2 + LADDER.indexOf(rung));
  }
  // Every answer was identical: no set, so the reference never changed and
  // no list subscriber re-rendered.
  expect(useAppStore.getState().instances).toBe(published);
});

test("a changed list publishes immediately and resets the ladder to 4s", async () => {
  const useAppStore = await connected();
  await vi.advanceTimersByTimeAsync(LADDER[0] - 1); // next poll fires at 4s
  await vi.advanceTimersByTimeAsync(1); // unchanged → step up (next 8s)
  expect(getCalls).toBe(2);

  rows = [INSTANCE_CHANGED];
  await vi.advanceTimersByTimeAsync(8000); // the 8s rung fires on the changed list
  expect(getCalls).toBe(3);
  expect(useAppStore.getState().instances.map((i) => i.id)).toEqual(["i2"]);

  // The reset means the NEXT gap is the bottom rung, not the 15s it would
  // have been unclimbed.
  await vi.advanceTimersByTimeAsync(LADDER[0]);
  expect(getCalls).toBe(4);
});

test("a manual probe resets the ladder and always publishes", async () => {
  const useAppStore = await connected();
  await vi.advanceTimersByTimeAsync(LADDER[0]); // one unchanged poll (next rung: 8s)
  const before = useAppStore.getState().instances;

  await useAppStore.getState().refreshInstances({ probe: true });
  // Same content, yet a probe is a resync: the list republishes.
  expect(useAppStore.getState().instances).not.toBe(before);

  // The re-arm must land 4s after the PROBE (t=8000), not at the retired
  // chain's old 8s rung (t=12000) — an earlier version of this test probed
  // on the boundary and could not tell the two apart.
  await vi.advanceTimersByTimeAsync(LADDER[0] - 1);
  expect(getCalls).toBe(3);
  await vi.advanceTimersByTimeAsync(1);
  expect(getCalls).toBe(4);
});

test("network failures hold the ladder at 4s so hubOffline stays timely", async () => {
  const useAppStore = await connected();
  // Climb two rungs first, so the next poll is scheduled 15s out — the
  // failure reset is only observable from a NON-bottom rung.
  await vi.advanceTimersByTimeAsync(LADDER[0]); // unchanged poll → step 1
  await vi.advanceTimersByTimeAsync(LADDER[1]); // unchanged poll → step 2
  expect(getCalls).toBe(3);

  networkFail = true;
  await vi.advanceTimersByTimeAsync(LADDER[2]); // the 15s rung fires: failure #1
  expect(getCalls).toBe(4);
  // Failure #2 must land 4s later (the reset), not 15s (the rung it died on).
  await vi.advanceTimersByTimeAsync(LADDER[0] - 1);
  expect(getCalls).toBe(4);
  await vi.advanceTimersByTimeAsync(1);
  expect(getCalls).toBe(5);
  // And #3 — the hubOffline banner's threshold — stays on the 4s cadence.
  await vi.advanceTimersByTimeAsync(LADDER[0]);
  expect(getCalls).toBe(6);
  expect(useAppStore.getState().hubOffline).toBe(true);
});

test("a lingering discovery error republishes even on an unchanged list", async () => {
  const useAppStore = await connected();
  // Non-network failure (an HTTP error surfaces as instancesError, not
  // hubOffline): the banner is up with the list frozen.
  (globalThis as Record<string, unknown>).fetch = async () =>
    new Response("nope", { status: 500 });
  await vi.advanceTimersByTimeAsync(LADDER[0]);
  expect(useAppStore.getState().instancesError).not.toBeNull();

  // Recovery: the list is unchanged, but the error flag must clear — the
  // skip-the-set path always publishes when an error is latching.
  rows = [INSTANCE];
  (globalThis as Record<string, unknown>).fetch = async (input: unknown) => {
    const url = String(input);
    if (url.includes("/api/instances")) return okJson(rows);
    if (url.includes("/api/quota")) return okJson([]);
    throw new Error(`test bug: ${url}`);
  };
  await vi.advanceTimersByTimeAsync(LADDER[0]);
  expect(useAppStore.getState().instancesError).toBeNull();
});

test("hidden stops polling; visible refreshes once and re-arms at 4s", async () => {
  const useAppStore = await connected();
  await vi.advanceTimersByTimeAsync(LADDER[1] - 1);
  await vi.advanceTimersByTimeAsync(1); // one rung up

  setVisibility("hidden");
  useAppStore.getState().applyVisibility();
  const atHide = getCalls;
  // No poll ever fires while hidden.
  await vi.advanceTimersByTimeAsync(120_000);
  expect(getCalls).toBe(atHide);

  setVisibility("visible");
  useAppStore.getState().applyVisibility();
  await vi.advanceTimersByTimeAsync(0);
  expect(getCalls).toBe(atHide + 1); // the foreground refresh itself
  // Bottom rung from here on.
  await vi.advanceTimersByTimeAsync(LADDER[0]);
  expect(getCalls).toBe(atHide + 2);
});

test("disconnect retires an in-flight chain mid-await", async () => {
  const useAppStore = await connected();
  await vi.advanceTimersByTimeAsync(LADDER[0] - 1);
  await vi.advanceTimersByTimeAsync(1); // poll fires; its fetch awaits a microtask
  const atTearDown = getCalls;
  useAppStore.getState().disconnectHub();
  await vi.advanceTimersByTimeAsync(120_000);
  // The awaited answer landed, but the chain saw its generation retired and
  // stood down instead of re-arming.
  expect(getCalls).toBe(atTearDown);
});
