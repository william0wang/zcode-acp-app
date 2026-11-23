// @vitest-environment node
// Response-shape coverage for the configuration screens (ADR-0009).
//
// The REST tests in hub-settings-client.test.ts lock the REQUESTS; these lock
// the DECODING. The distinction matters: an earlier round of these screens was
// written against an imagined payload (`models` as an array, `usage` unwrapped,
// `hooks.events` at the top level) and every request test still passed while
// four of the nine screens were dead on a real hub. Each case below feeds the
// shape the bridge actually emits and asserts the value the screen ends up
// with.
//
// Every decode function is IMPORTED from the screen that uses it, never copied
// here. A copy passing while the page reads a different path is exactly how the
// first round shipped dead — so a copy is now impossible to write by accident.
import { describe, expect, it } from "vitest";
import { isTeamPlan, windowUsedPercent } from "../src/screens/config/QuotaPage";
import {
  contextWindowFor,
  decodeSubagentModel,
  encodeSubagentModel,
  findModelOption,
  matchProviderId,
  modelOptions,
  orderedProviders,
  providerOptions,
} from "../src/screens/config/ModelsPage";
import { mcpServers, mcpStatuses, mcpUpsertBody } from "../src/screens/config/McpPage";
import {
  flattenHooks,
  hooksEnabled,
  timeoutMsOf,
} from "../src/screens/config/HooksPage";
import {
  decodeSkillDetail,
  isUserTreeScope,
  type SkillScope,
} from "../src/screens/config/SkillsPage";
import { isAgentReadOnly } from "../src/screens/config/AgentsPage";

// --- mcp: wrapped in `{ok, mcp:{servers}}`, like models and usage -----------
//
// Added after the same class of bug recurred a THIRD time: the screen read
// `servers` off the top level, so a machine with configured servers showed its
// empty state. `readMcpView` (settings-endpoint.ts) is the authority here.

describe("mcp payload", () => {
  const wire = {
    ok: true,
    mcp: {
      servers: [
        {
          name: "ctx7",
          type: "stdio",
          command: "npx",
          args: ["-y", "@upstash/context7-mcp"],
          enabled: true,
        },
        {
          name: "off",
          type: "http",
          url: "https://example.com/mcp",
          enabled: false,
        },
      ],
    },
  };

  it("reads servers from inside `mcp`, not the top level", () => {
    // The bug this guards: `payload.servers` is undefined, `.length === 0`,
    // and the page renders "no MCP servers configured" on a machine that has
    // them — indistinguishable from a genuinely empty list.
    expect((wire as { servers?: unknown }).servers).toBeUndefined();
    expect(mcpServers(wire)).toHaveLength(2);
  });

  it("keeps the command plus args for the row's subtitle", () => {
    const first = mcpServers(wire)[0]!;
    expect([first.command, ...(first.args ?? [])].join(" ")).toBe(
      "npx -y @upstash/context7-mcp",
    );
  });

  it("treats an absent `enabled` as on, matching the config-file default", () => {
    const rows = mcpServers(wire);
    expect(rows[0]!.enabled !== false).toBe(true);
    expect(rows[1]!.enabled !== false).toBe(false);
  });

  it("returns an empty list rather than throwing on a missing section", () => {
    expect(mcpServers({ ok: true })).toEqual([]);
    expect(mcpServers(null)).toEqual([]);
  });
});

// --- the per-instance health read behind the rows' status dots ---------------

