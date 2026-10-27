// ZCode configuration slice (ADR-0009), extracted from appStore: the
// config-screen state (settings sections, reset cards, app update, workflow
// gate/scope reads) and its actions. Composed into the single useAppStore at
// the assembly site — this is a slice, not a separate store.
import type { StoreApi } from "zustand";
import { HubApiError, HubClient } from "../lib/hub";
import { saveUpdateChannel, type UpdateChannel } from "../lib/storage";
import type {
  AgentUpsert,
  AppUpdateState,
  Effect,
  HookEntryPatch,
  McpServerUpsert,
  ModelUpsert,
  ResetCardStatus,
  SettingsAll,
  SettingsPlatformUsage,
  SettingsUsage,
  SystemStatsResponse,
  WorkflowGateBlock,
  WorkflowGateModeSetting,
  WorkflowListResponse,
  WorkflowScope,
} from "../lib/types";
import type { AppState } from "./appStore";

/** The configuration sections the app can open. */
export type ConfigSection =
  | "models"
  | "skills"
  | "mcp"
  | "hooks"
  | "agents"
  | "quota"
  | "usage"
  | "usagePlatform"
  | "backups"
  | "appUpdate"
  | "workflows"
  | "system";

export interface ConfigSliceState {
  // ---- ZCode configuration (ADR-0009) ----
  //
  // Machine-level state (one hub, one configuration), independent of any
  // session. `configSupported` is false once a settings route has answered
  // 404 — an older hub — so the screens say so instead of erroring.
  configOpen: boolean;
  // Which configuration section is open; null = the entry list.
  configSection: ConfigSection | null;
  // True when the screen OPENED straight at a section (session panel's
  // workflow manage, a run deep link) — the entry list was never shown, so a
  // section page's back must CLOSE the screen (return to the caller) instead
  // of revealing a list the user never navigated through.
  configDeepEntry: boolean;
  configSupported: boolean | null;
  configLoading: boolean;
  configError: string | null;
  // The /settings/all snapshot: one request for a screen's first paint.
  configAll: SettingsAll | null;
  // Section payloads fetched on their own (models/skills/mcp/hooks/agents
  // carry shapes the snapshot compresses; usage and backups likewise).
  configModels: unknown;
  configSkills: unknown;
  configMcp: unknown;
  configHooks: unknown;
  configAgents: unknown;
  configUsage: SettingsUsage | null;
  // The range the LAST usage read asked for. Stored so a response that arrives
  // out of order (two taps, two in-flight reads) can be recognised as stale
  // and dropped rather than shown under the wrong range button.
  configUsageRange: string;
  // Platform usage (the account-level monitor data). Same read-on-open pattern
  // and same stale-range guard as configUsage above; `false` in
  // configPlatformUsageSupported means THIS hub predates the route (404), which
  // degrades the platform tab alone instead of the whole settings API.
  configPlatformUsage: SettingsPlatformUsage | null;
  configPlatformUsageRange: string;
  configPlatformUsageSupported: boolean;
  configBackups: unknown;
  configAppUpdate: unknown;
  // Install progress for the app update, polled by the update screen.
  appUpdateInstall: AppUpdateState | null;
  // Reset cards (ADR-0009): the inventory plus which provider it belongs to.
  // `resetNonce` is what a spend must send back — it is tied to this read, and
  // so is `resetIdempotencyKey`: one key per GESTURE, minted alongside the
  // nonce, so every retry of that spend sends the same value.
  resetCards: ResetCardStatus["resetCards"] | null;
  resetProviderId: string | null;
  resetNonce: string | null;
  resetIdempotencyKey: string | null;
  // The provider ids the hub says could own cards, and whether the credential
  // store could be decrypted at all. Kept apart from `resetProviderId` (the
  // user's choice) because the list is a fact and the choice is not.
  resetEligible: string[];
  resetCredentials: boolean;
  resetBusy: boolean;
  // When a denied reset opportunity said to try again (epoch ms). Drives the
  // countdown on the plan-quota screen; null when there is nothing to count.
  resetNextTryAt: number | null;
  // True once this bridge has recorded a needs-restart write since its last
  // backend restart. Drives the restart affordance on the config screens.
  pendingRestart: boolean;
  // Which ZCode desktop release stream the update screen follows. Persisted:
  // a preview user who is silently checked against stable sees a version the
  // install then refuses.
  updateChannel: UpdateChannel;
  // ---- dynamic workflows (bridge 0.48.0, server ADR-0029) ----
  //
  // The saved-workflow list for the scope on screen. Every workflow route is
  // per-instance, so the reads/writes all name the connected bridge.
  configWorkflows: WorkflowListResponse | null;
  // The scope the LAST workflows read asked for — the stale-response guard's
  // comparison key (two scope taps race exactly like the usage ranges).
  configWorkflowScope: WorkflowScope;
  // The workflow gate verdict, read from the PER-INSTANCE settings snapshot:
  // the machine-level `/settings/all` the entry list loads has no backend and
  // reports no `enabled` field at all, so this is the only honest source for
  // "show the workflows entry". Null = not probed (reads as hidden).
  configWorkflowGate: SettingsAll["workflow"] | null;
  // ---- system status (bridge 0.62.0) ----
  //
  // The machine-level /api/system-stats snapshot the status page polls. Every
  // read replaces it whole (the payload is one point-in-time collect); the
  // page renders the latest answer as-is. `false` in
  // configSystemStatsSupported means THIS hub predates the route (404), which
  // degrades the page alone — configSupported belongs to /settings/all and
  // must not retire every section over one missing endpoint.
  configSystemStats: SystemStatsResponse | null;
  configSystemStatsError: string | null;
  configSystemStatsSupported: boolean;
  configSystemStatsLoading: boolean;
}

