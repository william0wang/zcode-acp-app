import type {
  AgentUpsert,
  ConversationRunsResponse,
  FsBrowseResult,
  FsListing,
  HookEntryPatch,
  HubCreateInstanceResult,
  HubHistoryCursor,
  HubHistoryPage,
  HubInstance,
  HubProject,
  HubUpgradeResult,
  McpServerUpsert,
  ModelUpsert,
  ResetCardStatus,
  SettingsAll,
  SystemStatsResponse,
  AmendRunSettingsResponse,
  WorkflowArtifactItemsResponse,
  WorkflowArtifactReadResponse,
  WorkflowCreatePromptResponse,
  WorkflowDetailResponse,
  WorkflowGateModeSetting,
  WorkflowGateSetResponse,
  WorkflowListResponse,
  WorkflowNodeResultResponse,
  WorkflowOverviewResponse,
  WorkflowRunEventsResponse,
  WorkflowRunsHistoryResponse,
  WorkflowScope,
  WorkflowStartResponse,
  WorkflowWorkspaceResponse,
  WriteEffect,
} from "./types";

export class HubApiError extends Error {
  // `network` = the fetch itself failed (hub unreachable): the hub lives and
  // dies with the editor, so this is an expected "offline" state, not an error.
  constructor(
    message: string,
    readonly status?: number,
    readonly network = false,
    // The hub no longer knows, or can no longer reach, the instance this
    // request targeted (idle-exited bridge under a still-open session): the
    // caller may respawn the instance instead of surfacing a plain failure.
    readonly instanceDead = false,
  ) {
    super(message);
    this.name = "HubApiError";
  }
}

export class HubClient {
  constructor(
    private readonly hubUrl: string,
    private readonly token: string,
  ) {}

  private url(path: string): string {
    return this.hubUrl.replace(/\/+$/, "") + path;
  }

  private async fetch(
    path: string,
    // The configuration API (ADR-0025) writes with PUT and DELETE as well as
    // POST; the discovery endpoints only ever needed the first two.
    method: "GET" | "POST" | "PUT" | "DELETE" = "GET",
    body?: unknown,
    // Slow-by-design routes override (null = wait indefinitely). A phone
    // waking from deep sleep can park fetch() on a socket that never
    // delivers a byte (no FIN, no RST — the old route is simply gone);
    // without a deadline the reconnect loop's discovery await hangs
    // forever and the banner wedges on "reconnecting". Aborts surface as
    // network errors, which callers already treat as "hub unreachable,
    // retry". Covers connect + response headers only: every observed hang
    // mode delivers zero bytes, so headers never arrive — once they have,
    // the body follows on the same hot socket.
    opts?: { timeoutMs?: number | null; passthrough?: number[] },
  ): Promise<Response> {
    const timeoutMs = opts?.timeoutMs !== undefined ? opts.timeoutMs : 10_000;
    const controller = new AbortController();
    const timer =
      timeoutMs === null
        ? null
        : setTimeout(() => controller.abort(), timeoutMs);
    let res: Response;
    try {
      res = await fetch(this.url(path), {
        method,
        headers: {
          Authorization: `Bearer ${this.token}`,
          ...(body !== undefined ? { "Content-Type": "application/json" } : {}),
        },
        ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
        signal: controller.signal,
      });
    } catch (e) {
      const err = e as Error;
      throw new HubApiError(
        err.name === "AbortError"
          ? `network error: no response within ${timeoutMs}ms (${path})`
          : `network error: ${err.message}`,
        undefined,
        true,
      );
    } finally {
      if (timer !== null) clearTimeout(timer);
    }
    if (res.status === 401)
      throw new HubApiError("unauthorized: check token", 401);
    // Callers that need to inspect a non-2xx BODY (fetchFs sniffs the hub's
    // plain-text dead-instance answers) get the raw response back instead.
    if (opts?.passthrough?.includes(res.status)) return res;
    if (!res.ok) {
      // The settings API answers a refused write with a REASON — "nonce is
      // stale — refresh the reset card status", "that is a built-in agent",
      // "url must be an official ZCode CDN artifact". Dropping it leaves the
      // user with "HTTP 409" and no way forward, so the body becomes the
      // message whenever it says something. The workflow routes carry BOTH a
      // short reason token (`error`) and the human detail (`message`, e.g.
      // compile diagnostics) — prefer the detail when present.
      const detail = await res
        .json()
        .then((b: unknown) => {
          const body = b as { error?: unknown; message?: unknown } | null;
          if (typeof body?.message === "string" && body.message)
            return body.message;
          const err = body?.error;
          return typeof err === "string" && err ? err : null;
        })
        .catch(() => null);
      throw new HubApiError(detail ?? `HTTP ${res.status}`, res.status);
    }
    return res;
  }

