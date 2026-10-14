// @vitest-environment node
// REST-layer coverage for HubClient: payload validation, error mapping
// (401 / HTTP / network), verbatim cursor pagination, fs line-window
// parsing, and the transparent 404 retry after a bridge upgrade.
import { beforeEach, expect, test } from "vitest";
import { HubApiError, HubClient } from "../src/lib/hub";

let responses: Response[] = [];
let requested: string[] = [];

function install(): void {
  (globalThis as Record<string, unknown>).fetch = async (
    input: unknown,
  ): Promise<Response> => {
    const url = String(input);
    requested.push(url);
    const next = responses.shift();
    if (!next) throw new Error("test bug: no scripted response left");
    return next;
  };
}

beforeEach(() => {
  responses = [];
  requested = [];
  install();
});

const client = () => new HubClient("http://hub/", "tok");

test("instances parses the payload and forwards the probe flag", async () => {
  responses.push(
    Response.json([{ id: "inst1", workspace: "/w", sessions: [] }]),
  );
  await expect(client().instances()).resolves.toHaveLength(1);
  expect(requested[0]).toBe("http://hub/api/instances");

  responses.push(Response.json([]));
  await client().instances(true);
  expect(requested[1]).toBe("http://hub/api/instances?probe=1");
});

test("instances rejects a non-array payload", async () => {
  responses.push(Response.json({}));
  await expect(client().instances()).rejects.toThrow(
    "unexpected /api/instances payload",
  );
});

test("401 keeps its status; other HTTP errors surface as 'HTTP <code>'", async () => {
  responses.push(new Response("no", { status: 401 }));
  const err = await client()
    .health()
    .catch((e: unknown) => e);
  expect(err).toBeInstanceOf(HubApiError);
  expect(err).toMatchObject({
    status: 401,
    message: "unauthorized: check token",
  });

  responses.push(new Response("boom", { status: 502 }));
  await expect(client().health()).rejects.toMatchObject({
    status: 502,
    message: "HTTP 502",
  });
});

test("fetch failures are marked network (expected hub-offline state)", async () => {
  (globalThis as Record<string, unknown>).fetch = async () => {
    throw new TypeError("dispatch error");
  };
  const err = await client()
    .health()
    .catch((e: unknown) => e);
  expect(err).toBeInstanceOf(HubApiError);
  expect(err).toMatchObject({ network: true });
});

test("projectSessions passes the composite cursor verbatim", async () => {
  responses.push(Response.json({ sessions: [], nextCursor: null }));
  await client().projectSessions("/my proj", {
    before: 1700000000,
    beforeId: "row9",
  });
  const url = new URL(requested[0]);
  expect(url.pathname).toBe("/api/projects/sessions");
  expect(url.searchParams.get("workspacePath")).toBe("/my proj");
  expect(url.searchParams.get("before")).toBe("1700000000");
  expect(url.searchParams.get("beforeId")).toBe("row9");
});

test("fsBrowse omits the query for home and encodes a path when given", async () => {
  responses.push(
    Response.json({
      path: "/Users/me",
      parent: "/Users",
      creatable: false,
      entries: [],
      truncated: false,
    }),
  );
  await client().fsBrowse();
  expect(requested[0]).toBe("http://hub/api/fs/list");
  responses.push(
    Response.json({
      path: "/Users/me/My Dir",
      parent: "/Users/me",
      creatable: true,
      entries: [{ name: "src" }],
      truncated: false,
    }),
  );
  const listing = await client().fsBrowse("/Users/me/My Dir");
  expect(requested[1]).toBe(
    "http://hub/api/fs/list?path=%2FUsers%2Fme%2FMy%20Dir",
  );
  expect(listing.creatable).toBe(true);
  expect(listing.entries).toEqual([{ name: "src" }]);
});

test("fsBrowse surfaces a 404 (old bridge or gone directory) with its status", async () => {
  responses.push(new Response("not found", { status: 404 }));
  const err = await client()
    .fsBrowse()
    .catch((e: unknown) => e);
  expect(err).toBeInstanceOf(HubApiError);
  expect(err).toMatchObject({ status: 404 });
});

test("deleteSession POSTs the per-instance tombstone route", async () => {
  const calls: Array<{ url: string; method: string }> = [];
  (globalThis as Record<string, unknown>).fetch = async (
    input: unknown,
    init?: { method: string },
  ) => {
    calls.push({ url: String(input), method: init?.method ?? "GET" });
    return Response.json({ ok: true, deleted: true });
  };
  await client().deleteSession("inst 1", "sess/a");
  expect(calls).toEqual([
    {
      // The session id is encoded, the instance id rides raw — same shape
      // as closeSession/renameSession (instance ids are hub-numeric).
      url: "http://hub/api/instances/inst 1/sessions/sess%2Fa/delete",
      method: "POST",
    },
  ]);
});

test("deleteProject POSTs the workspace tombstone route with the path in the body", async () => {
  const calls: Array<{ url: string; method: string; body: unknown }> = [];
  (globalThis as Record<string, unknown>).fetch = async (
    input: unknown,
    init?: { method?: string; body?: unknown },
  ) => {
    calls.push({
      url: String(input),
      method: init?.method ?? "GET",
      body: JSON.parse(String(init?.body ?? "null")),
    });
    return Response.json({ ok: true, deletedTasks: 3 });
  };
  await client().deleteProject("/Users/me/my proj");
  expect(calls).toEqual([
    {
      url: "http://hub/api/projects/delete",
      method: "POST",
      // Hub-level route (no instance scoping); the workspace path is JSON,
      // not a query param, so any path shape survives unencoded.
      body: { workspacePath: "/Users/me/my proj" },
    },
  ]);
});