export interface ConfigSliceActions {
  setUpdateChannel: (channel: UpdateChannel) => void;
  // ---- ZCode configuration (ADR-0009) ----
  // Opens/closes the configuration screen. `section` null = the entry list.
  openConfig: (section?: ConfigSection | null) => void;
  closeConfig: () => void;
  // A section page's back: to the entry list, or straight out when the screen
  // was deep-opened at the section (configDeepEntry).
  backFromConfigSection: () => void;
  // Loads the /settings/all snapshot; a 404 marks the hub as too old.
  loadConfigAll: () => Promise<void>;
  // Loads one section's own endpoint. Called when its screen mounts; `arg`
  // carries a section-specific option (the usage range — "7d" | "30d" | "all"
  // for the local read, "today" | "7d" | "30d" for platform usage) so a screen
  // can refetch without the store having to know its controls.
  loadConfigSection: (
    section: ConfigSection,
    arg?: { range?: "7d" | "30d" | "all" | "today" },
  ) => Promise<void>;
  // Applies a write and reports its effect class. Destructive operations
  // (delete skill / mcp / agent, restore backup) confirm at the call site.
  applyConfigWrite: (
    label: string,
    write: () => Promise<Effect | undefined>,
    reload?: ConfigSection[],
  ) => Promise<boolean>;
  // ---- configuration writes ----
  //
  // Thin wrappers over the hub routes so a screen never builds a HubClient or
  // a URL itself. Each returns the write's effect class; failures throw, and
  // `applyConfigWrite` turns them into a toast.
  setProviderEnabled: (
    providerId: string,
    enabled: boolean,
  ) => Promise<Effect | undefined>;
  setSkillEnabled: (
    path: string,
    enable: boolean,
  ) => Promise<Effect | undefined>;
  copySkillToUser: (path: string) => Promise<Effect | undefined>;
  // POST /settings/models is an upsert: one route adds a model and edits its
  // rule (context window, reasoning levels), so one wrapper serves both forms.
  upsertModel: (body: ModelUpsert) => Promise<Effect | undefined>;
  setMcpEnabled: (name: string, enable: boolean) => Promise<Effect | undefined>;
  upsertMcp: (
    name: string,
    body: McpServerUpsert,
  ) => Promise<Effect | undefined>;
  setHooksEnabled: (enabled: boolean) => Promise<Effect | undefined>;
  // Edits ONE existing hook entry in place; there is deliberately no add or
  // remove through here (an insert shifts every later index and races a
  // concurrent edit of the same event).
  updateHookEntry: (
    eventName: string,
    matcherIndex: number,
    hookIndex: number,
    body: HookEntryPatch,
  ) => Promise<Effect | undefined>;
  setAgentEnabled: (
    name: string,
    enable: boolean,
  ) => Promise<Effect | undefined>;
  // A PUT on a missing personal agent creates it, so this one route serves
  // editing, creating, and the built-ins' model override.
  upsertAgent: (name: string, body: AgentUpsert) => Promise<Effect | undefined>;
  restoreBackup: (file: string, path: string) => Promise<Effect | undefined>;
  // Reset cards: read the inventory, spend a card. Both actions take the
  // provider (and the spend its nonce) explicitly: reading the store at action
  // entry would let a provider switch mid-gesture spend the wrong card.
  loadResetCards: (providerId: string) => Promise<void>;
  spendResetCard: (input: {
    providerId: string;
    nonce: string;
    resetType: "FIVE_HOUR" | "WEEK";
  }) => Promise<boolean>;
  markResetHistoryRead: () => Promise<void>;
  // App update: start an install and poll its progress.
  installAppUpdate: (input: {
    version: string;
    url: string;
    channel?: string;
  }) => Promise<void>;
  pollAppUpdate: () => Promise<void>;
  // ---- dynamic workflows ----
  // Reads the saved-workflow list for one scope (per-instance routes).
  loadWorkflows: (scope: WorkflowScope) => Promise<void>;
  // Probes the per-instance settings snapshot for the workflow gate verdict
  // (the entry list's visibility source). A 404 marks the hub too old; any
  // other failure just leaves the gate unread — the entry stays hidden.
  loadWorkflowGate: () => Promise<void>;
  // Writes the machine-level gate override (bridge ≥0.53 PUT
  // /settings/workflow-gate) and stores the returned verdict, so every
  // gate-driven surface (config entry, session-panel launcher) flips live.
  // Resolves null when the write was refused (workflowAction posture).
  setWorkflowGate: (
    mode: WorkflowGateModeSetting,
  ) => Promise<WorkflowGateBlock | null>;
  // Restarts the backend of the instance that owns the configuration, so
  // needs-restart writes take effect. Returns the interrupted-turn count.
  restartConfigBackend: () => Promise<number>;
  // Reads the machine-level system-status snapshot (/api/system-stats). The
  // status page polls this; first paint is seeded through loadConfigSection.
  loadSystemStats: () => Promise<void>;
}

