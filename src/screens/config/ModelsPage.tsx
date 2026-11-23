import { useState } from "react";
import { useTranslation } from "react-i18next";
import { ChevronDown, ChevronRight, ChevronUp, Plus } from "lucide-react";
import { matchProviderId } from "../../lib/modelValue";
import { useAppStore } from "../../store/appStore";
import {
  ConfigBlock,
  ConfigEmpty,
  ConfigPageFrame,
} from "../../components/config/ConfigPage";
import {
  ConfigField,
  ConfigFormSheet,
  ConfigToggle,
  FormDeleteButton,
  configInputClass,
} from "../../components/config/ConfigFormSheet";

// Selectable models (ADR-0009). The bridge builds the list as the union of
// config.json and provider_config.json, so a model added in the desktop app
// shows up here without the app merging anything itself.
//
// The payload is an OBJECT, not an array: `available` is the selectable list,
// `providers` describes each provider's rule, and `modelRules` carries the
// per-model properties (context window, reasoning levels) that `available`
// only names. The context window is therefore looked up from `modelRules`,
// joined on `{providerId}/{modelId}` — the only key both halves share.
//
// Editing goes through POST /settings/models, which is an UPSERT: one route
// adds a model to a provider and rewrites its personal rule (enabled, context
// window, reasoning levels). The write is `immediate` — the backend polls the
// provider table — so no restart note ever comes out of this page.
export interface ModelRef {
  providerId?: string;
  providerName?: string;
  modelId?: string;
}

export interface ModelRule {
  providerId?: string;
  modelId?: string;
  enabled?: boolean;
  contextWindow?: number;
  reasoningLevels?: string[];
}

export interface ModelsPayload {
  ok?: boolean;
  models?: {
    available?: ModelRef[];
    providers?: Array<{
      providerId?: string;
      providerName?: string;
      enabled?: boolean;
      baseUrl?: string;
      apiType?: string;
      hasApiKey?: boolean;
    }>;
    providerOrder?: string[];
    modelRules?: ModelRule[];
  };
}

/** One provider rule row, as the settings payload reports it (the endpoint
 * fields exist only for providers this machine's provider_config.json
 * declares — a bundled or account-managed one carries no baseUrl). */
export interface ProviderRow {
  providerId: string;
  providerName: string;
  enabled: boolean;
  baseUrl?: string;
  apiType?: string;
  hasApiKey: boolean;
}

/**
 * The providers block in its display order (the reorder write's own
 * normalize rule, mirrored on the read side): `providerOrder` names the
 * sequence, ids the block carries but the order does not mention keep their
 * file order after the mentioned ones, and a missing order falls back to the
 * block's own order. Exported so the response-shape test pins it.
 */
export function orderedProviders(
  payload: ModelsPayload | null,
): ProviderRow[] {
  const block = payload?.models;
  const byId = new Map<string, ProviderRow>();
  for (const p of block?.providers ?? []) {
    if (!p.providerId) continue;
    byId.set(p.providerId, {
      providerId: p.providerId,
      providerName: p.providerName ?? p.providerId,
      enabled: p.enabled !== false,
      baseUrl: p.baseUrl,
      apiType: p.apiType,
      hasApiKey: Boolean(p.hasApiKey),
    });
  }
  const order = block?.providerOrder;
  if (!order) return [...byId.values()];
  const out: ProviderRow[] = [];
  const seen = new Set<string>();
  for (const id of order) {
    const row = byId.get(id);
    if (row && !seen.has(id)) {
      seen.add(id);
      out.push(row);
    }
  }
  for (const row of byId.values()) {
    if (!seen.has(row.providerId)) out.push(row);
  }
  return out;
}

/**
 * The context window for one selectable model.
 *
 * Exported so the response-shape test asserts against the REAL decode rather
 * than a copy that can drift — a copied version passing while the page reads a
 * different path is how four screens once shipped dead.
 *
 * The join goes through `matchProviderId`: `available` normalizes coding-plan
 * ids to `builtin:*` server-side while `modelRules` are passed through
 * verbatim (the desktop writes `account:*` there), so an exact-string compare
 * misses the rule and the window silently disappears from the row.
 *
 * A model absent from the rules (a personal rule with no properties) simply
 * has no window rather than being an error.
 */