describe("mcp status decode", () => {
  it("reads the per-server map and keeps the raw verdict vocabulary", () => {
    const { available, statuses } = mcpStatuses({
      ok: true,
      available: true,
      statuses: {
        ctx7: { status: "connected", toolCount: 12 },
        weird: { status: "never-heard-of", toolCount: 0, failureKind: "status_unavailable" },
      },
    });
    expect(available).toBe(true);
    // An unknown status spelling stays verbatim — the page renders it as-is
    // rather than folding it into a default (the api_type rule).
    expect(statuses["ctx7"]!.status).toBe("connected");
    expect(statuses["weird"]!.status).toBe("never-heard-of");
    expect(statuses["weird"]!.failureKind).toBe("status_unavailable");
  });

  it("treats `available:false` as unknown, not an error", () => {
    // An older bridge or a timed-out RPC answers this BODY — the decode must
    // succeed so the page can show its "unknown" hint instead of failing.
    const body = mcpStatuses({ ok: true, available: false, statuses: {} });
    expect(body.available).toBe(false);
    expect(body.statuses).toEqual({});
  });

  it("degrades a missing or malformed body to unknown-and-empty", () => {
    expect(mcpStatuses(null)).toEqual({ available: false, statuses: {} });
    expect(mcpStatuses({ ok: true })).toEqual({ available: false, statuses: {} });
  });
});

// --- the upsert body: only the transport's fields, blank kv omitted ---------

describe("mcp upsert body", () => {
  // The route MERGES the body into the entry, so a field from the wrong
  // transport branch lands next to the real one instead of replacing it. This
  // builder is what the form submits — the JSX branches and the body builder
  // drifted apart once (remote rendered the stdio fields), and every edit the
  // user made in the visible fields was silently dropped.
  const stdioFields = {
    type: "stdio",
    enabled: true,
    remote: false,
    url: "",
    headersText: "",
    command: "npx",
    argsText: "-y\nctx7",
    envText: "A=1\nB=x=y",
  };

  it("sends only the transport's own fields", () => {
    expect(mcpUpsertBody(stdioFields)).toEqual({
      type: "stdio",
      enabled: true,
      command: "npx",
      args: ["-y", "ctx7"],
      env: { A: "1", B: "x=y" },
    });
    expect(
      mcpUpsertBody({
        type: "sse",
        enabled: false,
        remote: true,
        url: "https://x/mcp",
        headersText: "Authorization=Bearer t",
        command: "leftover",
        argsText: "leftover",
        envText: "",
      }),
    ).toEqual({
      type: "sse",
      enabled: false,
      url: "https://x/mcp",
      headers: { Authorization: "Bearer t" },
    });
  });

  it("omits a blank env/headers instead of clearing what the entry had", () => {
    // Desktop parity: its JSON textarea drops a field that does not parse, so
    // blank means "keep". A `{}` would wipe the entry's env — or shadow a
    // legacy `http_headers` block the runtime merges with `??`.
    expect(
      "env" in
        mcpUpsertBody({
          ...stdioFields,
          argsText: "",
          envText: "  \n",
        }),
    ).toBe(false);
    expect(
      "headers" in
        mcpUpsertBody({
          type: "http",
          enabled: true,
          remote: true,
          url: "https://x",
          headersText: "",
          command: "",
          argsText: "",
          envText: "",
        }),
    ).toBe(false);
  });

  it("still sends args as [] — the desktop's spelling for no arguments", () => {
    expect(
      mcpUpsertBody({ ...stdioFields, argsText: "  \n", envText: "" }).args,
    ).toEqual([]);
  });
});

// --- models: an OBJECT with `available` + `modelRules`, not an array -------

interface ModelRef {
  providerId?: string;
  providerName?: string;
  modelId?: string;
}
interface ModelRule {
  providerId?: string;
  modelId?: string;
  contextWindow?: number;
}