  // Body is plain text "ok".
  async health(): Promise<void> {
    await (await this.fetch("/api/health")).text();
  }

  /**
   * Fetch registered instances. With `probe` the hub TCP-probes each bridge
   * first and prunes unreachable ones (hard-killed bridges otherwise linger
   * for the 30s heartbeat TTL); use it where an immediately-honest list
   * matters — manual refresh, reconnect liveness checks.
   */
  async instances(probe = false): Promise<HubInstance[]> {
    const res = await this.fetch(`/api/instances${probe ? "?probe=1" : ""}`);
    const data: unknown = await res.json();
    if (!Array.isArray(data))
      throw new HubApiError("unexpected /api/instances payload");
    return data as HubInstance[];
  }

  /**
   * Account quota (bridge 0.8.0, ADR-0005): same payload as the ACP
   * `account/usage_stats` method but no instance connection needed. The hub
   * caches ~30s server-side; 502 = upstream query failed (retry later).
   */
  async quota(): Promise<unknown> {
    const res = await this.fetch("/api/quota");
    return res.json();
  }

  /**
   * The one-line quota dock string (ADR-0021) — the exact line the Martty TUI
   * renders in its resident bottom dock, preformatted hub-side (`45% 14:23 ·
   * 12% 10-18 · go 8% 10-11`). `null` = no quota data (hide, not an error).
   */
  async quotaDock(): Promise<string | null> {
    const res = await this.fetch("/api/quota/dock");
    const data = (await res.json()) as { formatted?: unknown };
    return typeof data.formatted === "string" ? data.formatted : null;
  }

  /**
   * Trigger the hub's staleness check (bridge 0.11.7). The hub alone decides
   * whether the on-disk code is newer and restarts onto it; `restarting`
   * just means it is about to exit and re-spawn — poll health() until it
   * answers, then re-fetch instances.
   */
  async upgrade(): Promise<HubUpgradeResult> {
    const res = await this.fetch("/api/upgrade", "POST");
    return (await res.json()) as HubUpgradeResult;
  }

  /**
   * Known-project list (bridge 0.17.0, ADR-0014), newest activity first.
   * Every workspace that ever ran a session — this list doubles as the
   * create whitelist, so the paths it returns are exactly the ones
   * createInstance() accepts.
   */
  async projects(): Promise<HubProject[]> {
    const res = await this.fetch("/api/projects");
    const data: unknown = await res.json();
    if (!Array.isArray(data))
      throw new HubApiError("unexpected /api/projects payload");
    return data as HubProject[];
  }

  /**
   * One directory level of the hub machine's filesystem (bridge 0.65.0,
   * browse-and-create). Absent `path` lists the home dir; navigate by the
   * RESOLVED `path` in the answer, never by what was sent. `creatable` flags
   * whether createInstance() accepts the listed directory itself — with no
   * remote.projectRoots configured it is true only on known projects.
   * 404 = missing path/file (or a bridge too old for the route).
   */
  async fsBrowse(path?: string): Promise<FsBrowseResult> {
    const q = path ? `?path=${encodeURIComponent(path)}` : "";
    const res = await this.fetch(`/api/fs/list${q}`);
    return (await res.json()) as FsBrowseResult;
  }

  /**
   * Create (or reuse) a serve bridge for one known project (bridge 0.17.0,
   * ADR-0014). The hub spawns a bridge in the project's cwd and waits for its
   * registration; `reused` means a live serve instance for the same workspace
   * already existed. Since ADR-0016 the spawn opens a VISIBLE terminal REPL
   * on the desktop (headless is the fallback), so the registration budget is
   * ~20s — let the fetch run, no client-side timeout. 403 = path not on the
   * projects list; 502 = spawn failed or the bridge never registered.
   *
   * `sessionId` (ADR-0017, bridge-side resume) makes the spawned TUI boot
   * straight into that session instead of a fresh conversation; the answer is
   * always the NEW instance (resume never reuses a live serve bridge).
   */
  async createInstance(
    workspacePath: string,
    sessionId?: string,
  ): Promise<HubCreateInstanceResult> {
    const res = await this.fetch(
      "/api/instances",
      "POST",
      { workspacePath, ...(sessionId ? { sessionId } : {}) },
      { timeoutMs: null },
    );
    return (await res.json()) as HubCreateInstanceResult;
  }

  /**
   * One page of a project's session history (bridge 0.19.0, ADR-0015): the
   * backend session store, closed conversations included, newest first.
   * `cursor` is the previous response's `nextCursor` passed back VERBATIM —
   * the composite `{before, beforeId}` names the exact last row, so
   * timestamps tied across a page boundary never skip rows. A cold project's
   * first page incubates its serve bridge (~12s budget) — let it run.
   */
  async projectSessions(
    workspacePath: string,
    cursor?: HubHistoryCursor,
  ): Promise<HubHistoryPage> {
    const q = new URLSearchParams({ workspacePath });
    if (cursor) {
      q.set("before", String(cursor.before));
      q.set("beforeId", cursor.beforeId);
    }
    const res = await this.fetch(`/api/projects/sessions?${q}`, "GET", undefined, {
      timeoutMs: null,
    });
    return (await res.json()) as HubHistoryPage;
  }

