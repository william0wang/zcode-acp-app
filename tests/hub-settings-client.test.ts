// @vitest-environment node
// REST-layer coverage for the ZCode configuration client (ADR-0009): the
// hub-level settings routes every screen reads, the write routes and their
// method/URL spelling, and the two contracts that are easy to get wrong —
// the reset-card nonce travelling with a spend, and the backend restart
// naming an instance because the hub's own mount has no backend to restart.
import { beforeEach, expect, test } from "vitest";
import { HubClient } from "../src/lib/hub";

let responses: Response[] = [];
let requested: Array<{ url: string; method: string; body?: unknown }> = [];

function install(): void {
  (globalThis as Record<string, unknown>).fetch = async (
    input: unknown,
    init?: { method?: string; body?: string },
  ): Promise<Response> => {
    requested.push({
      url: String(input),
      method: init?.method ?? "GET",
      ...(init?.body ? { body: JSON.parse(init.body) as unknown } : {}),
    });
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

test("settingsAll reads the machine-level mount, not an instance", async () => {
  responses.push(Response.json({ ok: true, models: {}, skills: [] }));
  await client().settingsAll();
  expect(requested[0]!.url).toBe("http://hub/api/settings/all");
});

test("workflowOverview is a hub-level read (the hub aggregates the instances)", async () => {
  responses.push(Response.json({ ok: true, groups: [], activeRuns: [] }));
  const res = await client().workflowOverview();
  expect(res.groups).toEqual([]);
  // NOT instance-addressed — the whole point of the route is that the hub
  // dedupes bridges and merges the machine-wide journal server-side.
  expect(requested[0]!.url).toBe("http://hub/api/workflow-overview");
});

test("section reads hit their own routes with the usage range encoded", async () => {
  responses.push(Response.json({ ok: true }));
  responses.push(Response.json({ ok: true }));
  responses.push(Response.json({ ok: true }));
  await client().settingsModels();
  await client().settingsSkills();
  await client().settingsUsage("30d");
  expect(requested.map((r) => r.url)).toEqual([
    "http://hub/api/settings/models",
    "http://hub/api/settings/skills",
    "http://hub/api/settings/usage?range=30d",
  ]);
});

test("writes use the verb the route expects (PUT for upsert, DELETE for remove)", async () => {
  responses.push(Response.json({ ok: true, effect: "needs-restart" }));
  responses.push(Response.json({ ok: true, effect: "needs-restart" }));
  responses.push(Response.json({ ok: true, effect: "immediate" }));

  await client().upsertMcpServer("ctx7", { command: "npx" });
  await client().removeMcpServer("ctx7");
  await client().setSkillEnabled("/s/SKILL.md", false);

  expect(requested[0]).toMatchObject({
    url: "http://hub/api/settings/mcp/ctx7",
    method: "PUT",
  });
  expect(requested[1]).toMatchObject({
    url: "http://hub/api/settings/mcp/ctx7",
    method: "DELETE",
  });
  expect(requested[2]).toMatchObject({
    url: "http://hub/api/settings/skills/enable",
    method: "POST",
  });
});

test("a skill path is encoded so slashes survive the route", async () => {
  responses.push(Response.json({ ok: true }));
  await client().deleteSkill("/home/u/.zcode/skills/x/SKILL.md");
  expect(requested[0]!.url).toBe(
    "http://hub/api/settings/skills/%2Fhome%2Fu%2F.zcode%2Fskills%2Fx%2FSKILL.md",
  );
});

test("the skill detail read reuses the delete route's path encoding", async () => {
  responses.push(Response.json({ ok: true, skill: { name: "x" } }));
  await client().skillDetail("/home/u/.zcode/skills/x/SKILL.md");
  // Machine-level like the rest of the skills tree, whole absolute path in
  // one segment — the bridge decodes one segment, so unencoded slashes would
  // 404 as a multi-segment route.
  expect(requested[0]).toMatchObject({
    url: "http://hub/api/settings/skills/%2Fhome%2Fu%2F.zcode%2Fskills%2Fx%2FSKILL.md",
    method: "GET",
  });
});

test("a model upsert POSTs provider and model with only the set rule fields", async () => {
  responses.push(Response.json({ ok: true, effect: "immediate" }));
  await client().addModel({
    providerId: "account:bigmodel-individual-coding-plan",
    modelId: "glm-4.6",
    enabled: true,
    contextWindow: 200000,
    reasoningLevels: ["high", "low"],
  });
  // One route serves add AND edit (an upsert): the body names the model and
  // the rule fields that changed, nothing else.
  expect(requested[0]).toMatchObject({
    url: "http://hub/api/settings/models",
    method: "POST",
    body: {
      providerId: "account:bigmodel-individual-coding-plan",
      modelId: "glm-4.6",
      enabled: true,
      contextWindow: 200000,
      reasoningLevels: ["high", "low"],
    },
  });
});

test("an agent upsert PUTs nulls verbatim — clearing rides on them", async () => {
  responses.push(Response.json({ ok: true, effect: "needs-restart" }));
  responses.push(Response.json({ ok: true, effect: "needs-restart" }));
  // A personal-agent patch: `model: null` means "remove the key", so the body
  // must carry the null, not drop it.
  await client().upsertAgent("mine", { description: "d", model: null });
  // A built-in override clear: BOTH keys null together, or the server calls
  // the request malformed.
  await client().upsertAgent("general-purpose", {
    providerId: null,
    modelId: null,
  });
  expect(requested[0]).toMatchObject({
    url: "http://hub/api/settings/agents/mine",
    method: "PUT",
    body: { description: "d", model: null },
  });
  expect(requested[1]).toMatchObject({
    url: "http://hub/api/settings/agents/general-purpose",
    method: "PUT",
    body: { providerId: null, modelId: null },
  });
});

test("a hook edit carries all three coordinates — two in the path, one in the body", async () => {
  responses.push(Response.json({ ok: true, effect: "needs-restart" }));
  await client().updateHook("PostToolUse", 2, 1, { command: "echo hi" });
  // The route only takes two path segments, so the hook's index inside its
  // matcher group travels in the body. Sending `{event}/{index}` alone is a
  // 400 — and it is exactly the shape a reader would guess.
  expect(requested[0]).toMatchObject({
    url: "http://hub/api/settings/hooks/PostToolUse/2",
    method: "PUT",
    body: { command: "echo hi", hookIndex: 1 },
  });
});

test("a hook add addresses the group by its matcher string", async () => {
  responses.push(Response.json({ ok: true, effect: "needs-restart" }));
  await client().addHook("PreToolUse", {
    matcher: "Bash",
    hook: { type: "command", command: "echo hi", timeout: 30 },
  });
  // The matcher STRING locates the group — an index would race a concurrent
  // edit — and the entry itself rides under `hook`.
  expect(requested[0]).toMatchObject({
    url: "http://hub/api/settings/hooks/PreToolUse",
    method: "POST",
    body: {
      matcher: "Bash",
      hook: { type: "command", command: "echo hi", timeout: 30 },
    },
  });
});

test("a hook remove carries the command as the fingerprint", async () => {
  responses.push(Response.json({ ok: true, effect: "needs-restart" }));
  await client().removeHook("Stop", {
    matcherIndex: 0,
    hookIndex: 1,
    command: "say done",
  });
  // The indexes address the entry, but `command` is the guard: a stale read
  // whose entry changed is refused server-side instead of deleting the wrong
  // hook.
  expect(requested[0]).toMatchObject({
    url: "http://hub/api/settings/hooks/Stop/remove",
    method: "POST",
    body: { matcherIndex: 0, hookIndex: 1, command: "say done" },
  });
});

test("a provider create posts the name only — the id comes back", async () => {
  responses.push(
    Response.json({ ok: true, providerId: "my-api", effect: "immediate" }),
  );
  const created = await client().createProvider({
    providerName: "My API",
    apiType: "openai-chat-completions",
    baseUrl: "https://api.example.com/v1",
  });
  // The id is DERIVED server-side from the name; the response is where it
  // first appears, never the request.
  expect(requested[0]).toMatchObject({
    url: "http://hub/api/settings/providers",
    method: "POST",
    body: {
      providerName: "My API",
      apiType: "openai-chat-completions",
      baseUrl: "https://api.example.com/v1",
    },
  });
  expect(created.providerId).toBe("my-api");
});

test("a provider remove deletes by encoded id, a reorder posts the sequence", async () => {
  responses.push(Response.json({ ok: true, effect: "immediate" }));
  await client().removeProvider("my api");
  expect(requested[0]).toMatchObject({
    url: "http://hub/api/settings/providers/my%20api",
    method: "DELETE",
  });

  responses.push(Response.json({ ok: true, effect: "immediate" }));
  await client().reorderProviders(["beta", "alpha"]);
  expect(requested[1]).toMatchObject({
    url: "http://hub/api/settings/providers/reorder",
    method: "POST",
    body: { providerIds: ["beta", "alpha"] },
  });
});

test("the MCP enable route wants `enabled`, not `enable`", async () => {
  responses.push(Response.json({ ok: true, effect: "needs-restart" }));
  await client().setMcpEnabled("ctx7", false);
  expect(requested[0]).toMatchObject({
    url: "http://hub/api/settings/mcp/enable",
    method: "POST",
    body: { name: "ctx7", enabled: false },
  });
});

test("the MCP status read rides the per-instance mount", async () => {
  responses.push(
    Response.json({
      ok: true,
      available: true,
      statuses: { ctx7: { status: "connected", toolCount: 12 } },
    }),
  );
  await client().mcpStatus("i 1");
  // Per-instance on purpose — the machine-level mount has no backend to ask
  // and answers 409 for this route, and the id rides the path encoded.
  expect(requested[0]).toMatchObject({
    url: "http://hub/api/instances/i%201/settings/mcp-status",
    method: "GET",
  });
});

test("an unavailable MCP status is a body, not an error", async () => {
  responses.push(Response.json({ ok: true, available: false, statuses: {} }));
  const body = (await client().mcpStatus("i1")) as {
    available: boolean;
    statuses: Record<string, unknown>;
  };
  // `available:false` (older bridge, timed-out RPC) must resolve Ok — the
  // page renders "unknown", it does not fail.
  expect(body.available).toBe(false);
  expect(body.statuses).toEqual({});
});

test("a reset-card spend carries the nonce from the status read", async () => {
  responses.push(
    Response.json({
      ok: true,
      resetCards: {
        availableFiveHour: [{ expireAt: 1700000000000 }],
        availableWeek: [],
        latestFiveHour: null,
        latestWeek: null,
        hasUnreadHistory: false,
        nonce: "n-1",
      },
    }),
  );
  const status = await client().resetCardStatus(
    "account:bigmodel-individual-coding-plan",
  );
  expect(requested[0]!.url).toBe(
    "http://hub/api/settings/reset-cards?providerId=account%3Abigmodel-individual-coding-plan",
  );

  responses.push(Response.json({ ok: true, used: true }));
  await client().spendResetCard({
    providerId: "account:bigmodel-individual-coding-plan",
    resetType: "FIVE_HOUR",
    nonce: status.resetCards.nonce,
    idempotencyKey: "k-1",
  });
  expect(requested[1]).toMatchObject({
    url: "http://hub/api/settings/reset-cards/use",
    method: "POST",
    body: {
      nonce: "n-1",
      idempotencyKey: "k-1",
      resetType: "FIVE_HOUR",
    },
  });
});

test("the backend restart names an instance — the hub mount has no backend", async () => {
  responses.push(Response.json({ cancelledTurns: 2 }));
  await expect(client().restartBackend("inst-1")).resolves.toEqual({
    cancelledTurns: 2,
  });
  expect(requested[0]).toMatchObject({
    url: "http://hub/api/instances/inst-1/backend/restart",
    method: "POST",
  });
});

test("a backup restore sends the file and the backup path", async () => {
  responses.push(Response.json({ ok: true, effect: "needs-restart" }));
  await client().restoreBackup({
    file: "cliConfig",
    path: "/h/.zcode/cli/config.json.bak-20260922",
  });
  expect(requested[0]).toMatchObject({
    url: "http://hub/api/settings/backups/restore",
    method: "POST",
    body: {
      file: "cliConfig",
      path: "/h/.zcode/cli/config.json.bak-20260922",
    },
  });
});

test("an app-update install posts the version and artifact url", async () => {
  responses.push(
    Response.json({ ok: true, install: { stage: "downloading" } }),
  );
  await client().installAppUpdate({
    version: "3.14.2",
    url: "https://cdn/zcode.zip",
    channel: "stable",
  });
  expect(requested[0]).toMatchObject({
    url: "http://hub/api/settings/app-update/install",
    method: "POST",
    body: {
      version: "3.14.2",
      url: "https://cdn/zcode.zip",
      channel: "stable",
    },
  });
});

test("the install carries the channel the check was made with", async () => {
  // The install route re-reads the manifest for the channel it is TOLD and
  // refuses any version that is not the latest THERE (409 otherwise). A
  // preview user whose install defaulted to stable is refused a version the
  // check itself just offered — so the store fills the channel in.
  responses.push(Response.json({ ok: true }));
  await client().installAppUpdate({
    version: "3.14.3-preview.1",
    url: "https://cdn/zcode.zip",
    channel: "preview",
  });
  expect(requested[0]).toMatchObject({
    body: { version: "3.14.3-preview.1", channel: "preview" },
  });
});

test("the check reads the requested channel, not just stable", async () => {
  responses.push(Response.json({ ok: true }));
  await client().settingsAppUpdate("preview");
  expect(requested[0]!.url).toBe(
    "http://hub/api/settings/app-update?channel=preview",
  );
});

// --- dynamic-workflow routes (bridge 0.48.0, server ADR-0029) -------------
//
// Every workflow route is per-instance and must spell the per-instance
// prefix — the hub's machine-level mount answers 409 — and the run-detail
// queries must carry the ACP session id plus the journal cursor, because
// that pair is what the bridge resolves the query with.

test("workflow management routes name the instance and use their verbs", async () => {
  responses.push(Response.json({ ok: true }));
  responses.push(Response.json({ ok: true }));
  responses.push(Response.json({ ok: true }));
  responses.push(Response.json({ ok: true }));
  responses.push(Response.json({ ok: true }));
  responses.push(Response.json({ ok: true }));

  await client().workflowsList("i1", "project");
  await client().workflowGet("i1", "global", "night ly");
  await client().workflowUpdateMeta("i1", "global", "night ly", {
    description: "x",
  });
  await client().workflowDelete("i1", "global", "night ly");
  await client().workflowMove("i1", "global", "night ly");
  await client().workflowStart("i1", "project", "deploy", {
    args: { target: "src/" },
  });

  expect(requested[0]).toMatchObject({
    url: "http://hub/api/instances/i1/settings/workflows?scope=project",
    method: "GET",
  });
  expect(requested[1]).toMatchObject({
    url: "http://hub/api/instances/i1/settings/workflows/global/night%20ly",
    method: "GET",
  });
  expect(requested[2]).toMatchObject({
    url: "http://hub/api/instances/i1/settings/workflows/global/night%20ly/meta",
    method: "PUT",
    body: { description: "x" },
  });
  expect(requested[3]).toMatchObject({
    url: "http://hub/api/instances/i1/settings/workflows/global/night%20ly",
    method: "DELETE",
  });
  expect(requested[4]).toMatchObject({
    url: "http://hub/api/instances/i1/settings/workflows/global/night%20ly/move",
    method: "POST",
  });
  expect(requested[5]).toMatchObject({
    url: "http://hub/api/instances/i1/settings/workflows/project/deploy/start",
    method: "POST",
    body: { args: { target: "src/" } },
  });
});

test("run queries carry the session id and the journal cursor", async () => {
  responses.push(Response.json({ ok: true }));
  responses.push(Response.json({ ok: true }));
  responses.push(Response.json({ ok: true }));
  responses.push(Response.json({ ok: true }));
  responses.push(Response.json({ ok: true }));

  await client().conversationRuns("i1", "sess 1");
  await client().runEvents("i1", "sess 1", "run 2", 41);
  await client().runArtifactData("i1", "sess 1", "run 2", "a/b", 7, 50);
  await client().runArtifactRead("i1", "sess 1", "run 2", "a/b", 3, 0, 1024);
  await client().runNodeResult("i1", "sess 1", "run 2", "site 1", 2);

  expect(requested[0]!.url).toBe(
    "http://hub/api/instances/i1/settings/workflow-runs?sessionId=sess+1",
  );
  expect(requested[1]!.url).toBe(
    "http://hub/api/instances/i1/settings/workflow-runs/run%202/events?sessionId=sess+1&afterSequence=41",
  );
  expect(requested[2]!.url).toBe(
    "http://hub/api/instances/i1/settings/workflow-runs/run%202/artifacts/a%2Fb/data?sessionId=sess+1&afterSequence=7&limit=50",
  );
  expect(requested[3]!.url).toBe(
    "http://hub/api/instances/i1/settings/workflow-runs/run%202/artifacts/a%2Fb/read?sessionId=sess+1&version=3&offset=0&limit=1024",
  );
  expect(requested[4]!.url).toBe(
    "http://hub/api/instances/i1/settings/workflow-runs/run%202/nodes/site%201/2?sessionId=sess+1",
  );
});

test("resume and the create prompt keep their body/shape contracts", async () => {
  responses.push(Response.json({ ok: true }));
  responses.push(Response.json({ ok: true, prompt: "Help me…" }));

  await client().workflowResume("i1", "run 2", {
    sessionId: "sess 1",
    name: "deploy",
  });
  await client().workflowCreatePrompt("i1", "global");

  expect(requested[0]).toMatchObject({
    url: "http://hub/api/instances/i1/settings/workflow-runs/run%202/resume",
    method: "POST",
    body: { sessionId: "sess 1", name: "deploy" },
  });
  expect(requested[1]!.url).toBe(
    "http://hub/api/instances/i1/settings/workflow-create-prompt?scope=global",
  );
});

test("amendRunSettings addresses the run by path, the session by query (bridge 0.49.0)", async () => {
  responses.push(
    Response.json({
      ok: true,
      runId: "run-new",
      toolCallId: "settings-1",
      supersededRunId: "run-old",
    }),
  );

  const res = await client().amendRunSettings("i1", "run 2", "sess 1", {
    subagentModel: null,
    maxConcurrency: 4,
  });

  expect(requested[0]).toMatchObject({
    url: "http://hub/api/instances/i1/settings/workflow-runs/run%202/settings?sessionId=sess+1",
    method: "POST",
    // Key PRESENCE is the three-state discriminator — `null` must ride the
    // wire as a real JSON null, never be dropped or stringified.
    body: { subagentModel: null, maxConcurrency: 4 },
  });
  expect(res).toMatchObject({
    runId: "run-new",
    supersededRunId: "run-old",
  });
});

test("a workflow refusal surfaces the message detail over the reason token", async () => {
  // The workflow routes answer {ok:false, error:<token>, message:<detail>} —
  // for compile_failed the diagnostics ride in `message`, and "compile_failed"
  // alone would leave the user staring at a token.
  responses.push(
    new Response(
      JSON.stringify({
        ok: false,
        error: "compile_failed",
        message: "line 3: unexpected token",
      }),
      { status: 422 },
    ),
  );
  await expect(
    client().workflowStart("i1", "project", "broken", {}),
  ).rejects.toThrow("line 3: unexpected token");
});

test("the workflow gate probe reads the per-instance settings snapshot", async () => {
  // The machine-level /settings/all carries no gate verdict (its mount has
  // no backend), so the entry visibility must come from this spelling.
  responses.push(
    Response.json({
      ok: true,
      workflow: { enabled: true, mode: "alwaysOn", source: "remote" },
    }),
  );
  await client().instanceSettingsAll("i1");
  expect(requested[0]!.url).toBe("http://hub/api/instances/i1/settings/all");
});