describe("models payload", () => {
  const payload = {
    ok: true,
    models: {
      available: [
        {
          providerId: "account:bigmodel-individual-coding-plan",
          modelId: "glm-4.6",
        },
        { providerId: "builtin:bigmodel-coding-plan", modelId: "glm-4.5-air" },
      ],
      providers: [
        {
          providerId: "builtin:bigmodel-coding-plan",
          providerName: "Bigmodel",
        },
      ],
      modelRules: [
        {
          providerId: "account:bigmodel-individual-coding-plan",
          modelId: "glm-4.6",
          contextWindow: 200000,
        },
      ],
    },
  };

  it("reads the selectable list out of the object, not the object itself", () => {
    const available = payload.models.available;
    expect(Array.isArray(available)).toBe(true);
    expect(available).toHaveLength(2);
    // The bug this guards: treating `models` as an array gives `length`
    // undefined, which passes a `=== 0` check and then throws on `.map`.
    expect(Array.isArray(payload.models)).toBe(false);
  });

  it("joins the context window from modelRules by provider+model", () => {
    expect(
      contextWindowFor(payload.models.available[0]!, payload.models.modelRules),
    ).toBe(200000);
  });

  it("leaves the window absent when no rule carries properties", () => {
    expect(
      contextWindowFor(payload.models.available[1]!, payload.models.modelRules),
    ).toBeUndefined();
  });

  it("joins a rule across the two coding-plan spellings", () => {
    // `available` normalizes coding-plan ids to `builtin:*` server-side while
    // `modelRules` ride through verbatim (the desktop writes `account:*`
    // there) — an exact-string join misses the rule, its context window and
    // its reasoning levels, and the edit form then prefills empty.
    const rule = {
      providerId: "account:bigmodel-individual-coding-plan",
      modelId: "glm-4.5-air",
      contextWindow: 128000,
      reasoningLevels: ["low", "high"],
    };
    expect(
      contextWindowFor(
        { providerId: "builtin:bigmodel-coding-plan", modelId: "glm-4.5-air" },
        [rule],
      ),
    ).toBe(128000);
    const options = modelOptions({
      ...payload,
      models: {
        ...payload.models,
        modelRules: [...payload.models.modelRules, rule],
      },
    });
    expect(
      options.find((o) => o.modelId === "glm-4.5-air")?.reasoningLevels,
    ).toEqual(["low", "high"]);
  });

  it("builds picker options with each model's reasoning levels", () => {
    // The agent forms pick a model from this list; a rule that declares levels
    // must reach the picker, or the level dropdown is empty on a model that
    // supports reasoning.
    const rules = [
      ...payload.models.modelRules,
      {
        providerId: "builtin:bigmodel-coding-plan",
        modelId: "glm-4.5-air",
        reasoningLevels: ["high", "low"],
      },
    ];
    const options = modelOptions({
      ...payload,
      models: { ...payload.models, modelRules: rules },
    });
    expect(options).toHaveLength(2);
    expect(options[0]!.reasoningLevels).toEqual([]);
    expect(options[1]!.reasoningLevels).toEqual(["high", "low"]);
    // Duplicate entries (the union across providers) collapse on the key.
    expect(
      new Set(options.map((o) => `${o.providerId}/${o.modelId}`)).size,
    ).toBe(2);
  });

  it("keeps a picker option for a provider the rules block omits", () => {
    // The agent picker names models from `available`; an account plan provider
    // that `providers` omits must still reach the model list, or the picker
    // cannot express the session model.
    const ids = modelOptions(payload).map((o) => o.providerId);
    expect(ids).toContain("account:bigmodel-individual-coding-plan");
    expect(ids).toContain("builtin:bigmodel-coding-plan");
  });

  it("leaves account providers out of the add-model picker", () => {
    // The write route refuses `account:*` providers outright (their models come
    // from the coding plan), so offering one only produces an unactionable 400.
    const ids = providerOptions(payload).map((p) => p.id);
    expect(ids).toContain("builtin:bigmodel-coding-plan");
    expect(ids).not.toContain("account:bigmodel-individual-coding-plan");
  });

  it("labels models the way the session switcher does", () => {
    // One rule everywhere: a builtin/coding-plan model reads as its bare id,
    // a third-party one is qualified, and an id two providers share always
    // qualifies. The fixture's glm-4.6 is account-plan (bare); add a
    // third-party provider and a collision to cover the other two.
    const options = modelOptions({
      ok: true,
      models: {
        available: [
          ...payload.models.available,
          { providerId: "uuid-1", providerName: "Go", modelId: "deepseek-v4" },
          { providerId: "uuid-2", providerName: "Other", modelId: "glm-4.6" },
        ],
        providers: [],
        modelRules: [],
      },
    });
    const label = (providerId: string, modelId: string) =>
      options.find((o) => o.providerId === providerId && o.modelId === modelId)!
        .label;
    // Collision on glm-4.6: both carriers qualify, a nameless one with its id.
    expect(label("uuid-2", "glm-4.6")).toBe("Other › glm-4.6");
    expect(label("account:bigmodel-individual-coding-plan", "glm-4.6")).toBe(
      "account:bigmodel-individual-coding-plan › glm-4.6",
    );
    expect(label("builtin:bigmodel-coding-plan", "glm-4.5-air")).toBe(
      "glm-4.5-air",
    ); // builtin: bare
    expect(label("uuid-1", "deepseek-v4")).toBe("Go › deepseek-v4"); // third-party
  });
});

