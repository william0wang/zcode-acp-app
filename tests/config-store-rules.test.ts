// @vitest-environment node
// Store-level rules for the configuration screens (ADR-0009).
//
// The response-shape tests cover DECODING and the REST tests cover REQUESTS;
// these cover the two things neither can see: the store's own guards around
// the reset-card session, and what survives a hub switch.
//
// Both were found by reading the code against the server contract rather than
// by a failing test, which is the point — each one had a plausible-looking
// implementation whose failure only shows up as a dead button or a permanent
// "unsupported" banner, with nothing red anywhere.
import { afterEach, beforeAll, beforeEach, expect, test, vi } from "vitest";

interface FakeResponse {
  ok: boolean;
  status: number;
  json: () => Promise<unknown>;
  text: () => Promise<string>;
}

let requested: Array<{ url: string; method: string; body?: unknown }> = [];
// Routed by URL fragment rather than taken from a queue: connecting also
// starts the background instance/quota polling, and a shared queue lets that
// traffic consume the response meant for the call under test.
let routes: Array<{ match: string; res: FakeResponse }> = [];

beforeAll(() => {
  const backing = new Map<string, string>();
  (globalThis as Record<string, unknown>).localStorage = {
    getItem: (k: string) => backing.get(k) ?? null,
    setItem: (k: string, v: string) => backing.set(k, String(v)),
    removeItem: (k: string) => backing.delete(k),
    clear: () => backing.clear(),
  };
});

/** Script the answer for any request whose URL contains `match`. */
function route(match: string, res: FakeResponse): void {
  // Re-routing the same fragment replaces the previous answer: a test that
  // scripts 404 and then 200 for the same route is scripting a CHANGE, not
  // two alternatives.
  routes = routes.filter((r) => r.match !== match);
  routes.push({ match, res });
}

beforeEach(async () => {
  requested = [];
  routes = [];
  // The store is a module singleton, so state from the previous test would
  // otherwise ride along — a leftover nonce is indistinguishable from a real
  // one and would make the guards look broken. disconnectHub runs the same
  // connection-reset patch a hub switch does, which is what clears the
  // configuration state under test.
  const useAppStore = await store();
  useAppStore.getState().disconnectHub();
  (globalThis as Record<string, unknown>).fetch = async (
    input: unknown,
    init?: { method?: string; body?: string },
  ): Promise<Response> => {
    const url = String(input);
    requested.push({
      url,
      method: init?.method ?? "GET",
      ...(init?.body ? { body: JSON.parse(init.body) as unknown } : {}),
    });
    const hit = routes
      .filter((r) => url.includes(r.match))
      // Longest match wins: `/settings/reset-cards/use` also contains
      // `/settings/reset-cards`, and the generic route would otherwise answer
      // for the write it was never meant to cover.
      .sort((a, b) => b.match.length - a.match.length)[0];
    if (!hit) throw new Error(`test bug: no scripted response for ${url}`);
    return hit.res as unknown as Response;
  };
});

afterEach(() => {
  vi.unstubAllGlobals();
});

const ok = (body: unknown): FakeResponse => ({
  ok: true,
  status: 200,
  json: async () => body,
  text: async () => JSON.stringify(body),
});

/** A refused write: the settings API answers 4xx with a reason in `error`. */
const refused = (status: number, error: string): FakeResponse => ({
  ok: false,
  status,
  json: async () => ({ ok: false, error }),
  text: async () => JSON.stringify({ ok: false, error }),
});

/** A response that resolves after `ms`, to order two in-flight reads. */
const delayed = (res: FakeResponse, ms: number) => ({
  ok: res.ok,
  status: res.status,
  json: async () => {
    await new Promise((r) => setTimeout(r, ms));
    return (await res.json()) as unknown;
  },
  text: async () => {
    await new Promise((r) => setTimeout(r, ms));
    return res.text();
  },
});

async function store() {
  // Imported lazily so the localStorage stub above is in place first.
  const mod = await import("../src/store/appStore");
  return mod.useAppStore;
}

/** Put the store in a connected state pointed at a stub hub. */
async function connectedStore() {
  const useAppStore = await store();
  useAppStore.getState().connectToHub({ hubUrl: "http://hub/", token: "tok" });
  return useAppStore;
}

// --- the reset-card nonce -------------------------------------------------

