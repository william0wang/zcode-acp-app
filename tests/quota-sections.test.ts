// @vitest-environment node
// The account-quota read (/api/quota) must carry EVERY section the hub sends
// through to the store.
//
// This is a regression lock, not general coverage: `parseUsageStats` rebuilds
// the payload field by field instead of passing it through, so a section the
// parser does not know about is discarded before any screen can render it.
// That is exactly how the CLIProxy section shipped twice — the types and the
// card were added, the parser was not, and the rows were dropped in silence.
// The assertion is therefore on the STORE STATE after a real refresh, never on
// a locally re-implemented decode: a copy would pass while the app drops the
// data.
import { beforeAll, beforeEach, expect, test, vi } from "vitest";

interface FakeResponse {
  ok: boolean;
  status: number;
  json: () => Promise<unknown>;
}

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

beforeEach(async () => {
  routes = [];
  const useAppStore = await store();
  useAppStore.getState().disconnectHub();
  (globalThis as Record<string, unknown>).fetch = async (
    input: unknown,
  ): Promise<Response> => {
    const url = String(input);
    const hit = routes
      .filter((r) => url.includes(r.match))
      .sort((a, b) => b.match.length - a.match.length)[0];
    if (!hit) throw new Error(`test bug: no scripted response for ${url}`);
    return hit.res as unknown as Response;
  };
});

async function store() {
  const mod = await import("../src/store/appStore");
  return mod.useAppStore;
}

const ok = (body: unknown): FakeResponse => ({
  ok: true,
  status: 200,
  json: async () => body,
});

/**
 * The shape a bridge 0.66+ hub answers for /api/quota: the three long-standing
 * sections plus the CLIProxy pool. `combined` is the pool-wide row set the
 * card paints; `accounts` is the per-subscription detail it ignores.
 */
function hubQuotaPayload() {
  return {
    glm: {
      kind: "success",
      level: "pro",
      items: [
        { key: "five_hour", label: "5h limit", usedPercent: 12 },
        { key: "mcp", label: "MCP tools", usedPercent: 5, usedCount: 5, totalCount: 100 },
      ],
    },
    opencode: {
      kind: "success",
      windows: [
        { key: "rolling", label: "5h", usagePercent: 8, resetsAt: 1_792_000_000_000 },
      ],
    },
    ollama: {
      kind: "success",
      windows: [
        { key: "weekly", label: "Week", usagePercent: 3, resetsAt: 1_792_100_000_000 },
      ],
    },
    cliproxy: {
      kind: "success",
      accounts: [
        {
          label: "antutu1ww",
          models: [
            {
              name: "Gemini Models",
              windows: [
                { window: "5h", usedPercent: 0, resetsAt: 1_792_499_907_000 },
                { window: "week", usedPercent: 4.222184, resetsAt: 1_792_905_568_000 },
              ],
            },
          ],
        },
      ],
      combined: [
        { window: "week", usedPercent: 2.8815745, resetsAt: 1_792_718_293_000 },
        { window: "5h", usedPercent: 0, resetsAt: 1_792_499_907_000 },
      ],
      otherAccounts: 2,
    },
  };
}

test("the CLIProxy pool survives the quota decode", async () => {
  routes.push({ match: "/api/quota/dock", res: ok({ formatted: null }) });
  routes.push({ match: "/api/quota", res: ok(hubQuotaPayload()) });

  const useAppStore = await store();
  useAppStore.getState().connectToHub({ hubUrl: "http://hub/", token: "tok" });
  await useAppStore.getState().refreshUsageStats();

  const stats = useAppStore.getState().usageStats;
  expect(stats).not.toBeNull();
  expect(stats?.cliproxy).toBeDefined();
  expect(stats?.cliproxy?.kind).toBe("success");
  expect(stats?.cliproxy?.combined).toHaveLength(2);
  expect(stats?.cliproxy?.combined?.[0]).toEqual({
    window: "week",
    usedPercent: 2.8815745,
    resetsAt: 1_792_718_293_000,
  });
  expect(stats?.cliproxy?.otherAccounts).toBe(2);
});

test("a configured-but-failing pool keeps its kind instead of vanishing", async () => {
  // auth_error / unavailable are what the card's status line renders. Losing
  // the section entirely would hide the failure behind an absent heading.
  routes.push({ match: "/api/quota/dock", res: ok({ formatted: null }) });
  routes.push({
    match: "/api/quota",
    res: ok({ ...hubQuotaPayload(), cliproxy: { kind: "auth_error" } }),
  });

  const useAppStore = await store();
  useAppStore.getState().connectToHub({ hubUrl: "http://hub/", token: "tok" });
  await useAppStore.getState().refreshUsageStats();

  const stats = useAppStore.getState().usageStats;
  expect(stats?.cliproxy?.kind).toBe("auth_error");
  expect(stats?.cliproxy?.combined).toBeUndefined();
});

test("a pre-0.66 hub without the section still decodes the older three", async () => {
  const legacy = hubQuotaPayload() as Record<string, unknown>;
  delete legacy.cliproxy;
  routes.push({ match: "/api/quota/dock", res: ok({ formatted: null }) });
  routes.push({ match: "/api/quota", res: ok(legacy) });

  const useAppStore = await store();
  useAppStore.getState().connectToHub({ hubUrl: "http://hub/", token: "tok" });
  await useAppStore.getState().refreshUsageStats();

  const stats = useAppStore.getState().usageStats;
  expect(stats).not.toBeNull();
  expect(stats?.cliproxy).toBeUndefined();
  expect(stats?.glm.kind).toBe("success");
  expect(stats?.opencode.windows).toHaveLength(1);
  expect(stats?.ollama?.windows).toHaveLength(1);
});