// --- provider spelling: `account:` vs `builtin:` -----------------------------
//
// The models payload normalizes coding-plan providers to the legacy `builtin:`
// spelling server-side (configProviderIdFor), while agent files written by the
// desktop keep the registry's `account:` spelling. Raw-string comparison never
// matches the two — every agent's model read as unmappable and the picker
// could not prefill.

describe("provider order decode", () => {
  it("orders by providerOrder, unmentioned ids keep file order", () => {
    // The reorder write's normalize rule, mirrored on the read side.
    const rows = orderedProviders({
      ok: true,
      models: {
        providers: [
          { providerId: "alpha", providerName: "Alpha" },
          {
            providerId: "beta",
            providerName: "Beta",
            baseUrl: "https://b.example",
            apiType: "openai-chat-completions",
            hasApiKey: true,
          },
          { providerId: "gamma" },
        ],
        providerOrder: ["beta", "ghost", "alpha"],
      },
    });
    expect(rows.map((r) => r.providerId)).toEqual(["beta", "alpha", "gamma"]);
    // The endpoint fields ride through for the form's prefill.
    expect(rows[0]).toMatchObject({
      providerName: "Beta",
      baseUrl: "https://b.example",
      apiType: "openai-chat-completions",
      hasApiKey: true,
    });
  });

  it("falls back to the block's own order when providerOrder is absent", () => {
    const rows = orderedProviders({
      ok: true,
      models: { providers: [{ providerId: "a" }, { providerId: "b" }] },
    });
    expect(rows.map((r) => r.providerId)).toEqual(["a", "b"]);
  });
});

describe("provider spelling normalization", () => {
  const planPayload = {
    ok: true,
    models: {
      available: [
        {
          providerId: "builtin:bigmodel-coding-plan",
          providerName: "BigModel",
          modelId: "GLM-5.3",
        },
      ],
      providers: [],
      modelRules: [],
    },
  };

  it("maps coding-plan spellings onto one match key", () => {
    expect(matchProviderId("account:bigmodel-individual-coding-plan")).toBe(
      "builtin:bigmodel-coding-plan",
    );
    expect(matchProviderId("account:acme-team-coding-plan")).toBe(
      "builtin:acme-coding-plan",
    );
    expect(matchProviderId("account:acme-start-coding-plan")).toBe(
      "builtin:acme-coding-plan",
    );
    // Anything else passes through untouched, whatever the prefix.
    expect(matchProviderId("builtin:bigmodel-coding-plan")).toBe(
      "builtin:bigmodel-coding-plan",
    );
    expect(matchProviderId("account:odd-shape")).toBe("account:odd-shape");
  });

  it("maps an agent file's model onto the payload's option", () => {
    // The real-world pair from ~/.zcode/agents: the file spells the plan
    // `account:…` (the desktop's registry spelling), the payload spells it
    // `builtin:…`. The cross-spelling match is what lets the picker prefill.
    const options = modelOptions(planPayload);
    expect(options[0]!.label).toBe("GLM-5.3");
    const stored = decodeSubagentModel(
      "custom:account%3Abigmodel-individual-coding-plan:GLM-5.3",
    );
    expect(stored).toEqual({
      providerId: "account:bigmodel-individual-coding-plan",
      modelId: "GLM-5.3",
    });
    expect(
      findModelOption(options, stored!.providerId, stored!.modelId),
    )?.toMatchObject({ modelId: "GLM-5.3" });
  });
});

