// @vitest-environment node
// Store-level rules for the dynamic-workflow screens (bridge 0.48.0, server
// ADR-0029): the scope guard on list reads, the launch memory a start must
// leave behind (the ONLY run→session join the app will ever have), the
// prefill contract with the composer, and the silent-degrade reads.
import { afterEach, beforeAll, beforeEach, expect, test, vi } from "vitest";

interface FakeResponse {
  ok: boolean;
  status: number;
  json: () => Promise<unknown>;
  text: () => Promise<string>;
}

let requested: Array<{ url: string; method: string; body?: unknown }> = [];
let routes: Array<{ match: string; res: FakeResponse }> = [];
let backing: Map<string, string> = new Map();

beforeAll(() => {
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
  backing = new Map();
  const useAppStore = await store();
  useAppStore.getState().disconnectHub();
  // The toast auto-clears on a 4s timer; a failure toast from the previous
  // test must not bleed into the next test's silence assertions.
  useAppStore.setState({ toast: null });
  (globalThis as Record<string, unknown>).fetch = async (
    input: unknown,
    init?: { method?: string; body?: string },
  ): Promise<Response> => {
    const url = String(input);
    // connectToHub starts the background instance/quota polling. Serve it a
    // standing answer OUTSIDE the routes and the request log — the poll's
    // URLs contain "api/instances" and would otherwise win the longest-match
    // routing over the per-instance workflow calls under test.
    if (/\/api\/instances(\?|$)/.test(url) || url.includes("/api/quota")) {
      return Response.json([]) as unknown as Response;
    }
    requested.push({
      url,
      method: init?.method ?? "GET",
      ...(init?.body ? { body: JSON.parse(init.body) as unknown } : {}),
    });
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

const refused = (status: number, body: unknown): FakeResponse => ({
  ok: false,
  status,
  json: async () => body,
  text: async () => JSON.stringify(body),
});

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
  // The workflow routes are per-instance; without a bridge named they all
  // answer 409 from the hub's machine-level mount.
  useAppStore.setState({ instanceId: "1" });
  return useAppStore;
}

test("loadWorkflows stores the list for the scope asked", async () => {
  const useAppStore = await connectedStore();
  route(
    "settings/workflows",
    ok({ ok: true, workflows: [{ name: "deploy" }], invalid: [] }),
  );
  await useAppStore.getState().loadWorkflows("project");
  expect(useAppStore.getState().configWorkflows?.workflows).toEqual([
    { name: "deploy" },
  ]);
  expect(useAppStore.getState().configWorkflowScope).toBe("project");
});

test("loadWorkflowHub consumes the hub's pre-aggregated overview as-is", async () => {
  const useAppStore = await connectedStore();
  route(
    "/api/workflow-overview",
    ok({
      ok: true,
      groups: [
        {
          instanceId: "i1",
          scope: "global",
          workspace: "",
          workflows: [{ name: "nightly" }],
          invalid: [],
          lastRuns: {},
          error: null,
        },
        {
          instanceId: "i2",
          scope: "project",
          workspace: "/proj/a",
          workflows: [{ name: "deploy" }],
          invalid: [],
          lastRuns: { deploy: { runId: "r1", name: "deploy", status: "running" } },
          error: null,
        },
      ],
      activeRuns: [
        {
          runId: "r1",
          name: "deploy",
          status: "running",
          acpSessionId: "sess",
          ownerInstanceId: "i2",
        },
      ],
    }),
  );
  await useAppStore.getState().loadWorkflowHub();
  const s = useAppStore.getState();
  // One group per WORKSPACE, exactly as served — no per-instance fan-out and
  // no client-side merge to get wrong.
  expect(s.workflowHub?.map((g) => g.workspace)).toEqual(["", "/proj/a"]);
  expect(s.workflowActiveRuns?.[0]).toMatchObject({
    runId: "r1",
    ownerInstanceId: "i2",
  });
  // The ONLY request is the hub-level route — per-instance reads are gone.
  expect(requested.filter((r) => r.url.includes("workflow"))).toEqual([
    expect.objectContaining({ url: expect.stringContaining("/api/workflow-overview") }),
  ]);
});

test("a 404 overview is the actionable hub-too-old story, cleared by a later success", async () => {
  const useAppStore = await connectedStore();
  route(
    "/api/workflow-overview",
    refused(404, { ok: false, error: "not found" }),
  );
  await useAppStore.getState().loadWorkflowHub();
  let s = useAppStore.getState();
  // First load failed: an empty list that has "loaded" plus the actionable
  // error — the page shows the restart-the-hub story, not a dead spinner.
  expect(s.workflowHub).toEqual([]);
  expect(s.workflowHubError).toContain("restart the hub");
  expect(s.workflowHubLoading).toBe(false);
  expect(s.toast).toBeNull();

  // The hub comes back (restarted): the next successful poll clears the
  // error and lands real groups.
  route(
    "/api/workflow-overview",
    ok({
      ok: true,
      groups: [
        {
          instanceId: "i1",
          scope: "global",
          workspace: "",
          workflows: [],
          invalid: [],
          lastRuns: {},
          error: null,
        },
      ],
      activeRuns: [],
    }),
  );
  await useAppStore.getState().loadWorkflowHub();
  s = useAppStore.getState();
  expect(s.workflowHubError).toBeNull();
  expect(s.workflowHub).toHaveLength(1);
});

test("a transient overview failure keeps the last groups without an error paint", async () => {
  const useAppStore = await connectedStore();
  route(
    "/api/workflow-overview",
    ok({
      ok: true,
      groups: [
        {
          instanceId: "i1",
          scope: "global",
          workspace: "",
          workflows: [{ name: "nightly" }],
          invalid: [],
          lastRuns: {},
          error: null,
        },
      ],
      activeRuns: [],
    }),
  );
  await useAppStore.getState().loadWorkflowHub();

  route(
    "/api/workflow-overview",
    refused(500, { ok: false, error: "boom" }),
  );
  await useAppStore.getState().loadWorkflowHub();
  const s = useAppStore.getState();
  expect(s.workflowHub).toHaveLength(1); // the working list stays on screen
  expect(s.workflowHubError).toBeNull();
  expect(s.workflowHubLoading).toBe(false);
});

test("a scope answer that lands after a newer ask is dropped, not painted", async () => {
  const useAppStore = await connectedStore();
  // Two taps, two in-flight reads: the slower (project) answer must not
  // repaint the list the user moved off of — same guard as the usage ranges.
  route(
    "settings/workflows?scope=project",
    delayed(ok({ ok: true, workflows: [{ name: "old" }], invalid: [] }), 20),
  );
  route(
    "settings/workflows?scope=global",
    ok({ ok: true, workflows: [{ name: "new" }], invalid: [] }),
  );
  const first = useAppStore.getState().loadWorkflows("project");
  await useAppStore.getState().loadWorkflows("global");
  await first;
  expect(useAppStore.getState().configWorkflows?.workflows).toEqual([
    { name: "new" },
  ]);
});

test("the gate verdict comes from the per-instance snapshot, not the machine-level one", async () => {
  const useAppStore = await connectedStore();
  // The machine-level /settings/all (what the entry list loads) reports no
  // `enabled` field at all — its mount has no backend. The probe must name
  // the instance, or the workflows entry can never appear on any hub.
  route(
    "/settings/all",
    ok({
      ok: true,
      workflow: { enabled: true, mode: "alwaysOn", source: "remote" },
    }),
  );
  await useAppStore.getState().loadWorkflowGate();
  expect(useAppStore.getState().configWorkflowGate).toEqual({
    enabled: true,
    mode: "alwaysOn",
    source: "remote",
  });
  const asked = requested.find((r) => r.url.includes("/settings/all"));
  expect(asked?.url).toContain("/api/instances/1/settings/all");
});

test("a refused gate probe leaves the entry hidden, not errored", async () => {
  const useAppStore = await connectedStore();
  route("/settings/all", refused(500, { ok: false, error: "boom" }));
  await useAppStore.getState().loadWorkflowGate();
  expect(useAppStore.getState().configWorkflowGate).toBeNull();
  expect(useAppStore.getState().configSupported).not.toBe(false);
  expect(useAppStore.getState().toast).toBeNull();
  // The 404 flavor — a hub re-learning its instances after a restart — must
  // not be read as "hub too old" either: that verdict would retire EVERY
  // config section on one transient answer, and reconnect does not revive it.
  route(
    "/settings/all",
    refused(404, { ok: false, error: "unknown instance" }),
  );
  await useAppStore.getState().loadWorkflowGate();
  expect(useAppStore.getState().configWorkflowGate).toBeNull();
  expect(useAppStore.getState().configSupported).toBeNull();
  expect(useAppStore.getState().toast).toBeNull();
});

test("setWorkflowGate PUTs the mode and stores the returned verdict", async () => {
  const useAppStore = await connectedStore();
  route(
    "/settings/all",
    ok({
      ok: true,
      workflow: { enabled: false, mode: "disabled", source: "default" },
    }),
  );
  await useAppStore.getState().loadWorkflowGate();
  expect(useAppStore.getState().configWorkflowGate?.enabled).toBe(false);

  route(
    "workflow-gate",
    ok({
      ok: true,
      gate: {
        enabled: true,
        mode: "alwaysOn",
        source: "override",
        override: "alwaysOn",
      },
    }),
  );
  const gate = await useAppStore.getState().setWorkflowGate("alwaysOn");
  // The store rides the PUT's answer, so every gate-driven surface (config
  // entry, session-panel launcher) flips without a follow-up probe.
  expect(gate).toMatchObject({ enabled: true, source: "override" });
  expect(useAppStore.getState().configWorkflowGate).toMatchObject({
    override: "alwaysOn",
    enabled: true,
  });
  expect(
    requested.find((r) => r.url.includes("workflow-gate")),
  ).toMatchObject({
    method: "PUT",
    body: { mode: "alwaysOn" },
  });
});

test("setWorkflowGate without an instance reports and resolves null", async () => {
  const useAppStore = await store();
  useAppStore.getState().connectToHub({ hubUrl: "http://hub/", token: "tok" });
  const gate = await useAppStore.getState().setWorkflowGate("auto");
  expect(gate).toBeNull();
  expect(useAppStore.getState().toast?.text).toContain("not connected");
  expect(requested).toHaveLength(0);
});

test("a 404 on the workflows list is a retriable error, not 'unsupported'", async () => {
  const useAppStore = await connectedStore();
  route(
    "/settings/workflows",
    refused(404, { ok: false, error: "unknown instance" }),
  );
  await useAppStore.getState().loadWorkflows("project");
  const s = useAppStore.getState();
  expect(s.configSupported).toBeNull();
  expect(s.configError).toContain("unknown instance");
  expect(s.configLoading).toBe(false);
  expect(s.configWorkflows).toBeNull();
});

test("a start records the launch memory — the only run→session join there is", async () => {
  const useAppStore = await connectedStore();
  route(
    "/start",
    ok({ ok: true, acpSessionId: "acp-9", runId: "run-9", toolCallId: "tc" }),
  );
  const res = await useAppStore
    .getState()
    .startWorkflow({ scope: "project", name: "deploy", args: { env: "prod" } });
  expect(res?.acpSessionId).toBe("acp-9");
  const { lookupLaunch } = await import("../src/lib/workflow-launches");
  expect(lookupLaunch("run-9")).toMatchObject({
    acpSessionId: "acp-9",
    instanceId: "1",
    name: "deploy",
    scope: "project",
  });
  // The request carries the args — the only knob the launch has. (Found by
  // URL, not index: background polling shares the request log.)
  expect(requested.find((r) => r.url.includes("/start"))).toMatchObject({
    method: "POST",
    body: { args: { env: "prod" } },
  });
});

test("an in-chat start pins the run to the caller's session", async () => {
  const useAppStore = await connectedStore();
  route("/start", ok({ ok: true, acpSessionId: "cur-1", runId: "run-10" }));
  const res = await useAppStore.getState().startWorkflow({
    scope: "global",
    name: "deploy",
    sessionId: "cur-1",
  });
  expect(res?.acpSessionId).toBe("cur-1");
  expect(requested.find((r) => r.url.includes("/start"))).toMatchObject({
    method: "POST",
    body: { sessionId: "cur-1" },
  });
});

test("a refused start toasts the detail and leaves no launch memory", async () => {
  const useAppStore = await connectedStore();
  route(
    "/start",
    refused(422, {
      ok: false,
      error: "compile_failed",
      message: "line 3: unexpected token",
    }),
  );
  const res = await useAppStore
    .getState()
    .startWorkflow({ scope: "project", name: "broken" });
  expect(res).toBeNull();
  expect(useAppStore.getState().toast?.text).toContain(
    "line 3: unexpected token",
  );
  // No runId ever came back, so the honest assertion is on the storage the
  // refused start must NOT have written (launch memory is post-success only).
  expect(backing.has("zcode.workflowLaunches")).toBe(false);
});

test("composer prefill is nonce-keyed so the same text re-triggers", async () => {
  const useAppStore = await store();
  useAppStore.getState().setComposerPrefill("Help me design…");
  const first = useAppStore.getState().composerPrefill;
  useAppStore.getState().setComposerPrefill("Help me design…");
  const second = useAppStore.getState().composerPrefill;
  expect(first?.text).toBe("Help me design…");
  expect(second?.text).toBe("Help me design…");
  expect(second?.nonce).not.toBe(first?.nonce);
  useAppStore.getState().clearComposerPrefill();
  expect(useAppStore.getState().composerPrefill).toBeNull();
});

test("loadRunSummaries degrades silently for sessions the bridge forgot", async () => {
  const useAppStore = await connectedStore();
  route(
    "settings/workflow-runs?sessionId=dead",
    refused(404, { ok: false, error: "unknown_session" }),
  );
  route(
    "settings/workflow-runs?sessionId=live",
    ok({ ok: true, runs: [{ runId: "r", resumable: true }] }),
  );
  const out = await useAppStore.getState().loadRunSummaries(["dead", "live"]);
  expect(out["dead"]).toBeUndefined();
  expect(out["live"]?.[0]).toMatchObject({ runId: "r", resumable: true });
  // Silent by design: no toast for the forgotten session.
  expect(useAppStore.getState().toast).toBeNull();
});

test("workflowAction without an instance reports and resolves null", async () => {
  const useAppStore = await store();
  useAppStore.getState().connectToHub({ hubUrl: "http://hub/", token: "tok" });
  expect(useAppStore.getState().instanceId).toBeNull();
  const res = await useAppStore
    .getState()
    .workflowAction("start workflow", () => Promise.resolve({ ok: true }));
  expect(res).toBeNull();
  expect(useAppStore.getState().toast?.text).toContain("not connected");
  expect(requested).toHaveLength(0);
});

test("workflowAction hands silent callers the bare failure message via onError", async () => {
  const useAppStore = await store();
  useAppStore.getState().connectToHub({ hubUrl: "http://hub/", token: "tok" });
  const reasons: string[] = [];

  // Not connected: "not connected" reaches the caller, silent means no toast.
  await useAppStore.getState().workflowAction(
    "load workflows",
    () => Promise.resolve({ ok: true }),
    true,
    (m) => reasons.push(m),
  );
  expect(reasons).toEqual(["not connected"]);
  expect(useAppStore.getState().toast).toBeNull();

  // A refused call: the wire reason token (here the gate) reaches the caller.
  useAppStore.setState({ instanceId: "1" });
  route(
    "settings/workflows",
    refused(403, { ok: false, error: "workflow_disabled" }),
  );
  await useAppStore.getState().workflowAction(
    "load workflows",
    (c, iid) => c.workflowsList(iid, "project"),
    true,
    (m) => reasons.push(m),
  );
  expect(reasons).toEqual(["not connected", "workflow_disabled"]);
  expect(useAppStore.getState().toast).toBeNull();
});

test("an amendment returns the continuation run — the caller re-points on a new id", async () => {
  const useAppStore = await connectedStore();
  route(
    "/settings",
    ok({
      ok: true,
      runId: "run-new",
      toolCallId: "settings-1",
      supersededRunId: "run-old",
    }),
  );
  const res = await useAppStore.getState().amendWorkflowRun({
    runId: "run-old",
    sessionId: "acp-1",
    body: { subagentModel: null },
  });
  expect(res).toMatchObject({ runId: "run-new", supersededRunId: "run-old" });
  // The route addresses the session by QUERY (bridge 0.49.0 contract), and a
  // `null` rides the body as a real value.
  expect(requested.find((r) => r.url.includes("/settings?"))).toMatchObject({
    method: "POST",
    body: { subagentModel: null },
  });
});

test("stopWorkflowRun without a live ACP connection reports and resolves false", async () => {
  const useAppStore = await connectedStore();
  // connectedStore wires the hub HTTP client only — no WS, so acp is null.
  const ok = await useAppStore.getState().stopWorkflowRun("run-1", "acp-1");
  expect(ok).toBe(false);
  expect(useAppStore.getState().toast?.text).toContain("not connected");
});

test("config deep entry: section back closes the screen, root navigation keeps the list", async () => {
  const useAppStore = await store();
  // Deep entry (session panel): back from the section returns to the caller,
  // never to an entry list the user did not navigate through.
  useAppStore.getState().openConfig("workflows");
  expect(useAppStore.getState().configDeepEntry).toBe(true);
  useAppStore.getState().backFromConfigSection();
  expect(useAppStore.getState().configOpen).toBe(false);
  expect(useAppStore.getState().configSection).toBeNull();

  // Root → section navigation: back returns to the entry list.
  useAppStore.getState().openConfig(null);
  useAppStore.getState().openConfig("workflows");
  expect(useAppStore.getState().configDeepEntry).toBe(false);
  useAppStore.getState().backFromConfigSection();
  expect(useAppStore.getState().configOpen).toBe(true);
  expect(useAppStore.getState().configSection).toBeNull();

  useAppStore.getState().closeConfig();
});

test("openWorkflowRun stages the run deep link and deep-opens the workflows page", async () => {
  const useAppStore = await store();
  useAppStore.getState().openWorkflowRun({
    sessionId: "s1",
    runId: "r1",
    name: "deploy",
    instanceId: "i1",
  });
  expect(useAppStore.getState().configOpen).toBe(true);
  expect(useAppStore.getState().configSection).toBe("workflows");
  expect(useAppStore.getState().configDeepEntry).toBe(true);
  expect(useAppStore.getState().workflowRunTarget).toMatchObject({ runId: "r1" });
  // Consumed exactly once by the page's mount.
  useAppStore.getState().clearWorkflowRunTarget();
  expect(useAppStore.getState().workflowRunTarget).toBeNull();
  useAppStore.getState().closeConfig();
});