  /**
   * Soft-deletes (tombstones) a whole project (hub ≥0.53, server ADR-0031):
   * every tasks row of the workspace flips to deleted=1, hiding the project
   * and all its sessions from every listing (both app surfaces, the CLI
   * /resume picker, the desktop sidebar — same index). Bytes are kept, and a
   * NEW session in the project records a fresh row so it reappears — a hide,
   * not a ban. Idempotent.
   */
  async deleteProject(workspacePath: string): Promise<void> {
    await this.fetch("/api/projects/delete", "POST", { workspacePath });
  }

  /**
   * Retires a session from remote discovery (ADR-0006). Close, not delete —
   * backend store and editor storage are untouched; an editor-side-still-open
   * conversation self-heals back into discovery on its next use. Refused with
   * 409 while a turn is running.
   */
  async closeSession(instanceId: string, sessionId: string): Promise<void> {
    await this.fetch(
      `/api/instances/${instanceId}/sessions/${encodeURIComponent(sessionId)}/close`,
      "POST",
    );
  }

  /**
   * Terminates a remote-incubated bridge — instance-level, the "close the
   * session window" counterpart for app-created instances. Editor-origin
   * bridges are refused server-side with 403.
   */
  async shutdownInstance(instanceId: string): Promise<void> {
    await this.fetch(`/api/instances/${instanceId}/shutdown`, "POST");
  }

  /**
   * Renames a session (bridge 0.11.9). The title is set once automatically
   * from the first prompt; this is the only later modifier — the bridge pins
   * it (title_overridden) and broadcasts session_info_update so attached
   * editors update live.
   */
  async renameSession(
    instanceId: string,
    sessionId: string,
    title: string,
  ): Promise<void> {
    await this.fetch(
      `/api/instances/${instanceId}/sessions/${encodeURIComponent(sessionId)}/rename`,
      "POST",
      { title },
    );
  }

  /**
   * Soft-deletes (tombstones) a session from EVERY listing (bridge ≥0.53,
   * server ADR-0031): project history, discovery, the CLI /resume picker,
   * and the desktop app's sidebar — the conversation bytes stay in the
   * backend store, nothing is purged. Refused with 409 while the session
   * is live or a turn is in flight (deleting history is this route's job);
   * 503 when the tasks index is unavailable.
   */
  async deleteSession(instanceId: string, sessionId: string): Promise<void> {
    await this.fetch(
      `/api/instances/${instanceId}/sessions/${encodeURIComponent(sessionId)}/delete`,
      "POST",
    );
  }

  /**
   * Session Files (ADR-0005). A 404 right after a bridge upgrade is the hub
   * self-learning the fs route — retried once transparently.
   */
  private async fetchFs(path: string): Promise<Response> {
    // 404/502 bodies are needed here (dead-instance sniffing), so both
    // statuses ride the passthrough instead of the generic HTTP error.
    const opts = { passthrough: [404, 502] as number[] };
    let res = await this.fetch(path, "GET", undefined, opts);
    if (res.status === 404) {
      // Bridge-upgrade race: the route briefly 404s around a respawn —
      // retry once before believing it.
      await new Promise((r) => setTimeout(r, 400));
      res = await this.fetch(path, "GET", undefined, opts);
    }
    if (res.status === 404 || res.status === 502) {
      // The hub's instance routes answer these two exact plain-text bodies
      // when the bridge is gone (idle-exit under a still-open session). They
      // must not surface as a JSON-parse failure: flag them so the store can
      // respawn the instance. Genuine 404s ("not found") pass through with
      // the body re-wrapped.
      const text = await res.text();
      if (text === "unknown instance" || text === "bridge unreachable") {
        throw new HubApiError(text, res.status, false, true);
      }
      return new Response(text, { status: res.status, headers: res.headers });
    }
    return res;
  }

  /** One directory level; dotfiles included (filter client-side). */
  async fsList(
    instanceId: string,
    sessionId: string,
    path: string,
  ): Promise<FsListing> {
    const q = new URLSearchParams({ sessionId, path });
    const res = await this.fetchFs(`/api/instances/${instanceId}/fs/list?${q}`);
    return (await res.json()) as FsListing;
  }