// --- the frontmatter model spelling ----------------------------------------
//
// A personal agent's `model` is not `providerId/modelId` verbatim: a custom
// endpoint's provider id contains `/`, which the runtime's own parser splits
// at, so the desktop encodes it as `custom:<encoded provider>:<encoded model>`
// (subagent-markdown-selection.ts). A form that writes the raw value produces
// an agent the runtime reads as a different provider — or none.

describe("subagent model encoding", () => {
  it("keeps a plain id readable and encodes a colliding one", () => {
    expect(encodeSubagentModel("builtin:bigmodel", "glm-4.6")).toBe(
      "builtin:bigmodel/glm-4.6",
    );
    expect(
      encodeSubagentModel(
        "custom:account:bigmodel-individual-coding-plan",
        "GLM-5.3",
      ),
    ).toBe("custom:custom%3Aaccount%3Abigmodel-individual-coding-plan:GLM-5.3");
  });

  it("decodes the desktop's custom: spelling back to ids", () => {
    expect(
      decodeSubagentModel(
        "custom:account%3Abigmodel-individual-coding-plan:GLM-5.3",
      ),
    ).toEqual({
      providerId: "account:bigmodel-individual-coding-plan",
      modelId: "GLM-5.3",
    });
    expect(decodeSubagentModel("builtin:bigmodel/glm-4.6")).toEqual({
      providerId: "builtin:bigmodel",
      modelId: "glm-4.6",
    });
  });

  it("reads the legacy custom:builtin: spelling the way the desktop does", () => {
    // The provider id itself contains an UNENCODED colon, so the separator is
    // not the first one — treating it as such yields providerId "builtin" and
    // modelId "bigmodel-coding-plan:GLM-5.3", matching no picker option.
    expect(
      decodeSubagentModel("custom:builtin:bigmodel-coding-plan:GLM-5.3"),
    ).toEqual({
      providerId: "builtin:bigmodel-coding-plan",
      modelId: "GLM-5.3",
    });
  });

  it("round-trips through both spellings", () => {
    const encoded = encodeSubagentModel("custom:a:b", "GLM-5.3");
    expect(decodeSubagentModel(encoded)).toEqual({
      providerId: "custom:a:b",
      modelId: "GLM-5.3",
    });
  });

  it("returns null for a value it cannot map onto a picker option", () => {
    // The form then shows it read-only instead of substituting a model.
    expect(decodeSubagentModel("")).toBeNull();
    expect(decodeSubagentModel(undefined)).toBeNull();
    expect(decodeSubagentModel("inherit")).toBeNull();
    expect(decodeSubagentModel("noseparator")).toBeNull();
    expect(decodeSubagentModel("custom:onlyprovider")).toBeNull();
  });
});

// --- usage: wrapped in `{ok, usage}` --------------------------------------