export function contextWindowFor(
  model: ModelRef,
  rules: ModelRule[] | undefined,
): number | undefined {
  return ruleFor(model, rules ?? [])?.contextWindow;
}

/** The rule row for one selectable model — the prefill source for editing. */
function ruleFor(model: ModelRef, rules: ModelRule[]): ModelRule | undefined {
  const pid = matchProviderId(model.providerId);
  return rules.find(
    (r) => matchProviderId(r.providerId) === pid && r.modelId === model.modelId,
  );
}

/**
 * Whether a typed context window is acceptable. The route requires a POSITIVE
 * INTEGER and throws on anything else, so `1.5` or `0` or `abc` must be
 * rejected here — a toast for a value the user can still fix in the form is a
 * worse answer than a disabled save button.
 */
function contextWindowOk(text: string): boolean {
  const trimmed = text.trim();
  if (trimmed === "") return true;
  const n = Number(trimmed);
  return Number.isInteger(n) && n > 0;
}

/**
 * Providers a new model may belong to: the rules block first, then any
 * provider the selectable list names that the block omits — the picker should
 * offer the ones this machine actually uses.
 *
 * `account:*` providers are left out. The write route refuses them outright
 * (their models come from the coding plan, not this file), so offering one
 * would only produce a 400 the user cannot act on.
 */
export function providerOptions(
  payload: ModelsPayload | null,
): Array<{ id: string; name: string }> {
  const out = new Map<string, string>();
  for (const p of payload?.models?.providers ?? []) {
    if (p.providerId && !isAccountProvider(p.providerId)) {
      out.set(p.providerId, p.providerName ?? p.providerId);
    }
  }
  for (const m of payload?.models?.available ?? []) {
    if (
      m.providerId &&
      !out.has(m.providerId) &&
      !isAccountProvider(m.providerId)
    ) {
      out.set(m.providerId, m.providerName ?? m.providerId);
    }
  }
  return [...out].map(([id, name]) => ({ id, name }));
}

/** A coding-plan provider the desktop manages; its models are not ours to add. */
export function isAccountProvider(providerId: string): boolean {
  return providerId.startsWith("account:");
}

/**
 * The provider id as a MATCH key — lives in lib/modelValue.ts now (the
 * session panel's option matching needs the same fold); re-exported here for
 * the config pages' imports. See there for the dialect rationale.
 */
export { matchProviderId };

/**
 * Every selectable model as a picker option: the value the runtime spells
 * (`providerId/modelId`), the label, and the reasoning levels its rule
 * carries. One list feeds both the built-in override picker and the personal
 * agent's frontmatter model, so a value the runtime would reject never
 * reaches the form.
 *
 * The label follows the bridge's own session-switcher rule (buildModelSelect
 * Options): a builtin (or coding-plan) model reads as its bare model id, a
 * third-party one is qualified with its provider, and a model id two
 * providers share is always qualified. One rule everywhere is the point —
 * the same model must not read three different ways across the app.
 */
export function modelOptions(payload: ModelsPayload | null): Array<{
  providerId: string;
  modelId: string;
  label: string;
  reasoningLevels: string[];
}> {
  const rules = payload?.models?.modelRules ?? [];
  const seen = new Set<string>();
  const byId = new Map<string, number>();
  for (const m of payload?.models?.available ?? []) {
    if (!m.providerId || !m.modelId) continue;
    byId.set(m.modelId, (byId.get(m.modelId) ?? 0) + 1);
  }
  const out: Array<{
    providerId: string;
    modelId: string;
    label: string;
    reasoningLevels: string[];
  }> = [];
  for (const m of payload?.models?.available ?? []) {
    if (!m.providerId || !m.modelId) continue;
    const key = `${m.providerId}/${m.modelId}`;
    if (seen.has(key)) continue;
    seen.add(key);
    // The rule join goes through `matchProviderId` for the same reason as
    // `contextWindowFor`: the two halves of the payload can spell one coding
    // plan provider differently, and the picker must still see its levels.
    const rule = ruleFor(m, rules);
    const builtinLike =
      m.providerId.startsWith("builtin:") || isAccountProvider(m.providerId);
    const qualify = !builtinLike || (byId.get(m.modelId) ?? 0) > 1;
    out.push({
      providerId: m.providerId,
      modelId: m.modelId,
      label: qualify
        ? `${m.providerName ?? m.providerId} › ${m.modelId}`
        : m.modelId,
      reasoningLevels: rule?.reasoningLevels ?? [],
    });
  }
  return out;
}