test("a spend refused with 409 refreshes the nonce instead of leaving it dead", async () => {
  const useAppStore = await connectedStore();
  // 409 means the nonce is no longer the one the server issued — a newer status
  // read replaced it, or the user switched provider. The button would then be
  // permanently dead with no way out but finding the refresh control by trial
  // and error. (A spend that merely failed mid-request is NOT a 409: the route
  // burns the nonce only after the call settles, so that retry still works.)
  route("reset-cards", ok({ resetCards: { availableFiveHour: [], nonce: "n-1" } }));
  await useAppStore.getState().loadResetCards("account:bigmodel-individual-coding-plan");
  expect(useAppStore.getState().resetNonce).toBe("n-1");

  route("reset-cards/use", refused(409, "nonce is stale — refresh the reset card status"));
  route("reset-cards?", ok({ resetCards: { availableFiveHour: [], nonce: "n-2" } }));
  const spent = await useAppStore.getState().spendResetCard({
    providerId: "account:bigmodel-individual-coding-plan",
    nonce: "n-1",
    resetType: "FIVE_HOUR",
  });

  expect(spent).toBe(false);
  // A fresh status read replaced the dead nonce, so the next tap can work.
  expect(useAppStore.getState().resetNonce).toBe("n-2");
  // And the user is told why, rather than shown a bare failure.
  expect(useAppStore.getState().notice).toBe("notice.configResetStale");
});

test("a spend refused for another reason does not silently re-read the status", async () => {
  const useAppStore = await connectedStore();
  route("reset-cards", ok({ resetCards: { availableFiveHour: [], nonce: "n-1" } }));
  await useAppStore.getState().loadResetCards("account:bigmodel-individual-coding-plan");

  // A non-409 refusal (an eligibility or upstream failure) says nothing about
  // the nonce, so re-reading it would mask the real error with a second
  // request the user did not ask for.
  route("reset-cards/use", refused(403, "reset cards require an account coding-plan provider id"));
  const spent = await useAppStore.getState().spendResetCard({
    providerId: "account:bigmodel-individual-coding-plan",
    nonce: "n-1",
    resetType: "FIVE_HOUR",
  });

  expect(spent).toBe(false);
  expect(useAppStore.getState().resetNonce).toBe("n-1");
  // One status read and one refused spend — no extra read on the way out.
  const resetCalls = requested.filter((r) => r.url.includes("/settings/reset-cards"));
  expect(resetCalls).toHaveLength(2);
});

test("a slow status read cannot overwrite the provider the user switched to", async () => {
  const useAppStore = await connectedStore();
  // The provider picker fires a read per tap. Queue both answers BEFORE the
  // calls: each read awaits its fetch, so a queue filled afterwards is already
  // too late. The first call resolves LAST — that is the race, and it is the
  // response that would otherwise land on top of the user's newer choice.
  route(
    "providerId=account%3Abigmodel-individual-coding-plan",
    delayed(ok({ resetCards: { availableFiveHour: [], nonce: "n-bigmodel" } }), 20),
  );
  route(
    "providerId=account%3Azai-individual-coding-plan",
    ok({ resetCards: { availableFiveHour: [], nonce: "n-zai" } }),
  );

  await Promise.all([
    useAppStore
      .getState()
      .loadResetCards("account:bigmodel-individual-coding-plan"),
    useAppStore.getState().loadResetCards("account:zai-individual-coding-plan"),
  ]);

  // The nonce and the providerId must name the SAME provider, or the spend
  // pairs a nonce with a provider it was never minted for and is refused with
  // a 409 that reads as a broken button.
  expect(useAppStore.getState().resetProviderId).toBe(
    "account:zai-individual-coding-plan",
  );
  expect(useAppStore.getState().resetNonce).toBe("n-zai");
});