describe("usage payload", () => {
  const wire = {
    ok: true,
    usage: {
      available: true,
      range: "7d" as const,
      summary: { totalTokens: 12345, requestCount: 7, models: 2 },
      models: [{ modelId: "glm-4.6", totalTokens: 9000, share: 0.72 }],
      daily: [
        {
          date: "2026-09-22",
          models: [{ modelId: "glm-4.6", totalTokens: 9000 }],
        },
      ],
    },
  };

  it("unwraps `usage` — the raw response has no `available` at the top", () => {
    // The bug this guards: storing the whole response made `usage.available`
    // undefined, so the "no data" empty state rendered forever.
    expect((wire as { available?: boolean }).available).toBeUndefined();
    expect(wire.usage.available).toBe(true);
    expect(wire.usage.summary.totalTokens).toBe(12345);
    expect(wire.usage.models).toHaveLength(1);
    expect(wire.usage.daily).toHaveLength(1);
  });

  it("treats a machine with no agent database as unavailable, not broken", () => {
    const empty = {
      ok: true,
      usage: { available: false, range: "7d" as const },
    };
    expect(empty.usage.available).toBe(false);
  });
});

// --- hooks: `{ok, hooks:{enabled, events: Record<event, matcher[]>}` -------

describe("hooks payload", () => {
  const wire = {
    ok: true,
    hooks: {
      enabled: true,
      events: {
        PostToolUse: [
          {
            matcher: "Bash",
            hooks: [{ command: "echo one" }, { command: "echo two" }],
          },
        ],
        SessionStart: [
          { hooks: [{ command: "boot", enabled: false, timeout: 5 }] },
        ],
      },
    },
    enabled: true,
  };

  it("reads events from inside `hooks`, not the top level", () => {
    // The bug this guards: `payload.events` is undefined, so the screen showed
    // its empty state on a machine that had hooks configured.
    expect((wire as { events?: unknown }).events).toBeUndefined();
    expect(wire.hooks.events).toBeDefined();
  });

  it("flattens the two-level matcher/hook nesting into rows", () => {
    const rows = flattenHooks(wire.hooks.events);
    expect(rows).toHaveLength(3);
    expect(rows.map((r) => r.key)).toEqual([
      "PostToolUse/0/0",
      "PostToolUse/0/1",
      "SessionStart/0/0",
    ]);
  });

  it("keeps the matcher as a label and defaults an absent one to any", () => {
    const rows = flattenHooks(wire.hooks.events);
    expect(rows[0]!.matcher).toBe("Bash");
    expect(rows[2]!.matcher).toBe("");
  });

  it("reads an absent `enabled` as on, and keeps the seconds timeout raw", () => {
    const rows = flattenHooks(wire.hooks.events);
    expect(rows[0]!.enabled).toBe(true);
    expect(rows[2]!.enabled).toBe(false);
    expect(rows[2]!.rawTimeoutSec).toBe(5);
  });

  it("keeps each timeout spelling raw, and either reads back in ms", () => {
    // The edit form writes back in the unit the entry already spells, so the
    // decode must not collapse the two into one field — editing a
    // `timeoutMs` entry must not add a second `timeout` next to it.
    const rows = flattenHooks({
      Mixed: [
        {
          hooks: [
            { command: "a", timeoutMs: 1500 },
            { command: "b", timeout: 90 },
            { command: "c" },
          ],
        },
      ],
    });
    expect(rows[0]!.rawTimeoutMs).toBe(1500);
    expect(rows[0]!.rawTimeoutSec).toBeUndefined();
    expect(timeoutMsOf(rows[0]!)).toBe(1500);
    expect(rows[1]!.rawTimeoutSec).toBe(90);
    expect(timeoutMsOf(rows[1]!)).toBe(90000);
    expect(timeoutMsOf(rows[2]!)).toBeUndefined();
    // The edit route addresses an entry by all three coordinates.
    expect(rows[1]!.matcherIndex).toBe(0);
    expect(rows[1]!.hookIndex).toBe(1);
  });

  it("reads an absent nested `enabled` as OFF, because false deletes the key", () => {
    // `setHooksEnabled(false)` removes `enabled` rather than storing false, and
    // the runtime treats the missing key as disabled. The bug this guards: a
    // `!== false` read shows the tree as live while every hook is inert.
    expect(hooksEnabled({ hooks: { events: {} } })).toBe(false);
  });

  it("reads the gate ON only when the route says so explicitly", () => {
    expect(
      hooksEnabled({ enabled: true, hooks: { enabled: true, events: {} } }),
    ).toBe(true);
    expect(
      hooksEnabled({ enabled: false, hooks: { enabled: true, events: {} } }),
    ).toBe(false);
  });

  it("falls back to the nested copy when the route omits its field", () => {
    // An older build that did not surface the top-level gate: the config's own
    // flag is all there is, and absent still means off.
    expect(hooksEnabled({ hooks: { enabled: true, events: {} } })).toBe(true);
    expect(hooksEnabled({ hooks: { events: {} } })).toBe(false);
  });

  it("prefers the route's top-level gate over the nested copy", () => {
    // Both are present and either being false means hooks are inert; the
    // route's own field is the authoritative one.
    expect(
      hooksEnabled({ enabled: false, hooks: { enabled: true, events: {} } }),
    ).toBe(false);
    expect(
      hooksEnabled({ enabled: true, hooks: { enabled: false, events: {} } }),
    ).toBe(true);
  });
});