export type ModelOption = ReturnType<typeof modelOptions>[number];

/**
 * The picker option a stored model selection names — matching through
 * `matchProviderId`, so an `account:`-spelled agent file maps onto the
 * `builtin:`-spelled option for the same plan.
 */
export function findModelOption(
  options: ModelOption[],
  providerId: string,
  modelId: string,
): ModelOption | undefined {
  const key = matchProviderId(providerId);
  return options.find(
    (o) => matchProviderId(o.providerId) === key && o.modelId === modelId,
  );
}

/**
 * The frontmatter `model` value for one selection — the desktop's own
 * encoding (subagent-markdown-selection.ts): a plain `providerId/modelId`
 * stays readable, and anything the parser would misread (a provider id with
 * `/` or the `custom:` prefix, which is where a custom endpoint's models
 * live) travels as `custom:${encodeURIComponent(providerId)}:${…modelId}`.
 */
export function encodeSubagentModel(
  providerId: string,
  modelId: string,
): string {
  if (
    providerId.startsWith("custom:") ||
    providerId.includes("/") ||
    modelId.includes("$")
  ) {
    return `custom:${encodeURIComponent(providerId)}:${encodeURIComponent(modelId)}`;
  }
  return `${providerId}/${modelId}`;
}

/**
 * The inverse of `encodeSubagentModel`, for prefilling the picker from a
 * file that may have been written by the desktop app — where a custom-endpoint
 * model reads `custom:account%3A…:GLM-5.3` and the id needs decoding back.
 *
 * Returns null for a value this build cannot map onto a picker option (a
 * hand-written id, or the legacy `inherit`-style spellings); the form then
 * shows it read-only rather than silently substituting a different model.
 *
 * The legacy `custom:builtin:<family>:<model>` spelling carries an UNENCODED
 * colon inside the provider id, so the separator cannot simply be the first
 * one — the desktop's own decoder treats a `builtin` first segment as two.
 */
export function decodeSubagentModel(
  value: string | undefined,
): { providerId: string; modelId: string } | null {
  const raw = value?.trim();
  if (!raw) return null;
  if (raw.startsWith("custom:")) {
    const body = raw.slice("custom:".length);
    const sep = body.startsWith("builtin:")
      ? body.indexOf(":", "builtin:".length)
      : body.indexOf(":");
    if (sep <= 0) return null;
    return {
      providerId: decodeSegment(body.slice(0, sep)),
      modelId: decodeSegment(body.slice(sep + 1)),
    };
  }
  const sep = raw.indexOf("/");
  if (sep <= 0) return null;
  return {
    providerId: raw.slice(0, sep),
    // The `$level` suffix is a picker spelling the frontmatter does not use;
    // a value carrying it still names the same model.
    modelId: raw.slice(sep + 1).split("$")[0]!,
  };
}

/** decodeURIComponent that leaves a malformed escape as-is rather than throwing. */
function decodeSegment(value: string): string {
  try {
    return decodeURIComponent(value);
  } catch {
    return value;
  }
}

