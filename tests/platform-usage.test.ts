// @vitest-environment node
// Store rules for the platform-usage read (the Usage page's Platform tab).
//
// Mirrors the local-usage rules in config-store-rules.test.ts: the response
// envelope must be unwrapped (`{ok, platformUsage}`, not the whole body), a
// late answer for a superseded range must be dropped, and — specific to this
// section — a 404 from an OLD bridge must degrade the tab alone instead of
// flipping the global configSupported verdict that gates every section.
import { afterEach, beforeAll, beforeEach, expect, test, vi } from "vitest";

interface FakeResponse {
  ok: boolean;
  status: number;
  json: () => Promise<unknown>;
  text: () => Promise<string>;
}

let requested: Array<{ url: string; method: string }> = [];
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

function route(match: string, res: FakeResponse): void {
  routes = routes.filter((r) => r.match !== match);
  routes.push({ match, res });
}

beforeEach(async () => {
  requested = [];
  routes = [];
  const useAppStore = await store();
  useAppStore.getState().disconnectHub();
  (globalThis as Record<string, unknown>).fetch = async (
    input: unknown,
    init?: { method?: string },
  ): Promise<Response> => {
    const url = String(input);
    requested.push({ url, method: init?.method ?? "GET" });
    const hit = routes
      .filter((r) => url.includes(r.match))
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
  const mod = await import("../src/store/appStore");
  return mod.useAppStore;
}

async function connectedStore() {
  const useAppStore = await store();
  useAppStore.getState().connectToHub({ hubUrl: "http://hub/", token: "tok" });
  return useAppStore;
}

/** A full platform-usage payload with distinguishable values. */
function platformPayload(over: Record<string, unknown> = {}) {
  return {
    kind: "success",
    range: "7d",
    generatedAt: 1,
    activity: {
      summary: {
        totalTokens: 1_200_000,
        peakDailyTokens: 86_000,
        peakDailyTokensDate: "2026-09-12",
        totalUsageDurationMs: 4_980_000,
        currentStreakDays: 6,
        longestStreakDays: 41,
      },
      heatmap: {
        startDate: "2025-10-01",
        endDate: "2026-09-30",
        maxTokens: 86_000,
        weeks: [
          {
            days: [
              null,
              { date: "2025-10-05", level: 2, tokens: 40_000 },
              null,
              null,
              null,
              null,
              null,
            ],
          },
        ],
      },
    },
    detail: {
      model: {
        cacheHitRate: 72.4,
        cacheHitRateTrend: 3.1,
        totalCredits: 3400,
        totalCreditsTrend: -2,
        averageDailyCredits: 120,
        averageDailyCreditsTrend: 5,
      },
      tool: null,
    },
    models: [
      {
        name: "glm-4.7",
        totalTokens: 800_000,
        totalCredits: 2000,
        sortOrder: 0,
      },
    ],
    tools: [{ name: "web-reader", totalUsageCount: 214, totalCredits: 12 }],
    series: {
      granularity: "day",
      xTime: ["09-24", "09-25"],
      totals: [1000, 2000],
    },
    ...over,
  };
}

test("the platformUsage block is unwrapped from the {ok, platformUsage} envelope", async () => {
  route(
    "settings/usage-platform",
    ok({ ok: true, platformUsage: platformPayload() }),
  );
  const useAppStore = await connectedStore();
  await useAppStore.getState().loadConfigSection("usagePlatform");

  const state = useAppStore.getState();
  // Storing the whole response instead of the block is the historical bug this
  // guards (same class as the local-usage one): every field read would be
  // undefined and the tab would sit on its empty state forever.
  expect(
    (state.configPlatformUsage as unknown as Record<string, unknown>)["ok"],
  ).toBeUndefined();
  expect(state.configPlatformUsage?.activity?.summary.totalTokens).toBe(
    1_200_000,
  );
  expect(
    state.configPlatformUsage?.activity?.heatmap.weeks[0]?.days[1]?.level,
  ).toBe(2);
  expect(state.configPlatformUsage?.models).toHaveLength(1);
  expect(state.configPlatformUsage?.series?.totals).toEqual([1000, 2000]);
  expect(
    requested.some((r) => r.url.includes("settings/usage-platform?range=7d")),
  ).toBe(true);
});

test("a platform-usage answer for a superseded range is dropped", async () => {
  route(
    "settings/usage-platform?range=today",
    delayed(
      ok({ ok: true, platformUsage: platformPayload({ range: "today" }) }),
      60,
    ),
  );
  route(
    "settings/usage-platform?range=7d",
    delayed(
      ok({ ok: true, platformUsage: platformPayload({ range: "7d" }) }),
      5,
    ),
  );
  const useAppStore = await connectedStore();

  const today = useAppStore
    .getState()
    .loadConfigSection("usagePlatform", { range: "today" });
  const week = useAppStore
    .getState()
    .loadConfigSection("usagePlatform", { range: "7d" });
  await Promise.all([today, week]);

  const state = useAppStore.getState();
  expect(state.configPlatformUsageRange).toBe("7d");
  expect(state.configPlatformUsage?.range).toBe("7d");
});

test("a 404 (old bridge) degrades the platform tab, not the whole settings API", async () => {
  route("settings/usage-platform", refused(404, "not found"));
  const useAppStore = await connectedStore();
  await useAppStore.getState().loadConfigSection("usagePlatform");

  const state = useAppStore.getState();
  expect(state.configPlatformUsageSupported).toBe(false);
  // The global verdict belongs to /settings/all; a missing route here must not
  // mark every other section unsupported.
  expect(state.configSupported).not.toBe(false);
  expect(state.configError).toBeNull();
});

test("error kinds pass through to the tab's own empty state", async () => {
  route(
    "settings/usage-platform",
    ok({
      ok: true,
      platformUsage: { kind: "auth_error", range: "7d", generatedAt: 1 },
    }),
  );
  const useAppStore = await connectedStore();
  await useAppStore.getState().loadConfigSection("usagePlatform");

  expect(useAppStore.getState().configPlatformUsage?.kind).toBe("auth_error");
  expect(useAppStore.getState().configPlatformUsageSupported).toBe(true);
});