// --- skills: `scope` decides what may be done ------------------------------

describe("skills payload", () => {
  const wire = [
    {
      name: "mine",
      path: "/h/.zcode/skills/mine/SKILL.md",
      scope: "user" as SkillScope,
      enabled: true,
    },
    {
      name: "plug",
      path: "/h/.zcode/cli/plugins/cache/p/SKILL.md",
      scope: "plugin" as SkillScope,
      enabled: true,
    },
    {
      name: "ws",
      path: "/w/.zcode/skills/ws/SKILL.md",
      scope: "project" as SkillScope,
      enabled: false,
    },
  ];

  it("counts only the user's own trees as already-user-scope", () => {
    // The bug this guards: reading a `userTree` field the bridge never sends
    // made every row look like it belonged there. `isUserTreeScope` is the
    // page's own predicate — imported, so a page that starts reading another
    // field fails here instead of shipping a wrong copy button.
    expect(wire.map((s) => isUserTreeScope(s.scope))).toEqual([
      true,
      false,
      false,
    ]);
  });

  it("offers copy-to-user for exactly the scopes outside the user's trees", () => {
    expect(wire.map((s) => !isUserTreeScope(s.scope))).toEqual([
      false,
      true,
      true,
    ]);
  });
});

// --- skill detail: `{ok, skill}` — the row → detail view -------------------

describe("skill detail decode", () => {
  const wire = {
    ok: true,
    skill: {
      name: "pdf",
      description: "PDF tools",
      metadata: { name: "pdf", "allowed-tools": "Read, View" },
      body: "# pdf\n",
      path: "/h/.zcode/skills/pdf/SKILL.md",
      dir: "/h/.zcode/skills/pdf",
      scope: "user",
      enabled: false,
      deletable: true,
    },
  };

  it("reads the skill whole — frontmatter keys as written, body included", () => {
    // The detail's whole point is fidelity: every frontmatter key (hyphenated
    // ones included) and the body after the block must survive the decode.
    const skill = decodeSkillDetail(wire);
    expect(skill?.name).toBe("pdf");
    expect(skill?.metadata?.["allowed-tools"]).toBe("Read, View");
    expect(skill?.body).toBe("# pdf\n");
    expect(skill?.enabled).toBe(false);
    expect(skill?.deletable).toBe(true);
  });

  it("reads a body without a skill object as null, never as an empty skill", () => {
    // A malformed answer is a load failure in the view; inventing an empty
    // skill would paint a blank detail over a file that exists.
    expect(decodeSkillDetail({ ok: true })).toBeNull();
    expect(decodeSkillDetail(null)).toBeNull();
  });

  it("keeps a skill with only required fields paintable", () => {
    const skill = decodeSkillDetail({ ok: true, skill: { name: "bare" } });
    expect(skill?.name).toBe("bare");
    expect(skill?.metadata).toBeUndefined();
    expect(skill?.body).toBeUndefined();
  });
});