export function ModelsPage() {
  const { t } = useTranslation();
  const models = useAppStore((s) => s.configModels);
  const supported = useAppStore((s) => s.configSupported);
  const loading = useAppStore((s) => s.configLoading);
  const error = useAppStore((s) => s.configError);
  const loadConfigSection = useAppStore((s) => s.loadConfigSection);
  const applyConfigWrite = useAppStore((s) => s.applyConfigWrite);
  const reorderProviderEntries = useAppStore(
    (s) => s.reorderProviderEntries,
  );

  const payload = models as ModelsPayload | null;
  const available = payload?.models?.available ?? [];
  const rules = payload?.models?.modelRules ?? [];
  // The providers block renders in the payload's display order
  // (`providerOrder` — the reorder write's own sequence).
  const providers = orderedProviders(payload);
  const providerIds = providers.map((p) => p.providerId);

  const [editing, setEditing] = useState<ModelRef | null>(null);
  const [adding, setAdding] = useState(false);
  const [editingProvider, setEditingProvider] = useState<ProviderRow | null>(
    null,
  );
  const [addingProvider, setAddingProvider] = useState(false);

  // A move swaps two neighbors and posts the WHOLE sequence (the route takes
  // the desired order, not an index swap).
  async function moveProvider(index: number, delta: -1 | 1) {
    const next = [...providerIds];
    const target = index + delta;
    if (target < 0 || target >= next.length) return;
    [next[index], next[target]] = [next[target]!, next[index]!];
    await applyConfigWrite("reorder providers", () =>
      reorderProviderEntries(next),
    ["models"]);
  }

  return (
    <ConfigPageFrame
      title={t("zconfig.models")}
      onBack={() => useAppStore.getState().backFromConfigSection()}
      onRefresh={() => void loadConfigSection("models")}
      refreshing={loading}
      unsupported={supported === false}
      error={error}
      hasLoaded={models !== null}
    >
      {/* The providers block: every provider this machine declares, with its
          endpoint form, reorder pair, and create entry. It leads because an
          endpoint problem breaks every model under it. */}
      {providers.length > 0 && (
        <>
          <h2 className="px-4 pb-1 pt-2 text-2xs font-medium uppercase tracking-wide text-faint">
            {t("zconfig.providersTitle")}
          </h2>
          <ConfigBlock>
            {providers.map((p, index) => (
              <div
                key={p.providerId}
                className="flex items-center gap-2 px-4 py-3"
              >
                <button
                  onClick={() => setEditingProvider(p)}
                  className="min-w-0 flex-1 text-left active:opacity-80"
                >
                  <span className="block text-sm text-ink">{p.providerName}</span>
                  <span className="block truncate text-2xs text-faint">
                    {[p.baseUrl ?? "—", p.apiType, p.hasApiKey ? t("zconfig.providerKeySet") : t("zconfig.providerKeyMissing")]
                      .filter(Boolean)
                      .join(" · ")}
                  </span>
                </button>
                <button
                  onClick={() => void moveProvider(index, -1)}
                  disabled={loading || index === 0}
                  aria-label={t("zconfig.providerMoveUp")}
                  className="flex size-8 shrink-0 items-center justify-center rounded-full text-dim hover:bg-hover active:bg-active disabled:opacity-40"
                >
                  <ChevronUp className="size-4" />
                </button>
                <button
                  onClick={() => void moveProvider(index, 1)}
                  disabled={loading || index + 1 >= providers.length}
                  aria-label={t("zconfig.providerMoveDown")}
                  className="flex size-8 shrink-0 items-center justify-center rounded-full text-dim hover:bg-hover active:bg-active disabled:opacity-40"
                >
                  <ChevronDown className="size-4" />
                </button>
              </div>
            ))}
          </ConfigBlock>
          <div className="px-4">
            <button
              onClick={() => setAddingProvider(true)}
              className="mt-2 flex w-full items-center justify-center gap-2 rounded-xl bg-raised px-3 py-2.5 text-sm text-dim hover:bg-hover active:bg-active"
            >
              <Plus className="size-4" />
              {t("zconfig.providerAdd")}
            </button>
          </div>
        </>
      )}

      {available.length === 0 ? (
        <ConfigEmpty text={t("zconfig.modelsEmpty")} />
      ) : (
        <ConfigBlock>
          {available.map((m) => {
            const key = `${m.providerId ?? ""}/${m.modelId ?? ""}`;
            const contextWindow = contextWindowFor(m, rules);
            const provider = m.providerName ?? m.providerId ?? "—";
            return (
              <button
                key={key}
                onClick={() => setEditing(m)}
                className="flex w-full items-center gap-3 px-4 py-3 text-left hover:bg-hover active:bg-active"
              >
                <span className="min-w-0 flex-1">
                  <span className="block text-sm text-ink">
                    {m.modelId ?? key}
                  </span>
                  <span className="block truncate text-2xs text-faint">
                    {provider}
                    {contextWindow
                      ? ` · ${(contextWindow / 1000).toFixed(0)}k context`
                      : ""}
                  </span>
                </span>
                <ChevronRight className="size-4 shrink-0 text-faint" />
              </button>
            );
          })}
        </ConfigBlock>
      )}

      <div className="px-4">
        <button
          onClick={() => setAdding(true)}
          className="mt-3 flex w-full items-center justify-center gap-2 rounded-xl bg-raised px-3 py-2.5 text-sm text-dim hover:bg-hover active:bg-active"
        >
          <Plus className="size-4" />
          {t("zconfig.modelsAdd")}
        </button>
      </div>

      {editing && (
        <ModelForm
          key={`${editing.providerId}/${editing.modelId}`}
          payload={payload}
          target={editing}
          onClose={() => setEditing(null)}
        />
      )}
      {adding && (
        <ModelForm
          key="add"
          payload={payload}
          target={null}
          onClose={() => setAdding(false)}
        />
      )}
      {editingProvider && (
        <ProviderForm
          key={editingProvider.providerId}
          target={editingProvider}
          onClose={() => setEditingProvider(null)}
        />
      )}
      {addingProvider && (
        <ProviderForm key="provider-add" target={null} onClose={() => setAddingProvider(false)} />
      )}
    </ConfigPageFrame>
  );
}