  /**
   * Text line window (default 200, cap 5000). `firstLine` comes from the
   * X-Zcode-First-Line header — always trust it over the request's `line`.
   */
  async fsFileText(
    instanceId: string,
    sessionId: string,
    path: string,
    line: number,
    limit: number,
  ): Promise<{ firstLine: number; text: string }> {
    const q = new URLSearchParams({
      sessionId,
      path,
      line: String(line),
      limit: String(limit),
    });
    const res = await this.fetchFs(`/api/instances/${instanceId}/fs/file?${q}`);
    const firstLine = Number(res.headers.get("X-Zcode-First-Line")) || line;
    const raw = await res.text();
    // The line window always ends with a trailing "\n" (server emits
    // lines.join("\n") + "\n"); strip it so line counts and window stitching
    // in the viewer stay exact.
    return {
      firstLine,
      text: raw.endsWith("\n") ? raw.slice(0, -1) : raw,
    };
  }

  /**
   * Absolute fs/file URL with the token in the query string — for `<img src>`
   * and download links, which cannot carry an Authorization header.
   */
  fsFileUrl(instanceId: string, sessionId: string, path: string): string {
    const q = new URLSearchParams({ sessionId, path, token: this.token });
    return this.url(`/api/instances/${instanceId}/fs/file?${q}`);
  }

  // ---- ZCode configuration (bridge 0.47.0, server ADR-0025) ----
  //
  // Every call goes through the hub's machine-level mount, which serves the
  // same handler factory the bridge mounts on its own loopback — so the two
  // cannot drift apart — and needs no registered bridge. Reads are plain GETs;
  // writes carry an `effect` the caller must surface ("immediate" landed,
  // "needs-restart" waits for a backend restart).

  /** One-shot snapshot of every section (first screen of a config page). */
  async settingsAll(): Promise<SettingsAll> {
    const res = await this.fetch("/api/settings/all");
    return (await res.json()) as SettingsAll;
  }

  async settingsModels(): Promise<unknown> {
    const res = await this.fetch("/api/settings/models");
    return res.json();
  }

  async settingsSkills(): Promise<unknown> {
    const res = await this.fetch("/api/settings/skills");
    return res.json();
  }

  async settingsMcp(): Promise<unknown> {
    const res = await this.fetch("/api/settings/mcp");
    return res.json();
  }

  async settingsHooks(): Promise<unknown> {
    const res = await this.fetch("/api/settings/hooks");
    return res.json();
  }

  async settingsAgents(): Promise<unknown> {
    const res = await this.fetch("/api/settings/agents");
    return res.json();
  }

  /** `range` is 7d | 30d | all. */
  async settingsUsage(range: string): Promise<unknown> {
    const res = await this.fetch(
      `/api/settings/usage?range=${encodeURIComponent(range)}`,
    );
    return res.json();
  }

  /**
   * Platform usage (account-level monitor data). `range` is today | 7d | 30d.
   * A 404 means the bridge predates the route — the caller treats it as an
   * upgrade hint, not a settings-API capability gap.
   */
  async settingsUsagePlatform(range: string): Promise<unknown> {
    const res = await this.fetch(
      `/api/settings/usage-platform?range=${encodeURIComponent(range)}`,
    );
    return res.json();
  }

  async settingsQuota(): Promise<unknown> {
    const res = await this.fetch("/api/settings/quota");
    return res.json();
  }

  /**
   * Reset-card inventory for one coding-plan provider. The `nonce` in the
   * answer is what `spendResetCard` must send back — a spend is tied to a
   * status read made moments earlier, so a stale screen cannot burn a card.
   */
  async resetCardStatus(providerId: string): Promise<ResetCardStatus> {
    const res = await this.fetch(
      `/api/settings/reset-cards?providerId=${encodeURIComponent(providerId)}`,
    );
    return (await res.json()) as ResetCardStatus;
  }

  async spendResetCard(input: {
    providerId: string;
    resetType: "FIVE_HOUR" | "WEEK";
    nonce: string;
    idempotencyKey: string;
  }): Promise<unknown> {
    const res = await this.fetch(
      "/api/settings/reset-cards/use",
      "POST",
      input,
    );
    return res.json();
  }

  async requestResetOpportunity(input: {
    providerId: string;
    idempotencyKey: string;
  }): Promise<{
    ok: boolean;
    opportunity?: { granted: boolean; nextTryAt: number | null };
  }> {
    const res = await this.fetch(
      "/api/settings/reset-cards/opportunity",
      "POST",
      input,
    );
    return (await res.json()) as {
      ok: boolean;
      opportunity?: { granted: boolean; nextTryAt: number | null };
    };
  }

  async markResetHistoryRead(providerId: string): Promise<void> {
    await this.fetch("/api/settings/reset-cards/history-read", "POST", {
      providerId,
    });
  }

  async settingsBackups(): Promise<unknown> {
    const res = await this.fetch("/api/settings/backups");
    return res.json();
  }