// --- agents: `readOnly` is the server's verdict ---------------------------

describe("agents payload", () => {
  const wire = [
    {
      name: "general-purpose",
      frontmatter: { name: "general-purpose", description: "General agent" },
      systemPrompt: "…",
      enabled: true,
      readOnly: true,
    },
    {
      name: "mine",
      frontmatter: { name: "mine", description: "My agent" },
      systemPrompt: "…",
      enabled: true,
      readOnly: false,
      modelSelection: { modelId: "glm-4.6", thoughtLevel: "high" },
    },
  ];

  it("hides the controls for a read-only agent", () => {
    // The bug this guards: reading a `builtIn` field the bridge never sends
    // left the enable and delete controls on built-ins, where every call is
    // refused with a 400 the user cannot act on. `isAgentReadOnly` is the
    // page's own predicate, imported so the wire shape is checked against the
    // code that reads it rather than against a copy of it.
    expect(isAgentReadOnly(wire[0]!.readOnly)).toBe(true);
    expect((wire[0] as { builtIn?: boolean }).builtIn).toBeUndefined();
    expect(isAgentReadOnly(wire[1]!.readOnly)).toBe(false);
  });

  it("reads the description from the frontmatter and the override from modelSelection", () => {
    expect(wire[0]!.frontmatter.description).toBe("General agent");
    expect(wire[1]!.modelSelection?.modelId).toBe("glm-4.6");
    expect(wire[1]!.modelSelection?.thoughtLevel).toBe("high");
  });
});

// --- reset cards: the provider list is all four ids, not the account's -----

describe("reset-card eligibility", () => {
  const wire = {
    ok: true,
    resetCards: {
      providers: [
        "account:bigmodel-individual-coding-plan",
        "account:bigmodel-team-coding-plan",
        "account:zai-individual-coding-plan",
        "account:zai-team-coding-plan",
      ],
      credentials: true,
    },
  };

  it("never assumes a single provider — the hub lists all four ids", () => {
    // The bug this guards: a `providers.length === 1` check could never be
    // true, so the whole reset-card feature was unreachable.
    expect(wire.resetCards.providers).toHaveLength(4);
    expect(wire.resetCards.providers.length === 1).toBe(false);
  });

  it("separates 'no credentials' from 'no plan'", () => {
    // credentials:false means the store could not be decrypted — a normal
    // state, and a different message from having nothing to spend.
    const noCreds = { ...wire.resetCards, credentials: false };
    expect(noCreds.credentials).toBe(false);
    expect(wire.resetCards.credentials).toBe(true);
  });

  it("still refuses to spend on a team plan", () => {
    expect(isTeamPlan(wire.resetCards.providers[1]!)).toBe(true);
    expect(isTeamPlan(wire.resetCards.providers[0]!)).toBe(false);
  });
});

// --- the confirm threshold -------------------------------------------------

describe("spend threshold", () => {
  const usage = {
    glm: {
      items: [
        { key: "token_5h", usedPercent: 20 },
        { key: "token_week", usedPercent: 100 },
      ],
    },
  };

  it("reads each card's own window, never the max across them", () => {
    // The keys are the server's own (quota/parse.ts deriveLabel); a card that
    // clears only the 5-hour window must not read the exhausted week.
    expect(windowUsedPercent(usage, "FIVE_HOUR")).toBe(20);
    expect(windowUsedPercent(usage, "WEEK")).toBe(100);
  });

  it("keeps the warning when the card's window is not listed", () => {
    expect(windowUsedPercent(null, "WEEK")).toBe(0);
    expect(
      windowUsedPercent(
        { glm: { items: [{ key: "mcp", usedPercent: 100 }] } },
        "WEEK",
      ),
    ).toBe(0);
  });
});