/**
 * Add/edit form for one provider rule (settings parity: provider CRUD).
 *
 * Create posts the name (the id is DERIVED server-side) plus only the
 * optional fields actually filled; edit PUTs the endpoint fields, with the
 * key box starting EMPTY — the server never sends the secret back, so a
 * blank submit keeps the stored key. The edit form's footer deletes
 * (two taps, the form contract): removing a provider takes its model rules
 * and order entry with it.
 */
function ProviderForm({
  target,
  onClose,
}: {
  // Null = creating one; the name box exists only in that mode.
  target: ProviderRow | null;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const applyConfigWrite = useAppStore((s) => s.applyConfigWrite);
  const createProviderEntry = useAppStore((s) => s.createProviderEntry);
  const removeProviderEntry = useAppStore((s) => s.removeProviderEntry);
  const updateProviderEntry = useAppStore((s) => s.updateProviderEntry);

  const initialUrl = target?.baseUrl ?? "";
  const [name, setName] = useState("");
  const [url, setUrl] = useState(initialUrl);
  const [apiKey, setApiKey] = useState("");
  const [busy, setBusy] = useState(false);
  // The wire formats the runtime understands; "" means "not set" (create)
  // or "keep absent" (edit — the value rides only when changed).
  const [apiType, setApiType] = useState(target?.apiType ?? "");
  const knownTypes = [
    "anthropic-messages",
    "openai-chat-completions",
    "openai-responses",
  ];
  const typeOptions =
    apiType && !knownTypes.includes(apiType)
      ? [...knownTypes, apiType]
      : knownTypes;

  const urlOk = url.trim() === "" || /^[a-z][a-z0-9+.-]*:\/\/\S+$/i.test(url.trim());
  const canSubmit = (!target ? name.trim() !== "" : true) && urlOk;
  // The form contract's dirty gate: a create is dirty whenever valid; an
  // edit compares against the opened values (a typed key is always a change
  // — the box starts empty because the secret never comes back).
  const dirty = !target
    ? canSubmit
    : url.trim() !== initialUrl.trim() ||
      apiKey.trim() !== "" ||
      apiType !== (target.apiType ?? "");

  async function submit() {
    setBusy(true);
    const ok = target
      ? await applyConfigWrite("update provider", () =>
          updateProviderEntry(target.providerId, {
            ...(url.trim() !== initialUrl.trim() ? { baseUrl: url.trim() } : {}),
            ...(apiType !== (target.apiType ?? "") ? { apiType: apiType || null } : {}),
            ...(apiKey.trim() !== "" ? { apiKey: apiKey.trim() } : {}),
          }),
        ["models"])
      : await applyConfigWrite("add provider", () =>
          createProviderEntry({
            providerName: name.trim(),
            ...(apiType ? { apiType } : {}),
            ...(url.trim() ? { baseUrl: url.trim() } : {}),
            ...(apiKey.trim() ? { apiKey: apiKey.trim() } : {}),
          }),
        ["models"]);
    setBusy(false);
    if (ok) onClose();
  }

  async function remove() {
    if (!target) return;
    setBusy(true);
    const ok = await applyConfigWrite(
      "delete provider",
      () => removeProviderEntry(target.providerId),
      ["models"],
    );
    setBusy(false);
    if (ok) onClose();
  }

  return (
    <ConfigFormSheet
      title={target ? t("zconfig.providerEdit") : t("zconfig.providerAdd")}
      busy={busy}
      submitDisabled={!canSubmit || !dirty}
      dirty={dirty}
      footerLeft={
        target && !isAccountProvider(target.providerId) ? (
          <FormDeleteButton
            label={t("zconfig.providerDelete")}
            busy={busy}
            onConfirm={() => void remove()}
          />
        ) : undefined
      }
      onSubmit={() => void submit()}
      onClose={onClose}
    >
      {target && (
        <p className="mt-3 font-mono text-2xs text-faint">
          {target.providerName} · {target.providerId}
        </p>
      )}
      {!target && (
        <ConfigField label={t("zconfig.providerName")}>
          <input
            value={name}
            onChange={(e) => setName(e.target.value)}
            autoFocus
            autoCapitalize="none"
            autoCorrect="off"
            spellCheck={false}
            placeholder="My API"
            className={configInputClass}
          />
        </ConfigField>
      )}
      <ConfigField label={t("zconfig.providerBaseUrl")}>
        <input
          value={url}
          onChange={(e) => setUrl(e.target.value)}
          autoFocus={!!target}
          inputMode="url"
          autoCapitalize="none"
          autoCorrect="off"
          spellCheck={false}
          className={configInputClass}
        />
      </ConfigField>
      <ConfigField label={t("zconfig.providerApiType")}>
        <select
          value={apiType}
          onChange={(e) => setApiType(e.target.value)}
          className={configInputClass}
        >
          <option value="">{t("zconfig.providerApiTypeDefault")}</option>
          {typeOptions.map((v) => (
            <option key={v} value={v}>
              {v}
            </option>
          ))}
        </select>
      </ConfigField>
      <ConfigField
        label={t("zconfig.providerApiKey")}
        hint={
          target?.hasApiKey
            ? t("zconfig.providerKeyKeepHint")
            : t("zconfig.providerKeyHint")
        }
      >
        <input
          value={apiKey}
          onChange={(e) => setApiKey(e.target.value)}
          autoCapitalize="none"
          autoCorrect="off"
          spellCheck={false}
          className={configInputClass}
        />
      </ConfigField>
    </ConfigFormSheet>
  );
}

/**
 * Add/edit form for one model rule.
 *
 * Blank fields are OMITTED from the write, which under the route's merge
 * semantics means "unchanged" — the API has no way to clear a context window,
 * so the hints say keep rather than clear.
 *
 * The reasoning levels are chosen from the levels THIS machine's rules already
 * use (chips, not free text): the runtime only accepts a value the model
 * declares, and the union of the existing rules is the only list of those a
 * client can see. A level already set on this model stays selected even when
 * no other model uses it.
 */
function ModelForm({
  payload,
  target,
  onClose,
}: {
  payload: ModelsPayload | null;
  // Null = adding a model that may not exist yet.
  target: ModelRef | null;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const applyConfigWrite = useAppStore((s) => s.applyConfigWrite);
  const upsertModel = useAppStore((s) => s.upsertModel);
  const removeModel = useAppStore((s) => s.removeModel);

  const providers = providerOptions(payload);
  const rules = payload?.models?.modelRules ?? [];
  const rule = target ? ruleFor(target, rules) : undefined;

  const initialWindow = rule?.contextWindow ? String(rule.contextWindow) : "";
  const initialLevels = rule?.reasoningLevels ?? [];
  const initialEnabled = rule?.enabled !== false;

  const [providerId, setProviderId] = useState(target?.providerId ?? "");
  const [modelId, setModelId] = useState(target?.modelId ?? "");
  const [contextWindow, setContextWindow] = useState(initialWindow);
  const [levels, setLevels] = useState<string[]>(() => initialLevels);
  const [enabled, setEnabled] = useState(initialEnabled);
  const [busy, setBusy] = useState(false);

  // Every level any rule declares, plus the ones this model already carries —
  // a value the machine has never seen elsewhere is still this model's own.
  const candidates = [
    ...new Set([
      ...(rule?.reasoningLevels ?? []),
      ...rules.flatMap((r) => r.reasoningLevels ?? []),
    ]),
  ];

  const canSubmit =
    providerId.trim() !== "" &&
    modelId.trim() !== "" &&
    contextWindowOk(contextWindow);
  // The form contract's dirty gate: a create is dirty whenever it is valid
  // (a valid create has content); an edit compares against the opened values.
  const dirty = !target
    ? canSubmit
    : contextWindow.trim() !== initialWindow.trim() ||
      levels.join("\n") !== initialLevels.join("\n") ||
      enabled !== initialEnabled;

  async function submit() {
    const window = contextWindow.trim();
    setBusy(true);
    const ok = await applyConfigWrite(
      target ? "update model" : "add model",
      () =>
        upsertModel({
          providerId: providerId.trim(),
          modelId: modelId.trim(),
          enabled,
          ...(window !== "" ? { contextWindow: Number(window) } : {}),
          // A rule with no levels selected keeps whatever it has: the route
          // MERGES, so an empty array is a no-op rather than a clear. There is
          // no way to clear the list once written (delete the rule instead),
          // which is why the chip row explains what it holds.
          ...(levels.length > 0 ? { reasoningLevels: levels } : {}),
        }),
      ["models"],
    );
    setBusy(false);
    if (ok) onClose();
  }

  // The edit form's footer delete (the form contract): removes the rule —
  // the model stops being offered anywhere — so it stays two-tap.
  async function remove() {
    const providerId = target?.providerId;
    const modelId = target?.modelId;
    if (!target || !providerId || !modelId) return;
    setBusy(true);
    const ok = await applyConfigWrite(
      "delete model",
      () => removeModel(providerId, modelId),
      ["models"],
    );
    setBusy(false);
    if (ok) onClose();
  }

  return (
    <ConfigFormSheet
      title={target ? t("zconfig.modelsEdit") : t("zconfig.modelsAdd")}
      busy={busy}
      submitDisabled={!canSubmit || !dirty}
      dirty={dirty}
      footerLeft={
        target ? (
          <FormDeleteButton
            label={t("zconfig.modelDelete")}
            busy={busy}
            onConfirm={() => void remove()}
          />
        ) : undefined
      }
      onSubmit={() => void submit()}
      onClose={onClose}
    >
      {target ? (
        <p className="mt-3 font-mono text-2xs text-faint">
          {[target.providerId, target.modelId].filter(Boolean).join(" / ")}
        </p>
      ) : (
        <>
          <ConfigField label={t("zconfig.modelProvider")}>
            <select
              value={providerId}
              onChange={(e) => setProviderId(e.target.value)}
              className={configInputClass}
            >
              <option value="">—</option>
              {providers.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name}
                </option>
              ))}
            </select>
          </ConfigField>
          <ConfigField label={t("zconfig.modelId")}>
            <input
              value={modelId}
              onChange={(e) => setModelId(e.target.value)}
              autoFocus
              autoCapitalize="none"
              autoCorrect="off"
              spellCheck={false}
              placeholder="glm-4.6"
              className={configInputClass}
            />
          </ConfigField>
        </>
      )}

      <ConfigField
        label={t("zconfig.modelContextWindow")}
        hint={t("zconfig.modelKeepBlank")}
      >
        <input
          value={contextWindow}
          onChange={(e) => setContextWindow(e.target.value)}
          autoFocus={!!target}
          inputMode="numeric"
          autoCapitalize="none"
          placeholder="200000"
          className={configInputClass}
        />
      </ConfigField>

      <div className="mt-4">
        <span className="block text-xs font-medium text-dim">
          {t("zconfig.modelReasoningLevels")}
        </span>
        <span className="mt-1 block text-2xs text-faint">
          {t("zconfig.modelLevelsHint")}
        </span>
        {candidates.length === 0 ? (
          <p className="mt-2 text-2xs text-faint">
            {t("zconfig.modelLevelsNone")}
          </p>
        ) : (
          <div className="mt-2 flex flex-wrap gap-2">
            {candidates.map((l) => {
              const on = levels.includes(l);
              return (
                <button
                  key={l}
                  type="button"
                  aria-pressed={on}
                  onClick={() =>
                    setLevels(
                      on ? levels.filter((x) => x !== l) : [...levels, l],
                    )
                  }
                  className={`rounded-lg px-3 py-1.5 text-xs transition ${
                    on
                      ? "bg-active text-ink ring-1 ring-ink/50"
                      : "bg-raised text-faint"
                  }`}
                >
                  {l}
                </button>
              );
            })}
          </div>
        )}
      </div>

      <div className="mt-4 flex items-center justify-between">
        <span className="text-xs font-medium text-dim">
          {t("zconfig.enabled")}
        </span>
        <ConfigToggle
          checked={enabled}
          label={t("zconfig.enabled")}
          onChange={setEnabled}
        />
      </div>
    </ConfigFormSheet>
  );
}