test("a spend refuses when the user has switched provider since the gesture began", async () => {
  const useAppStore = await connectedStore();
  route("reset-cards", ok({ resetCards: { availableFiveHour: [], nonce: "n-1" } }));
  await useAppStore.getState().loadResetCards("account:bigmodel-individual-coding-plan");

  // The gesture captured provider A and its nonce. Before the spend leaves,
  // the user taps provider B — a switch that lands while the opportunity
  // request is in flight. Spending A's nonce against B's id is refused (the
  // backend pairs them and answers 409, reading as a broken button), and
  // spending B's card when the user confirmed A is worse, so the spend must
  // not go out at all.
  route("reset-cards", ok({ resetCards: { availableFiveHour: [], nonce: "n-2" } }));
  await useAppStore.getState().loadResetCards("account:zai-individual-coding-plan");

  const before = requested.filter((r) => r.url.includes("/reset-cards/use")).length;
  const spent = await useAppStore.getState().spendResetCard({
    providerId: "account:bigmodel-individual-coding-plan",
    nonce: "n-1",
    resetType: "FIVE_HOUR",
  });

  expect(spent).toBe(false);
  expect(requested.filter((r) => r.url.includes("/reset-cards/use")).length).toBe(before);
});

// --- what survives a hub switch ------------------------------------------

test("switching hubs re-probes the settings capability instead of trusting the old verdict", async () => {
  const useAppStore = await connectedStore();
  // The first hub predates the settings API, so the probe answers 404 and the
  // screens explain the gap.
  route("settings/all", refused(404, "not found"));
  await useAppStore.getState().loadConfigAll();
  expect(useAppStore.getState().configSupported).toBe(false);

  // The second hub is current. Its capability verdict must be discovered, not
  // inherited: `configSupported` only re-probes while it is still null, so a
  // stale false is permanent and every config screen reports "unsupported".
  useAppStore.getState().switchServer(useAppStore.getState().savedServers[0]!.id);
  expect(useAppStore.getState().configSupported).toBeNull();

  route(
    "settings/all",
    ok({
      ok: true,
      models: {},
      skills: [],
      resetCards: { providers: [], credentials: true },
    }),
  );
  await useAppStore.getState().loadConfigAll();
  expect(useAppStore.getState().configSupported).toBe(true);
});

test("switching hubs drops the previous machine's payloads", async () => {
  const useAppStore = await connectedStore();
  route("settings/models", ok({ ok: true, models: { available: [{ modelId: "glm-4.6" }] } }));
  await useAppStore.getState().loadConfigSection("models");
  expect(useAppStore.getState().configModels).not.toBeNull();

  useAppStore.getState().switchServer(useAppStore.getState().savedServers[0]!.id);
  // Otherwise the new hub's model screen flashes the old machine's list
  // before its own payload lands.
  expect(useAppStore.getState().configModels).toBeNull();
  expect(useAppStore.getState().configAll).toBeNull();
});

test("switching hubs clears the pending-restart flag", async () => {
  const useAppStore = await connectedStore();
  // A needs-restart write was made to THIS bridge, so the flag is armed and the
  // screens offer a restart. The next hub's backend never saw that write —
  // restarting it would cancel its in-flight turns to apply nothing, so the
  // flag must not travel with the user.
  useAppStore.setState({ pendingRestart: true });
  expect(useAppStore.getState().pendingRestart).toBe(true);

  useAppStore.getState().switchServer(useAppStore.getState().savedServers[0]!.id);
  expect(useAppStore.getState().pendingRestart).toBe(false);
});

test("a spend reuses the idempotency key minted with its nonce", async () => {
  const useAppStore = await connectedStore();
  const providerId = "account:bigmodel-individual-coding-plan";
  route("reset-cards", ok({ resetCards: { availableFiveHour: [], nonce: "n-1" } }));
  await useAppStore.getState().loadResetCards(providerId);
  const minted = useAppStore.getState().resetIdempotencyKey;
  expect(minted).toBeTruthy();

  route("reset-cards/use", ok({ ok: true, used: true }));
  route("reset-cards?", ok({ resetCards: { availableFiveHour: [], nonce: "n-2" } }));
  await useAppStore.getState().spendResetCard({ providerId, nonce: "n-1", resetType: "WEEK" });

  // The key is what makes a RETRY safe: the backend keys the spend on it, so
  // the same value answers the same outcome instead of burning a second card.
  // Generating one per attempt (which this store used to do) is exactly what
  // defeats that, and a mobile-network timeout followed by a retry is the case
  // where it costs a card.
  const spend = requested.find((r) => r.url.includes("reset-cards/use"));
  expect((spend?.body as { idempotencyKey?: string }).idempotencyKey).toBe(minted);
});