export type ConfigSlice = ConfigSliceState & ConfigSliceActions;

export const configSliceState: ConfigSliceState = {
  configOpen: false,
  configSection: null,
  configDeepEntry: false,
  configSupported: null,
  configLoading: false,
  configError: null,
  configAll: null,
  configModels: null,
  configSkills: null,
  configMcp: null,
  configHooks: null,
  configAgents: null,
  configUsage: null,
  configUsageRange: "7d",
  configPlatformUsage: null,
  configPlatformUsageRange: "7d",
  configPlatformUsageSupported: true,
  configBackups: null,
  configAppUpdate: null,
  configWorkflows: null,
  configWorkflowScope: "project",
  configWorkflowGate: null,
  configSystemStats: null,
  configSystemStatsError: null,
  configSystemStatsSupported: true,
  configSystemStatsLoading: false,
  appUpdateInstall: null,
  resetCards: null,
  resetProviderId: null,
  resetNonce: null,
  resetIdempotencyKey: null,
  resetEligible: [],
  resetCredentials: true,
  resetBusy: false,
  resetNextTryAt: null,
  pendingRestart: false,
  // Seeded from storage by init(), like every other persisted preference —
  // the initial state must stay free of storage calls so the node-environment
  // tests (which have no localStorage) can import the store.
  updateChannel: "stable",
};