  /**
   * Whether one bridge has recorded a needs-restart write since its last
   * restart. Per-instance on purpose: the hub-local spelling answers 409,
   * because the hub has no backend whose restart could clear its own answer.
   */
  async settingsPendingRestart(instanceId: string): Promise<{
    pendingRestart: boolean;
    writes: number;
  }> {
    const res = await this.fetch(
      `/api/instances/${instanceId}/settings/pending-restart`,
    );
    return (await res.json()) as { pendingRestart: boolean; writes: number };
  }

  async settingsAppUpdate(channel?: string): Promise<unknown> {
    const q = channel ? `?channel=${encodeURIComponent(channel)}` : "";
    const res = await this.fetch(`/api/settings/app-update${q}`);
    return res.json();
  }

  // ---- configuration writes ----
  //
  // Each returns the write's effect class. `send` is shared so the 4xx bodies
  // (a refused write says WHY — a non-coding-plan provider, a stale nonce)
  // reach the caller instead of collapsing into a bare status code.

  async updateProvider(
    providerId: string,
    body: Record<string, unknown>,
  ): Promise<WriteEffect> {
    const res = await this.fetch(
      `/api/settings/providers/${encodeURIComponent(providerId)}`,
      "PUT",
      body,
    );
    return (await res.json()) as WriteEffect;
  }

  async addModel(body: ModelUpsert): Promise<WriteEffect> {
    const res = await this.fetch("/api/settings/models", "POST", body);
    return (await res.json()) as WriteEffect;
  }

  async removeModel(providerId: string, modelId: string): Promise<WriteEffect> {
    const res = await this.fetch(
      `/api/settings/models/${encodeURIComponent(providerId)}/${encodeURIComponent(modelId)}`,
      "DELETE",
    );
    return (await res.json()) as WriteEffect;
  }

  async setSkillEnabled(body: {
    path: string;
    enable: boolean;
  }): Promise<WriteEffect> {
    const res = await this.fetch("/api/settings/skills/enable", "POST", body);
    return (await res.json()) as WriteEffect;
  }

  async copySkillToUser(body: { path: string }): Promise<WriteEffect> {
    const res = await this.fetch(
      "/api/settings/skills/copy-to-user",
      "POST",
      body,
    );
    return (await res.json()) as WriteEffect;
  }

  async deleteSkill(skillPath: string): Promise<WriteEffect> {
    const res = await this.fetch(
      `/api/settings/skills/${encodeURIComponent(skillPath)}`,
      "DELETE",
    );
    return (await res.json()) as WriteEffect;
  }

  async upsertMcpServer(
    name: string,
    body: McpServerUpsert,
  ): Promise<WriteEffect> {
    const res = await this.fetch(
      `/api/settings/mcp/${encodeURIComponent(name)}`,
      "PUT",
      body,
    );
    return (await res.json()) as WriteEffect;
  }

  async removeMcpServer(name: string): Promise<WriteEffect> {
    const res = await this.fetch(
      `/api/settings/mcp/${encodeURIComponent(name)}`,
      "DELETE",
    );
    return (await res.json()) as WriteEffect;
  }

  /** The route reads `enabled` (not `enable`, unlike skills and agents). */
  async setMcpEnabled(name: string, enable: boolean): Promise<WriteEffect> {
    const res = await this.fetch("/api/settings/mcp/enable", "POST", {
      name,
      enabled: enable,
    });
    return (await res.json()) as WriteEffect;
  }

  /**
   * Edit one existing hook entry.
   *
   * The entry is addressed by THREE coordinates: the event, the index of its
   * matcher group in that event, and the index of the hook inside the group —
   * the last one travels in the body because the server's route only takes two
   * path segments. There is no way to add or remove an entry through here; a
   * hook that does not exist yet must be written into the config file.
   */
  async updateHook(
    eventName: string,
    matcherIndex: number,
    hookIndex: number,
    body: HookEntryPatch,
  ): Promise<WriteEffect> {
    const res = await this.fetch(
      `/api/settings/hooks/${encodeURIComponent(eventName)}/${matcherIndex}`,
      "PUT",
      { ...body, hookIndex },
    );
    return (await res.json()) as WriteEffect;
  }

  async setHooksEnabled(enabled: boolean): Promise<WriteEffect> {
    const res = await this.fetch("/api/settings/hooks/enabled", "POST", {
      enabled,
    });
    return (await res.json()) as WriteEffect;
  }

  async upsertAgent(name: string, body: AgentUpsert): Promise<WriteEffect> {
    const res = await this.fetch(
      `/api/settings/agents/${encodeURIComponent(name)}`,
      "PUT",
      body,
    );
    return (await res.json()) as WriteEffect;
  }

  async deleteAgent(name: string): Promise<WriteEffect> {
    const res = await this.fetch(
      `/api/settings/agents/${encodeURIComponent(name)}`,
      "DELETE",
    );
    return (await res.json()) as WriteEffect;
  }