test("fsFileText trusts X-Zcode-First-Line and strips the trailing newline", async () => {
  responses.push(
    new Response("a\nb\nc\n", {
      headers: { "X-Zcode-First-Line": "7" },
    }),
  );
  await expect(
    client().fsFileText("inst", "sess", "/f.txt", 1, 3),
  ).resolves.toEqual({
    firstLine: 7,
    text: "a\nb\nc",
  });
});

test("fsFileText falls back to the requested line without the header", async () => {
  responses.push(new Response("only\n"));
  await expect(
    client().fsFileText("inst", "sess", "/f.txt", 4, 1),
  ).resolves.toEqual({
    firstLine: 4,
    text: "only",
  });
});

test("fsFileUrl embeds token + session in the query for <img src>", () => {
  const url = new URL(client().fsFileUrl("inst", "sess", "/a b.png"));
  expect(url.pathname).toBe("/api/instances/inst/fs/file");
  expect(url.searchParams.get("sessionId")).toBe("sess");
  expect(url.searchParams.get("path")).toBe("/a b.png");
  expect(url.searchParams.get("token")).toBe("tok");
});

test("a 404 right after a bridge upgrade is retried once transparently", async () => {
  responses.push(new Response("stale route", { status: 404 }));
  responses.push(Response.json({ entries: [] }));
  await expect(client().fsList("inst", "sess", "/")).resolves.toEqual({
    entries: [],
  });
  expect(requested).toHaveLength(2);
});

test("a gone bridge (unknown instance / bridge unreachable) is flagged instanceDead", async () => {
  // The retry-404-once path applies here too — the SECOND answer decides.
  responses.push(new Response("unknown instance", { status: 404 }));
  responses.push(new Response("unknown instance", { status: 404 }));
  const err = await client().fsList("inst", "sess", "/").catch((e) => e);
  expect(err).toBeInstanceOf(HubApiError);
  expect(err).toMatchObject({ status: 404, instanceDead: true });

  responses.push(new Response("bridge unreachable", { status: 502 }));
  const err2 = await client()
    .fsFileText("inst", "sess", "/f.txt", 1, 5)
    .catch((e) => e);
  expect(err2).toBeInstanceOf(HubApiError);
  expect(err2).toMatchObject({ status: 502, instanceDead: true });
});

test("a genuine 404 (missing path) is not flagged instanceDead", async () => {
  responses.push(new Response("not found", { status: 404 }));
  responses.push(new Response("not found", { status: 404 }));
  // Body is plain text, so the caller's res.json() throws — the pre-existing
  // behavior for missing paths, unchanged by the instanceDead plumbing.
  await expect(client().fsList("inst", "sess", "nope")).rejects.toThrow();
});

test("systemStats GETs the machine-level system-stats route", async () => {
  responses.push(
    Response.json({
      collectedAt: 1791106915456,
      host: {
        hostname: "Mac.local",
        osVersion: "15.8 (24H23)",
        model: "Mac14,9",
        chip: "Apple M2 Pro",
      },
      uptimeS: 528592,
      cpu: { cores: 10, usagePct: 12.3, loadAvg: [2.5, 2.4, 2.4] },
      memory: {
        totalBytes: 17_179_869_184,
        availableBytes: 5_428_199_424,
        usedBytes: 11_751_669_760,
        swapTotalBytes: 2_147_483_648,
        swapUsedBytes: 978_384_322,
      },
      storage: {
        root: {
          totalBytes: 494_384_795_648,
          usedBytes: 350_988_324_864,
          availableBytes: 143_396_470_784,
        },
        home: null,
      },
      battery: {
        present: true,
        percent: 99,
        powerSource: "battery",
        status: "discharging",
        remainingMin: 1200,
      },
      power: {
        preventSleep: true,
        sleepHolders: ["Amphetamine"],
        cpuSpeedLimitPct: null,
      },
      network: {
        addresses: ["10.0.0.2"],
        ssid: "HomeNet",
        rxBytesPerS: 11633,
        txBytesPerS: 450198,
      },
      processes: {
        hub: { pid: 38402, rssBytes: 69_255_168, cpuPct: 1.2 },
        bridges: [
          {
            id: "i1",
            workspace: "/w/proj",
            pid: 123,
            rssBytes: 300_000_000,
            cpuPct: 4.2,
          },
        ],
      },
      hub: { version: "0.62.0", uptimeS: 9000, instances: 1 },
    }),
  );
  const stats = await client().systemStats();
  expect(requested[0]).toBe("http://hub/api/system-stats");
  expect(stats.cpu.usagePct).toBe(12.3);
  expect(stats.power.sleepHolders).toEqual(["Amphetamine"]);
  expect(stats.processes.bridges).toHaveLength(1);
});

test("systemStats surfaces a 404 hub (route not served yet) as a HubApiError with status", async () => {
  responses.push(new Response("no route", { status: 404 }));
  const err = await client()
    .systemStats()
    .catch((e: unknown) => e);
  expect(err).toBeInstanceOf(HubApiError);
  expect(err).toMatchObject({ status: 404 });
});