/**
 * Idempotency key for one reset-card GESTURE.
 *
 * A spent card is gone, so the backend keys a spend on this value: a retry
 * (dropped response, double tap) answers the same outcome instead of burning a
 * second card. Generate it ONCE per user gesture and reuse it on every retry —
 * a fresh key per attempt is exactly what defeats the protection.
 *
 * The backend contract fixes the shape at a UUID and refuses anything else as
 * coding_plan_reset_api_error:3001. Minted from getRandomValues (unlike
 * randomUUID it also exists in plain-http browser contexts); Math.random is
 * the last-ditch fallback.
 */
function newIdempotencyKey(): string {
  const bytes = new Uint8Array(16);
  if (typeof crypto !== "undefined" && typeof crypto.getRandomValues === "function") {
    crypto.getRandomValues(bytes);
  } else {
    for (let i = 0; i < bytes.length; i += 1) {
      bytes[i] = Math.floor(Math.random() * 256);
    }
  }
  // Version 4, RFC 4122 variant.
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

/**
 * The release channel an app-update read must use.
 *
 * Kept in a module-local mirror rather than read from localStorage on every
 * call: `loadUpdateChannel` touches storage, and the check runs from a React
 * effect on a screen that also polls. `init()` seeds it from storage and
 * `setUpdateChannel` is the only other writer.
 */
let updateChannel: UpdateChannel = "stable";

function readAppUpdateChannel(): UpdateChannel {
  return updateChannel;
}

/** Seeds the channel mirror from storage (init's one write outside the slice). */
export function seedUpdateChannel(channel: UpdateChannel): void {
  updateChannel = channel;
}

export function createConfigSlice(
  set: StoreApi<AppState>["setState"],
  get: StoreApi<AppState>["getState"],
  hub: () => HubClient | null,
): ConfigSliceActions {
  return {
    setUpdateChannel: (channel) => {
      updateChannel = channel;
      saveUpdateChannel(channel);
      set({ updateChannel: channel });
      // The stored check is for the channel it was read with; switching without
      // a refetch would leave the screen showing the other stream's verdict.
      const section = get().configSection;
      if (section === "appUpdate") void get().loadConfigSection("appUpdate");
    },

    openConfig: (section = null) =>
      set((s) => ({
        configOpen: true,
        configSection: section,
        // Deep entry only when the screen was CLOSED before this call —
        // navigating list → section keeps the list as the back target.
        configDeepEntry: section !== null && !s.configOpen,
      })),

    closeConfig: () =>
      set({ configOpen: false, configSection: null, configDeepEntry: false }),

    backFromConfigSection: () => {
      if (get().configDeepEntry) get().closeConfig();
      else get().openConfig(null);
    },

    loadConfigAll: async () => {
      const client = hub();
      if (!client) return;
      set({ configLoading: true, configError: null });
      try {
        const all = await client.settingsAll();
        set({ configAll: all, configLoading: false, configSupported: true });
        // The snapshot carries usage and reset-card eligibility, so a section
        // screen that opens straight after the entry list needs no second
        // request for those two.
        if (all.usage) {
          set({ configUsage: all.usage, configUsageRange: all.usage.range });
        }
        if (all.resetCards) {
          const providers = all.resetCards.providers ?? [];
          // The hub reports the coding-plan provider ids that COULD own cards,
          // not the ones this account actually has — so there is usually more
          // than one and a picker is the norm. Remember the eligibility
          // verdict alongside: `credentials: false` means the store could not
          // be decrypted, which is a different (and non-fatal) state from
          // having no plan at all.
          set({
            resetCards: null,
            resetProviderId: null,
            resetEligible: providers,
            resetCredentials: all.resetCards.credentials !== false,
          });
        }
      } catch (e) {
        // 404 means this hub predates the settings API (bridge < 0.47.0). That
        // is a capability gap, not a failure — the screens explain it.
        if (e instanceof HubApiError && e.status === 404) {
          set({ configSupported: false, configLoading: false });
          return;
        }
        set({
          configError: e instanceof Error ? e.message : String(e),
          configLoading: false,
        });
      }
    },

    loadConfigSection: async (section, arg) => {
      const client = hub();
      if (!client) return;
      set({ configLoading: true, configError: null });
      try {
        switch (section) {
          case "models":
            set({ configModels: await client.settingsModels() });
            break;
          case "skills":
            set({ configSkills: await client.settingsSkills() });
            break;
          case "mcp":
            set({ configMcp: await client.settingsMcp() });
            break;
          case "hooks":
            set({ configHooks: await client.settingsHooks() });
            break;
          case "agents":
            set({ configAgents: await client.settingsAgents() });
            break;
          case "usage": {
            // The route wraps the payload: `{ok, usage}`. Storing the whole
            // response would make every `usage.available` read undefined and
            // the screen would show its "no data" empty state forever.
            const requestedRange = arg?.range ?? "7d";
            // Record the ask BEFORE awaiting: the guard below compares against
            // the newest request, and a read that lands first must not be able
            // to overwrite the record of one still in flight.
            set({ configUsageRange: requestedRange });
            const payload = (await client.settingsUsage(requestedRange)) as {
              usage?: SettingsUsage;
            };
            // Stale-response guard. The range buttons fire a fresh read each
            // tap, and nothing serializes them: tap "All" then "7d" on a slow
            // link and the All response can land AFTER the 7d one, leaving the
            // 7d button highlighting numbers computed for the whole history.
            // The snapshot carries the range it was computed for, so drop any
            // answer that no longer matches what the user asked for last.
            const answeredRange = payload.usage?.range ?? requestedRange;
            if (answeredRange !== get().configUsageRange) return;
            set({ configUsage: payload.usage ?? null });
            break;
          }
          case "usagePlatform": {
            // Same envelope pattern (`{ok, platformUsage}`) and the same
            // stale-range guard as the local usage read above.
            const requestedRange = arg?.range ?? "7d";
            set({ configPlatformUsageRange: requestedRange });
            try {
              const payload = (await client.settingsUsagePlatform(
                requestedRange,
              )) as {
                platformUsage?: SettingsPlatformUsage;
              };
              const answeredRange =
                payload.platformUsage?.range ?? requestedRange;
              if (answeredRange !== get().configPlatformUsageRange) return;
              set({ configPlatformUsage: payload.platformUsage ?? null });
            } catch (e) {
              // A 404 here means the bridge predates the route — unlike the
              // shared catch below, it must NOT flip configSupported (that
              // verdict belongs to /settings/all and gates every section); the
              // platform tab shows its own upgrade note instead.
              if (e instanceof HubApiError && e.status === 404) {
                set({ configPlatformUsageSupported: false });
                break;
              }
              throw e;
            }
            break;
          }
          case "backups":
            set({ configBackups: await client.settingsBackups() });
            break;
          case "appUpdate":
            set({
              configAppUpdate: await client.settingsAppUpdate(updateChannel),
            });
            break;
          case "workflows":
            // The grouped cross-instance list is driven by the workflows
            // screen itself (mount + running poll); this mount-effect path
            // just seeds its first paint.
            await get().loadWorkflowHub();
            break;
          case "system":
            // Same pattern as workflows: the status page polls on its own;
            // this mount-effect path seeds the first paint.
            await get().loadSystemStats();
            break;
          case "quota":
            // The plan-quota screen reuses the hub-level quota the side panels
            // already show — one source of truth, so the numbers outside and
            // inside can never disagree.
            await get().refreshUsageStats();
            break;
        }
        set({ configLoading: false });
      } catch (e) {
        if (e instanceof HubApiError && e.status === 404) {
          set({ configSupported: false, configLoading: false });
          return;
        }
        set({
          configError: e instanceof Error ? e.message : String(e),
          configLoading: false,
        });
      }
    },

    applyConfigWrite: async (label, write, reload) => {
      try {
        const effect = await write();
        // The effect class is the whole point: an immediate write is done, a
        // needs-restart one has not landed yet and the user must know which.
        // `notice` (an i18n key the banner translates) for the outcome, `notify`
        // (plain English, like every other failure message here) for errors.
        //
        // An undefined effect means the write wrapper found no client — the
        // connection dropped while the screen was open, and NOTHING was sent.
        // Reporting that as success would show a toggle that moved on a machine
        // that never heard about it.
        if (effect === undefined) {
          get().notify(`${label} failed: not connected`);
          return false;
        }
        if (effect === "needs-restart") {
          set({ pendingRestart: true, notice: "notice.configNeedsRestart" });
        }
        for (const section of reload ?? []) {
          await get().loadConfigSection(section);
        }
        return true;
      } catch (e) {
        get().notify(
          `${label} failed: ${e instanceof Error ? e.message : String(e)}`,
        );
        return false;
      }
    },

    setProviderEnabled: async (providerId, enabled) => {
      const client = hub();
      if (!client) return undefined;
      const res = await client.updateProvider(providerId, { enabled });
      return res.effect;
    },

    setSkillEnabled: async (path, enable) => {
      const client = hub();
      if (!client) return undefined;
      const res = await client.setSkillEnabled({ path, enable });
      return res.effect;
    },

    copySkillToUser: async (path) => {
      const client = hub();
      if (!client) return undefined;
      const res = await client.copySkillToUser({ path });
      return res.effect;
    },

    upsertModel: async (body) => {
      const client = hub();
      if (!client) return undefined;
      const res = await client.addModel(body);
      return res.effect;
    },

    setMcpEnabled: async (name, enable) => {
      const client = hub();
      if (!client) return undefined;
      const res = await client.setMcpEnabled(name, enable);
      return res.effect;
    },

    upsertMcp: async (name, body) => {
      const client = hub();
      if (!client) return undefined;
      const res = await client.upsertMcpServer(name, body);
      return res.effect;
    },

    setHooksEnabled: async (enabled) => {
      const client = hub();
      if (!client) return undefined;
      const res = await client.setHooksEnabled(enabled);
      return res.effect;
    },

    updateHookEntry: async (eventName, matcherIndex, hookIndex, body) => {
      const client = hub();
      if (!client) return undefined;
      const res = await client.updateHook(
        eventName,
        matcherIndex,
        hookIndex,
        body,
      );
      return res.effect;
    },

    setAgentEnabled: async (name, enable) => {
      const client = hub();
      if (!client) return undefined;
      const res = await client.setAgentEnabled(name, enable);
      return res.effect;
    },

    upsertAgent: async (name, body) => {
      const client = hub();
      if (!client) return undefined;
      const res = await client.upsertAgent(name, body);
      return res.effect;
    },

    restoreBackup: async (file, path) => {
      const client = hub();
      if (!client) return undefined;
      const res = await client.restoreBackup({ file, path });
      // The effect follows the file: restoring cliConfig needs a restart,
      // restoring providerConfig is immediate. Report what the bridge said
      // rather than assuming one or the other.
      return res.effect;
    },

    loadResetCards: async (providerId) => {
      const client = hub();
      if (!client) return;
      // Drop the previous provider's inventory NOW, not when the new read
      // lands: while it is in flight the screen would still show provider A's
      // cards, and if this read FAILS it would keep showing them — with a
      // nonce the next spend then sends for a provider it is not looking at.
      set({
        resetBusy: true,
        resetProviderId: providerId,
        resetCards: null,
        resetNonce: null,
        resetIdempotencyKey: null,
      });
      try {
        const status = await client.resetCardStatus(providerId);
        // The provider picker can fire two reads in quick succession. Without
        // this guard the SLOWER one lands last and its nonce is stored
        // alongside the FASTER one's providerId — a spend then pairs a nonce
        // minted for a different provider, which the route always rejects with
        // 409, and the user sees a broken button with no cause to guess.
        if (get().resetProviderId !== providerId) return;
        set({
          resetCards: status.resetCards,
          resetNonce: status.resetCards.nonce,
          // Minted HERE, with the nonce it belongs to, so a spend's every retry
          // sends the same key. Generating it inside the spend instead gave a
          // fresh key per attempt — which is precisely what defeats the
          // backend's dedupe and can burn a second card after a timeout.
          resetIdempotencyKey: newIdempotencyKey(),
          resetBusy: false,
        });
      } catch (e) {
        // Same guard: an error from a read the user has already moved on from
        // must not surface as a toast about a provider they are not looking at.
        if (get().resetProviderId !== providerId) return;
        set({ resetBusy: false });
        get().notify(
          `reset card status failed: ${e instanceof Error ? e.message : String(e)}`,
        );
      }
    },

    spendResetCard: async ({ providerId, nonce, resetType }) => {
      const client = hub();
      if (!client || !providerId || !nonce) return false;
      // Same guard as the opportunity: if the user has switched provider since
      // the gesture began, this nonce belongs to another provider's inventory
      // and spending it now would burn the wrong card.
      if (get().resetProviderId && get().resetProviderId !== providerId)
        return false;
      set({ resetBusy: true });
      // The key minted with this nonce, NOT a fresh one. The backend keys the
      // spend on it, so a retry (the 502/504 the bridge answers when a spend
      // may already have been carried out) sends the same value and gets the
      // same outcome. Generating it here instead gave a new key per attempt,
      // which defeats the dedupe and can burn a second card.
      const idempotencyKey = get().resetIdempotencyKey ?? newIdempotencyKey();
      try {
        const res = await client.spendResetCard({
          providerId,
          resetType,
          nonce,
          idempotencyKey,
        });
        set({ resetBusy: false });
        const used = (res as { ok: boolean; used?: boolean }).used !== false;
        set({
          notice: used
            ? "notice.configResetSpent"
            : "notice.configResetSpendFailed",
        });
        // Refresh both surfaces: the card list (one fewer card) and the quota
        // (the window it cleared). A spend that shows no result reads as a
        // failure even when it worked. The refresh also issues the next nonce.
        await get().loadResetCards(providerId);
        await get().refreshUsageStats();
        return used;
      } catch (e) {
        set({ resetBusy: false });
        // 409 = the nonce is no longer the one the server issued (a newer status
        // read replaced it, or the user switched provider). Re-read the status to
        // mint a fresh one and say so: without this the button looks broken and
        // the only escape is finding the refresh control by trial and error.
        //
        // The route burns the nonce only AFTER the spend settles, so a request
        // that reached the server and failed on the way back is NOT a 409 — the
        // same nonce and key still work, which is the retry path this protects.
        if (e instanceof HubApiError && e.status === 409) {
          await get().loadResetCards(providerId);
          set({ notice: "notice.configResetStale" });
          return false;
        }
        get().notify(
          `reset card spend failed: ${e instanceof Error ? e.message : String(e)}`,
        );
        return false;
      }
    },

    markResetHistoryRead: async () => {
      const client = hub();
      const providerId = get().resetProviderId;
      if (!client || !providerId) return;
      try {
        await client.markResetHistoryRead(providerId);
        await get().loadResetCards(providerId);
      } catch {
        // Clearing an unread badge is cosmetic; a failure is not worth a toast.
      }
    },

    installAppUpdate: async (input) => {
      const client = hub();
      if (!client) return;
      try {
        // The channel must travel with the install: the route re-reads the
        // release manifest and refuses any version that is not the LATEST on
        // the channel it was told. A preview user whose install defaulted to
        // stable would be refused with "latest stable version is X, not Y" for
        // a version the check itself offered.
        await client.installAppUpdate({
          ...input,
          channel: input.channel ?? readAppUpdateChannel(),
        });
        // The download runs in the background on the bridge; progress arrives
        // by polling, which the update screen drives.
        await get().pollAppUpdate();
      } catch (e) {
        get().notify(
          `app update install failed: ${e instanceof Error ? e.message : String(e)}`,
        );
      }
    },

    pollAppUpdate: async () => {
      const client = hub();
      if (!client) return;
      try {
        // Same channel as the check: a poll that answered for the other stream
        // would overwrite the install state with an unrelated manifest's.
        const res = (await client.settingsAppUpdate(updateChannel)) as {
          appUpdate?: { install?: AppUpdateState };
        };
        if (res.appUpdate?.install)
          set({ appUpdateInstall: res.appUpdate.install });
      } catch {
        // A poll failure is not worth a toast; the next one may succeed.
      }
    },

    loadWorkflowGate: async () => {
      const client = hub();
      const instanceId = get().instanceId;
      if (!client || !instanceId) return;
      try {
        const all = await client.instanceSettingsAll(instanceId);
        set({ configWorkflowGate: all.workflow ?? null });
      } catch {
        // Silent, and that includes 404: a per-instance 404 is usually the
        // hub re-learning its instances after a restart (the bridge
        // re-registers within seconds), not proof of an old hub — writing
        // `configSupported:false` here would retire EVERY config section on
        // one transient answer. A failed probe hides the entry, which is the
        // same verdict the gate itself gives on any doubt (fail-closed).
      }
    },

    setWorkflowGate: async (mode) => {
      const res = await get().workflowAction("set workflow gate", (c, iid) =>
        c.workflowGateSet(iid, mode),
      );
      if (!res) return null;
      set({ configWorkflowGate: res.gate });
      return res.gate;
    },

    loadWorkflows: async (scope) => {
      const client = hub();
      if (!client) return;
      set({
        configLoading: true,
        configError: null,
        configWorkflowScope: scope,
      });
      try {
        const instanceId = get().instanceId;
        if (!instanceId) throw new Error("no instance connected");
        const list = await client.workflowsList(instanceId, scope);
        // Stale-response guard: the scope buttons fire a fresh read per tap
        // and nothing serializes them — an answer for a scope the user has
        // already moved past must not repaint the list under the new one.
        if (get().configWorkflowScope !== scope) return;
        set({ configWorkflows: list, configLoading: false });
      } catch (e) {
        if (get().configWorkflowScope !== scope) return;
        // No 404 special-case: the machine-level loaders own the old-hub
        // verdict. A 404 here is per-instance (usually the hub re-learning
        // its instances), so it lands as a retriable page error instead of
        // the permanent "unsupported" screen.
        set({
          configError: e instanceof Error ? e.message : String(e),
          configLoading: false,
        });
      }
    },

    restartConfigBackend: async () => {
      const client = hub();
      const s = get();
      // The restart is per-instance: the hub's own settings mount has no
      // backend, so the bridge to disturb must be named. With no session open
      // there is no instance to address, and the restart is unavailable.
      if (!client || !s.instanceId) return -1;
      try {
        const res = await client.restartBackend(s.instanceId);
        // `closed: false` means the old subprocess survived — the writes this
        // restart existed to apply did NOT take effect. Clearing the flag here
        // would hide the affordance the user needs, so it stays armed and the
        // notice says the restart failed rather than succeeded.
        if (res.closed) {
          set({ pendingRestart: false, notice: "notice.configRestarted" });
        } else {
          set({ notice: "notice.configRestartFailed" });
        }
        return res.cancelledTurns;
      } catch (e) {
        get().notify(
          `backend restart failed: ${e instanceof Error ? e.message : String(e)}`,
        );
        return -1;
      }
    },

    loadSystemStats: async () => {
      const client = hub();
      if (!client) return;
      set({ configSystemStatsLoading: true });
      try {
        const stats = await client.systemStats();
        set({
          configSystemStats: stats,
          configSystemStatsError: null,
          configSystemStatsLoading: false,
        });
      } catch (e) {
        // 404 = the hub PREDATES the route (bridge < 0.62.0): a version gap,
        // not a transient failure — retire this page alone (configSupported
        // belongs to /settings/all and must not flip here). Anything else is
        // a blip: keep the last snapshot, no error paint over working
        // content; the FIRST load of a broken connection still needs a story.
        if (e instanceof HubApiError && e.status === 404) {
          set({ configSystemStatsSupported: false, configSystemStatsLoading: false });
          return;
        }
        const hadData = get().configSystemStats !== null;
        set({
          ...(hadData ? {} : { configSystemStatsError: e instanceof Error ? e.message : String(e) }),
          configSystemStatsLoading: false,
        });
      }
    },
  };
}