  async setAgentEnabled(name: string, enable: boolean): Promise<WriteEffect> {
    const res = await this.fetch(
      `/api/settings/agents/${encodeURIComponent(name)}/enable`,
      "POST",
      { enable },
    );
    return (await res.json()) as WriteEffect;
  }

  async restoreBackup(body: {
    file: string;
    path: string;
  }): Promise<WriteEffect> {
    // Size-dependent file copy — minutes would be wrong, but 10s is too.
    const res = await this.fetch("/api/settings/backups/restore", "POST", body, {
      timeoutMs: 60_000,
    });
    return (await res.json()) as WriteEffect;
  }

  async installAppUpdate(body: {
    version: string;
    url: string;
    channel?: string;
  }): Promise<unknown> {
    // The hub machine downloads the artifact before answering — however
    // long its own network takes.
    const res = await this.fetch(
      "/api/settings/app-update/install",
      "POST",
      body,
      { timeoutMs: null },
    );
    return res.json();
  }

  // ---- dynamic-workflow management (bridge 0.48.0, server ADR-0029) ----
  //
  // Every route is PER-INSTANCE: the hub's machine-level settings mount has
  // no backend (its `/settings/all` reports the workflow gate as a bare
  // `{available:false}` — no `enabled` field), so each call names the bridge
  // through the per-instance proxy. Run-detail queries additionally name the
  // ACP session that owns the run (`sessionId`); the bridge resolves it to
  // the backend id.

  /** Base path for one bridge's per-instance settings routes. */
  private instSettings(instanceId: string): string {
    return `/api/instances/${encodeURIComponent(instanceId)}/settings`;
  }

  /**
   * The per-instance `/settings/all` — the only mount whose `workflow` block
   * carries the real gate verdict (`{enabled, mode, source}`). The machine-
   * level snapshot the config screens use cannot answer "is the feature on".
   */
  async instanceSettingsAll(instanceId: string): Promise<SettingsAll> {
    const res = await this.fetch(`${this.instSettings(instanceId)}/all`);
    return (await res.json()) as SettingsAll;
  }

  /**
   * The machine-level gate override switch (bridge ≥0.53): persists
   * `workflow.mode` in the bridge's user config ("auto" = follow the remote
   * verdict) and answers the effective verdict the next gate read sees.
   */
  async workflowGateSet(
    instanceId: string,
    mode: WorkflowGateModeSetting,
  ): Promise<WorkflowGateSetResponse> {
    const res = await this.fetch(
      `${this.instSettings(instanceId)}/workflow-gate`,
      "PUT",
      { mode },
    );
    return (await res.json()) as WorkflowGateSetResponse;
  }

  async workflowsList(
    instanceId: string,
    scope?: WorkflowScope,
  ): Promise<WorkflowListResponse> {
    const q = scope ? `?scope=${scope}` : "";
    const res = await this.fetch(
      `${this.instSettings(instanceId)}/workflows${q}`,
    );
    return (await res.json()) as WorkflowListResponse;
  }

  /**
   * The machine-level workflow page payload (hub, bridge 0.60.0): one group
   * per workspace (deduped hub-side), pre-joined badges, plus the machine-wide
   * active-run list. Not instance-addressed — the hub aggregates every
   * registered bridge; the app renders the answer as-is.
   */
  async workflowOverview(): Promise<WorkflowOverviewResponse> {
    const res = await this.fetch("/api/workflow-overview");
    return (await res.json()) as WorkflowOverviewResponse;
  }

  /**
   * Machine-level system status (`/api/system-stats`, bridge 0.62.0): host
   * info, CPU/memory/storage, battery, power assertions, network, and the
   * hub/bridge process resources. Not instance-addressed; rate fields need
   * two reads ~1s apart (the hub deltas across reads — first read is null).
   */
  async systemStats(): Promise<SystemStatsResponse> {
    const res = await this.fetch("/api/system-stats");
    return (await res.json()) as SystemStatsResponse;
  }

  async workflowGet(
    instanceId: string,
    scope: WorkflowScope,
    name: string,
  ): Promise<WorkflowDetailResponse> {
    const res = await this.fetch(
      `${this.instSettings(instanceId)}/workflows/${encodeURIComponent(scope)}/${encodeURIComponent(name)}`,
    );
    return (await res.json()) as WorkflowDetailResponse;
  }

  async workflowUpdateMeta(
    instanceId: string,
    scope: WorkflowScope,
    name: string,
    meta: Record<string, unknown>,
  ): Promise<unknown> {
    const res = await this.fetch(
      `${this.instSettings(instanceId)}/workflows/${encodeURIComponent(scope)}/${encodeURIComponent(name)}/meta`,
      "PUT",
      meta,
    );
    return res.json();
  }