test("a retried spend after a timeout sends the same idempotency key", async () => {
  const useAppStore = await connectedStore();
  const providerId = "account:bigmodel-individual-coding-plan";
  route("reset-cards", ok({ resetCards: { availableFiveHour: [], nonce: "n-1" } }));
  await useAppStore.getState().loadResetCards(providerId);
  const minted = useAppStore.getState().resetIdempotencyKey;

  // First attempt: the bridge answers 502 — "your request may have been
  // carried out". The card may already be gone, so the client MUST retry with
  // the same key rather than mint a new one.
  route("reset-cards/use", refused(502, "coding_plan_reset_http_error:502"));
  await useAppStore.getState().spendResetCard({ providerId, nonce: "n-1", resetType: "WEEK" });
  route("reset-cards/use", ok({ ok: true, used: true }));
  route("reset-cards?", ok({ resetCards: { availableFiveHour: [], nonce: "n-2" } }));
  await useAppStore.getState().spendResetCard({ providerId, nonce: "n-1", resetType: "WEEK" });

  const spends = requested.filter((r) => r.url.includes("reset-cards/use"));
  expect(spends).toHaveLength(2);
  expect((spends[0]?.body as { idempotencyKey?: string }).idempotencyKey).toBe(minted);
  expect((spends[1]?.body as { idempotencyKey?: string }).idempotencyKey).toBe(minted);
});

test("a failed provider read drops the previous provider's cards", async () => {
  const useAppStore = await connectedStore();
  const a = "account:bigmodel-individual-coding-plan";
  const b = "account:zai-coding-plan";
  route("reset-cards", ok({ resetCards: { availableFiveHour: [], nonce: "n-a" } }));
  await useAppStore.getState().loadResetCards(a);
  expect(useAppStore.getState().resetNonce).toBe("n-a");

  // Provider B's read fails. Holding A's inventory while the user is looking
  // at B leaves a nonce that belongs to another plan's cards, and the next
  // spend sends it — the route always rejects a mismatched nonce with 409, so
  // the button dies with no visible cause.
  route("reset-cards", refused(500, "credentials_unavailable"));
  await useAppStore.getState().loadResetCards(b);
  expect(useAppStore.getState().resetProviderId).toBe(b);
  expect(useAppStore.getState().resetNonce).toBeNull();
  expect(useAppStore.getState().resetIdempotencyKey).toBeNull();
  expect(useAppStore.getState().resetCards).toBeNull();
});

test("a usage response for a superseded range is dropped", async () => {
  const useAppStore = await connectedStore();
  const usage = (range: string) => ({ ok: true, usage: { available: true, range, models: [] } });
  // "All" answers slowly, "7d" fast — the order the taps happen in is not the
  // order the answers arrive in, and nothing serializes the two reads.
  route("settings/usage?range=all", delayed(ok(usage("all")), 60));
  route("settings/usage?range=7d", delayed(ok(usage("7d")), 5));

  const all = useAppStore.getState().loadConfigSection("usage", { range: "all" });
  const week = useAppStore.getState().loadConfigSection("usage", { range: "7d" });
  await Promise.all([all, week]);

  // The 7d answer is the newest ask, so it is what the screen shows. Storing
  // the slower All response underneath it would put whole-history numbers
  // under the 7d button.
  expect(useAppStore.getState().configUsageRange).toBe("7d");
  expect(useAppStore.getState().configUsage?.range).toBe("7d");
});

test("a backend restart that did not close the old process keeps the flag armed", async () => {
  const useAppStore = await connectedStore();
  useAppStore.setState({ instanceId: "1", pendingRestart: true });
  // `closed: false` means the old subprocess survived, so the writes this
  // restart existed to apply did NOT take effect. Clearing the flag here would
  // remove the only affordance that can apply them, leaving the user with
  // saved changes that never land and no button to press.
  route("backend/restart", ok({ cancelledTurns: 0, closed: false }));
  await useAppStore.getState().restartConfigBackend();
  expect(useAppStore.getState().pendingRestart).toBe(true);
});

test("a backend restart that closed the old process clears the flag", async () => {
  const useAppStore = await connectedStore();
  useAppStore.setState({ instanceId: "1", pendingRestart: true });
  route("backend/restart", ok({ cancelledTurns: 2, closed: true }));
  const cancelled = await useAppStore.getState().restartConfigBackend();
  expect(cancelled).toBe(2);
  expect(useAppStore.getState().pendingRestart).toBe(false);
});