  async workflowDelete(
    instanceId: string,
    scope: WorkflowScope,
    name: string,
  ): Promise<unknown> {
    const res = await this.fetch(
      `${this.instSettings(instanceId)}/workflows/${encodeURIComponent(scope)}/${encodeURIComponent(name)}`,
      "DELETE",
    );
    return res.json();
  }

  async workflowMove(
    instanceId: string,
    scope: WorkflowScope,
    name: string,
  ): Promise<unknown> {
    const res = await this.fetch(
      `${this.instSettings(instanceId)}/workflows/${encodeURIComponent(scope)}/${encodeURIComponent(name)}/move`,
      "POST",
    );
    return res.json();
  }

  async workflowStart(
    instanceId: string,
    scope: WorkflowScope,
    name: string,
    body: { args?: Record<string, unknown>; sessionId?: string },
  ): Promise<WorkflowStartResponse> {
    const res = await this.fetch(
      `${this.instSettings(instanceId)}/workflows/${encodeURIComponent(scope)}/${encodeURIComponent(name)}/start`,
      "POST",
      body,
    );
    return (await res.json()) as WorkflowStartResponse;
  }

  async workflowResume(
    instanceId: string,
    runId: string,
    body: { sessionId: string; name?: string },
  ): Promise<unknown> {
    const res = await this.fetch(
      `${this.instSettings(instanceId)}/workflow-runs/${encodeURIComponent(runId)}/resume`,
      "POST",
      body,
    );
    return res.json();
  }

  /**
   * Stop a flying run (bridge 0.56.0): `session/cancelBackgroundTask` via the
   * hub, so a run on ANY instance is stoppable — the live ACP socket only
   * reaches the instance it is bound to. 409 = not running (already settled).
   */
  async workflowStop(
    instanceId: string,
    runId: string,
    body: { sessionId: string },
  ): Promise<unknown> {
    const res = await this.fetch(
      `${this.instSettings(instanceId)}/workflow-runs/${encodeURIComponent(runId)}/stop`,
      "POST",
      body,
    );
    return res.json();
  }

  /**
   * Amend a run's settings (bridge 0.49.0). Three-state per field: the key's
   * PRESENCE is the discriminator — absent = keep, `null` = revert to
   * default, value = set. `?sessionId=` is required by the route.
   */
  async amendRunSettings(
    instanceId: string,
    runId: string,
    sessionId: string,
    body: { subagentModel?: string | null; maxConcurrency?: number | null },
  ): Promise<AmendRunSettingsResponse> {
    const q = new URLSearchParams({ sessionId });
    const res = await this.fetch(
      `${this.instSettings(instanceId)}/workflow-runs/${encodeURIComponent(runId)}/settings?${q}`,
      "POST",
      body,
    );
    return (await res.json()) as AmendRunSettingsResponse;
  }

  /** Journal run history — cross-restart, not session-bound. */
  async workflowRunsHistory(
    instanceId: string,
    opts: { scope?: WorkflowScope; name?: string; limit?: number } = {},
  ): Promise<WorkflowRunsHistoryResponse> {
    const q = new URLSearchParams();
    if (opts.scope) q.set("scope", opts.scope);
    if (opts.name) q.set("name", opts.name);
    if (opts.limit !== undefined) q.set("limit", String(opts.limit));
    const qs = q.size > 0 ? `?${q.toString()}` : "";
    const res = await this.fetch(
      `${this.instSettings(instanceId)}/workflows/runs${qs}`,
    );
    return (await res.json()) as WorkflowRunsHistoryResponse;
  }

  /**
   * Hide one settled journal run from the bridge's run lists (bridge 0.58.0
   * bridge-side hide list — the journal itself has no delete). 409 while the
   * run is still flying.
   */
  async dismissWorkflowRun(
    instanceId: string,
    runId: string,
  ): Promise<{ ok: boolean }> {
    const res = await this.fetch(
      `${this.instSettings(instanceId)}/workflow-runs/${encodeURIComponent(runId)}/dismiss`,
      "POST",
    );
    return (await res.json()) as { ok: boolean };
  }

  /** Batch variant: still-running ids are skipped, the settled ones hidden. */
  async dismissWorkflowRuns(
    instanceId: string,
    runIds: string[],
  ): Promise<{ ok: boolean; dismissed: number; skippedActive: number }> {
    const res = await this.fetch(
      `${this.instSettings(instanceId)}/workflow-runs/dismiss-batch`,
      "POST",
      { runIds },
    );
    return (await res.json()) as {
      ok: boolean;
      dismissed: number;
      skippedActive: number;
    };
  }

  /** The desktop's prefilled "create via conversation" prompt. */
  async workflowCreatePrompt(
    instanceId: string,
    scope?: WorkflowScope,
  ): Promise<WorkflowCreatePromptResponse> {
    const q = scope ? `?scope=${scope}` : "";
    const res = await this.fetch(
      `${this.instSettings(instanceId)}/workflow-create-prompt${q}`,
    );
    return (await res.json()) as WorkflowCreatePromptResponse;
  }

  /** A session's runs, incl. the `resumable` verdict per run. */
  async conversationRuns(
    instanceId: string,
    sessionId: string,
    limit?: number,
  ): Promise<ConversationRunsResponse> {
    const q = new URLSearchParams({ sessionId });
    if (limit !== undefined) q.set("limit", String(limit));
    const res = await this.fetch(
      `${this.instSettings(instanceId)}/workflow-runs?${q}`,
    );
    return (await res.json()) as ConversationRunsResponse;
  }

  async runEvents(
    instanceId: string,
    sessionId: string,
    runId: string,
    afterSequence?: number,
  ): Promise<WorkflowRunEventsResponse> {
    const q = new URLSearchParams({ sessionId });
    if (afterSequence !== undefined)
      q.set("afterSequence", String(afterSequence));
    const res = await this.fetch(
      `${this.instSettings(instanceId)}/workflow-runs/${encodeURIComponent(runId)}/events?${q}`,
    );
    return (await res.json()) as WorkflowRunEventsResponse;
  }

  async runArtifacts(
    instanceId: string,
    sessionId: string,
    runId: string,
  ): Promise<{ ok: boolean; artifacts?: unknown[] }> {
    const q = new URLSearchParams({ sessionId });
    const res = await this.fetch(
      `${this.instSettings(instanceId)}/workflow-runs/${encodeURIComponent(runId)}/artifacts?${q}`,
    );
    return (await res.json()) as { ok: boolean; artifacts?: unknown[] };
  }

  async runArtifactData(
    instanceId: string,
    sessionId: string,
    runId: string,
    artifactId: string,
    afterSequence?: number,
    limit?: number,
  ): Promise<WorkflowArtifactItemsResponse> {
    const q = new URLSearchParams({ sessionId });
    if (afterSequence !== undefined)
      q.set("afterSequence", String(afterSequence));
    if (limit !== undefined) q.set("limit", String(limit));
    const res = await this.fetch(
      `${this.instSettings(instanceId)}/workflow-runs/${encodeURIComponent(runId)}/artifacts/${encodeURIComponent(artifactId)}/data?${q}`,
    );
    return (await res.json()) as WorkflowArtifactItemsResponse;
  }

  async runArtifactRead(
    instanceId: string,
    sessionId: string,
    runId: string,
    artifactId: string,
    version: number,
    offset: number,
    limit?: number,
  ): Promise<WorkflowArtifactReadResponse> {
    const q = new URLSearchParams({
      sessionId,
      version: String(version),
      offset: String(offset),
    });
    if (limit !== undefined) q.set("limit", String(limit));
    const res = await this.fetch(
      `${this.instSettings(instanceId)}/workflow-runs/${encodeURIComponent(runId)}/artifacts/${encodeURIComponent(artifactId)}/read?${q}`,
    );
    return (await res.json()) as WorkflowArtifactReadResponse;
  }

  async runWorkspace(
    instanceId: string,
    sessionId: string,
    runId: string,
  ): Promise<WorkflowWorkspaceResponse> {
    const q = new URLSearchParams({ sessionId });
    const res = await this.fetch(
      `${this.instSettings(instanceId)}/workflow-runs/${encodeURIComponent(runId)}/workspace?${q}`,
    );
    return (await res.json()) as WorkflowWorkspaceResponse;
  }

  async runNodeResult(
    instanceId: string,
    sessionId: string,
    runId: string,
    siteId: string,
    ordinal: number,
  ): Promise<WorkflowNodeResultResponse> {
    const q = new URLSearchParams({ sessionId });
    const res = await this.fetch(
      `${this.instSettings(instanceId)}/workflow-runs/${encodeURIComponent(runId)}/nodes/${encodeURIComponent(siteId)}/${ordinal}?${q}`,
    );
    return (await res.json()) as WorkflowNodeResultResponse;
  }

  /**
   * Restart one bridge's backend so its `needs-restart` writes take effect.
   *
   * Per-instance on purpose: the hub's own settings mount has no backend, so
   * the restart must name the bridge it means to disturb. `cancelledTurns` in
   * the answer is how many conversations were interrupted — the caller warns
   * with it BEFORE the user commits. `closed` is whether the old subprocess
   * actually went away: false means the writes this restart existed to apply
   * did not take effect, so the caller must keep the affordance alive.
   */
  async restartBackend(
    instanceId: string,
  ): Promise<{ cancelledTurns: number; closed: boolean }> {
    // Stop + respawn of the sandboxed backend: seconds-scale, not instant.
    const res = await this.fetch(
      `/api/instances/${instanceId}/backend/restart`,
      "POST",
      undefined,
      { timeoutMs: 30_000 },
    );
    return (await res.json()) as { cancelledTurns: number; closed: boolean };
  }
}
