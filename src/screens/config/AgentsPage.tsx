import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { ChevronRight, Plus } from "lucide-react";
import { useAppStore } from "../../store/appStore";
import type { AgentUpsert } from "../../lib/types";
import {
  ConfigBlock,
  ConfigEmpty,
  ConfigPageFrame,
} from "../../components/config/ConfigPage";
import {
  ConfigField,
  ConfigFormSheet,
  configInputClass,
} from "../../components/config/ConfigFormSheet";
import {
  providerOptions,
  modelOptions,
  findModelOption,
  matchProviderId,
  decodeSubagentModel,
  encodeSubagentModel,
  type ModelsPayload,
} from "./ModelsPage";

// Subagents (ADR-0009). Tapping an agent opens its configuration; the API
// splits the two kinds by what they are:
//   - a built-in (general-purpose, Explore) has no markdown file, so the only
//     editable thing is its model override, stored in the agents state file;
//   - a personal agent is a markdown file — the form edits the frontmatter
//     fields the server exposes (description, color, model, thoughtLevel) and
//     a PUT on a missing name creates it with a template body.
// The system-prompt body and the advanced frontmatter lists stay out: a body
// round-trip through a phone keyboard is how a broken agent gets committed,
// so the footer points at the desktop for that.

/** The eight colors the server's own form offers (agents-config.ts). */
export const AGENT_COLORS = [
  "red",
  "blue",
  "green",
  "yellow",
  "purple",
  "orange",
  "pink",
  "cyan",
] as const;

export type AgentColor = (typeof AGENT_COLORS)[number];

// Agent accent swatches: user-chosen dot colors, mapped onto the token
// palette (status + chart hues) — no raw Tailwind palette values.
const SWATCH: Record<AgentColor, string> = {
  red: "bg-danger",
  blue: "bg-chart-5",
  green: "bg-success",
  yellow: "bg-warning",
  purple: "bg-chart-6",
  orange: "bg-chart-1",
  pink: "bg-chart-4",
  cyan: "bg-info",
};

/** The server's name rule for a personal agent: 3–50 of [a-zA-Z0-9-]. */
export function isValidAgentName(name: string): boolean {
  return /^[a-zA-Z0-9-]{3,50}$/.test(name);
}

/**
 * Whether an agent's controls must be hidden, on the server's verdict.
 *
 * A built-in has no file to edit or delete, so the API refuses both with a 400
 * the user cannot act on. Reading the field the bridge sends (rather than
 * guessing from the name) is what keeps those controls off; the page's payload
 * type spells the same field.
 */
export function isAgentReadOnly(readOnly: boolean | undefined): boolean {
  return readOnly === true;
}

export interface AgentEntryView {
  name?: string;
  frontmatter?: {
    name?: string;
    description?: string;
    color?: string;
    model?: string;
    thoughtLevel?: string;
  };
  enabled?: boolean;
  /** Built-in agents cannot be edited or deleted — the server's word. */
  readOnly?: boolean;
  /** The resolved model override, when one is set. */
  modelSelection?: {
    providerId?: string;
    modelId?: string;
    thoughtLevel?: string | null;
  };
  path?: string;
}

export function AgentsPage() {
  const { t } = useTranslation();
  const agents = useAppStore((s) => s.configAgents);
  const configModels = useAppStore((s) => s.configModels);
  const supported = useAppStore((s) => s.configSupported);
  const loading = useAppStore((s) => s.configLoading);
  const error = useAppStore((s) => s.configError);
  const loadConfigSection = useAppStore((s) => s.loadConfigSection);
  const applyConfigWrite = useAppStore((s) => s.applyConfigWrite);
  const setAgentEnabled = useAppStore((s) => s.setAgentEnabled);

  const payload = agents as { ok?: boolean; agents?: AgentEntryView[] } | null;

  const rows = payload?.agents ?? [];
  // For the rows' model labels only; the payload may not be loaded yet, in
  // which case the label degrades to the bare model id.
  const pickerOptions = modelOptions(configModels as ModelsPayload | null);
  useEffect(() => {
    if (configModels === null) void loadConfigSection("models");
  }, [configModels, loadConfigSection]);

  const [editing, setEditing] = useState<AgentEntryView | null>(null);
  const [adding, setAdding] = useState(false);

  async function toggle(name: string, enable: boolean) {
    await applyConfigWrite(
      enable ? "enable subagent" : "disable subagent",
      () => setAgentEnabled(name, enable),
      ["agents"],
    );
  }

  return (
    <ConfigPageFrame
      title={t("zconfig.agents")}
      onBack={() => useAppStore.getState().openConfig(null)}
      onRefresh={() => void loadConfigSection("agents")}
      refreshing={loading}
      unsupported={supported === false}
      error={error}
      hasLoaded={agents !== null}
    >
      {rows.length === 0 ? (
        <ConfigEmpty text={t("zconfig.agentsEmpty")} />
      ) : (
        <ConfigBlock>
          {rows.map((a) => {
            const name = a.name ?? "";
            // `readOnly` is the server's verdict (a built-in has no file to
            // edit or delete). Trusting it — rather than guessing from the
            // name — is what keeps the enable control off agents the API
            // would refuse anyway.
            const readOnly = isAgentReadOnly(a.readOnly);
            const description = a.frontmatter?.description;
            // `modelSelection` is the built-ins' override; a personal agent's
            // model lives in its own frontmatter. Both render as the picker
            // label — never the raw `custom:…` spelling the file carries.
            const stored = decodeSubagentModel(a.frontmatter?.model);
            const model =
              a.modelSelection?.modelId ??
              (stored
                ? (findModelOption(
                    pickerOptions,
                    stored.providerId,
                    stored.modelId,
                  )?.label ?? stored.modelId)
                : a.frontmatter?.model);
            const thoughtLevel =
              a.modelSelection?.thoughtLevel ?? a.frontmatter?.thoughtLevel;
            return (
              <div key={name} className="flex items-start gap-3 px-4 py-3">
                <button
                  onClick={() => setEditing(a)}
                  className="min-w-0 flex-1 text-left active:opacity-80"
                >
                  <span className="block text-sm text-ink">{name}</span>
                  {description && (
                    <span className="block truncate text-2xs text-faint">
                      {description}
                    </span>
                  )}
                  {model && (
                    <span className="block truncate text-2xs text-faint">
                      {model}
                      {thoughtLevel ? ` · ${thoughtLevel}` : ""}
                    </span>
                  )}
                  {readOnly && (
                    <span className="block text-2xs text-faint">
                      {t("zconfig.agentBuiltIn")}
                    </span>
                  )}
                </button>
                {/* Immediate identity: the switch only exists where the API
                    accepts it — a built-in cannot be disabled. */}
                {!readOnly && (
                  <button
                    role="switch"
                    aria-checked={a.enabled !== false}
                    aria-label={t("zconfig.agentEnabled")}
                    onClick={() => void toggle(name, a.enabled === false)}
                    className={`mt-0.5 h-6 w-10 shrink-0 rounded-full transition ${
                      a.enabled !== false
                        ? "bg-success/80"
                        : "bg-active"
                    }`}
                  >
                    <span
                      className={`block size-5 rounded-full bg-white transition ${
                        a.enabled !== false
                          ? "translate-x-4.5"
                          : "translate-x-0.5"
                      }`}
                    />
                  </button>
                )}
                <ChevronRight className="mt-1 size-4 shrink-0 text-faint" />
              </div>
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
          {t("zconfig.agentsAdd")}
        </button>
      </div>

      <p className="px-4 pt-3 text-center text-2xs text-faint">
        {t("zconfig.agentsCreateHint")}
      </p>

      {editing &&
        (isAgentReadOnly(editing.readOnly) ? (
          <BuiltInAgentForm
            key={editing.name}
            target={editing}
            onClose={() => setEditing(null)}
          />
        ) : (
          <PersonalAgentForm
            key={editing.name}
            target={editing}
            onClose={() => setEditing(null)}
          />
        ))}
      {adding && <NewAgentForm onClose={() => setAdding(false)} />}
    </ConfigPageFrame>
  );
}

/**
 * The model override for a built-in agent. The picker reads the models
 * section (provider → model → reasoning level), fetching it on first open if
 * the entry list's snapshot did not carry it.
 */
function BuiltInAgentForm({
  target,
  onClose,
}: {
  target: AgentEntryView;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const configModels = useAppStore((s) => s.configModels);
  const loadConfigSection = useAppStore((s) => s.loadConfigSection);
  const applyConfigWrite = useAppStore((s) => s.applyConfigWrite);
  const upsertAgent = useAppStore((s) => s.upsertAgent);

  const name = target.name ?? "";
  const override = target.modelSelection;

  // The models payload is the picker's data source; an entry screen that
  // never listed models leaves it null, so fetch it here.
  useEffect(() => {
    if (configModels === null) void loadConfigSection("models");
  }, [configModels, loadConfigSection]);

  const payload = configModels as ModelsPayload | null;
  const providers = providerOptions(payload);
  const available = payload?.models?.available ?? [];
  const rules = payload?.models?.modelRules ?? [];

  const [providerId, setProviderId] = useState(override?.providerId ?? "");
  const [modelId, setModelId] = useState(override?.modelId ?? "");
  const [reasoningLevel, setReasoningLevel] = useState(
    override?.thoughtLevel ?? "",
  );
  const [busy, setBusy] = useState(false);

  const modelIds = [
    ...new Set(
      available
        .filter(
          (m) => matchProviderId(m.providerId) === matchProviderId(providerId),
        )
        .map((m) => m.modelId!)
        .filter(Boolean),
    ),
  ];
  const levels =
    rules.find(
      (r) =>
        matchProviderId(r.providerId) === matchProviderId(providerId) &&
        r.modelId === modelId,
    )?.reasoningLevels ?? [];

  async function submit() {
    setBusy(true);
    const level = reasoningLevel.trim();
    const ok = await applyConfigWrite(
      "set model override",
      () =>
        upsertAgent(name, {
          providerId,
          modelId,
          ...(level !== "" ? { reasoningLevel: level } : {}),
        }),
      ["agents"],
    );
    setBusy(false);
    if (ok) onClose();
  }

  async function clearOverride() {
    setBusy(true);
    // Both keys null TOGETHER is the server's spelling for "no override" —
    // one null alone is a malformed request.
    const ok = await applyConfigWrite(
      "clear model override",
      () => upsertAgent(name, { providerId: null, modelId: null }),
      ["agents"],
    );
    setBusy(false);
    if (ok) onClose();
  }

  return (
    <ConfigFormSheet
      title={`${t("zconfig.agentEdit")} · ${name}`}
      busy={busy}
      submitDisabled={providerId === "" || modelId === ""}
      onSubmit={() => void submit()}
      onClose={onClose}
    >
      <p className="mt-3 text-2xs text-faint">
        {override?.modelId
          ? `${providers.find((p) => matchProviderId(p.id) === matchProviderId(override.providerId))?.name ?? override.providerId} / ${override.modelId}${override.thoughtLevel ? ` · ${override.thoughtLevel}` : ""}`
          : t("zconfig.agentOverrideNone")}
      </p>

      {configModels === null && (
        <p className="mt-3 text-xs text-faint">{t("zconfig.loading")}</p>
      )}

      <ConfigField label={t("zconfig.modelProvider")}>
        <select
          value={providerId}
          onChange={(e) => {
            setProviderId(e.target.value);
            setModelId("");
            setReasoningLevel("");
          }}
          className={configInputClass}
        >
          <option value="">—</option>
          {/* A stored override may name a provider the picker filters out (an
              account plan). Keep it selectable so re-saving the same override
              does not blank it. */}
          {providerId && !providers.some((p) => p.id === providerId) && (
            <option value={providerId}>{providerId}</option>
          )}
          {providers.map((p) => (
            <option key={p.id} value={p.id}>
              {p.name}
            </option>
          ))}
        </select>
      </ConfigField>

      <ConfigField label={t("zconfig.modelId")}>
        <select
          value={modelId}
          onChange={(e) => {
            setModelId(e.target.value);
            setReasoningLevel("");
          }}
          disabled={providerId === ""}
          className={configInputClass}
        >
          <option value="">—</option>
          {/* The stored override may name a model the payload's list omits (a
              model removed from the dropdown since it was set). Keep it
              selectable so re-saving the same override does not blank it. */}
          {modelId && !modelIds.includes(modelId) && (
            <option value={modelId}>{modelId}</option>
          )}
          {modelIds.map((id) => (
            <option key={id} value={id}>
              {id}
            </option>
          ))}
        </select>
      </ConfigField>

      {levels.length > 0 && (
        <ConfigField label={t("zconfig.agentReasoning")}>
          <select
            value={reasoningLevel}
            onChange={(e) => setReasoningLevel(e.target.value)}
            className={configInputClass}
          >
            <option value="">—</option>
            {levels.map((l) => (
              <option key={l} value={l}>
                {l}
              </option>
            ))}
          </select>
        </ConfigField>
      )}

      {override?.modelId && (
        <button
          type="button"
          onClick={() => void clearOverride()}
          className="mt-4 w-full rounded-xl px-3 py-2 text-xs font-medium text-dim hover:bg-hover active:bg-active"
        >
          {t("zconfig.agentOverrideClear")}
        </button>
      )}
    </ConfigFormSheet>
  );
}

/**
 * The frontmatter fields of a personal agent.
 *
 * The picker's data (the models section) arrives asynchronously, so the stored
 * model cannot seed the picker on the first render — it would be filled in as
 * "no model chosen" and, on save, sent as an explicit null that WIPES the
 * agent's model. The prefill therefore waits for the payload and marks whether
 * it has run: `undefined` means "not seeded yet", `""` means "deliberately
 * inheriting".
 *
 * The write is gated on the user having PICKED a model (`modelTouched`): an
 * untouched save rewrites neither the model nor the thought level, so the
 * file keeps its own spelling of both. Picking "inherit" sends the null that
 * clears the key; a stored value this build cannot map onto a picker option
 * is shown read-only and never written.
 */
function PersonalAgentForm({
  target,
  onClose,
}: {
  target: AgentEntryView;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const configModels = useAppStore((s) => s.configModels);
  const loadConfigSection = useAppStore((s) => s.loadConfigSection);
  const applyConfigWrite = useAppStore((s) => s.applyConfigWrite);
  const upsertAgent = useAppStore((s) => s.upsertAgent);

  // The picker's data source; the entry screen's snapshot carries it, but a
  // screen opened straight to an agent does not.
  const payloadLanding = configModels === null;
  useEffect(() => {
    if (payloadLanding) void loadConfigSection("models");
  }, [payloadLanding, loadConfigSection]);

  const name = target.name ?? "";
  const modelWas = target.frontmatter?.model;
  const thoughtWas = target.frontmatter?.thoughtLevel;

  // The file's model is a picker option's value only when it decodes to one
  // this machine offers. Matching goes through `findModelOption`, which
  // normalizes the coding-plan provider spellings — the file says
  // `account:…`, the payload says `builtin:…`, and a raw comparison never
  // matches. Anything still unmappable (a hand-written id) stays visible
  // read-only instead of being silently replaced by a different model.
  const stored = decodeSubagentModel(modelWas);
  const payload = configModels as ModelsPayload | null;
  const options = modelOptions(payload);
  const matched = stored
    ? findModelOption(options, stored.providerId, stored.modelId)
    : undefined;
  const storedKnown = stored !== null && matched !== undefined;

  const [description, setDescription] = useState(
    target.frontmatter?.description ?? "",
  );
  const [color, setColor] = useState(
    (AGENT_COLORS as readonly string[]).includes(
      target.frontmatter?.color ?? "",
    )
      ? target.frontmatter!.color!
      : "",
  );
  // Seeded once, when the picker's options first exist: before that the state
  // is unknown, not empty (see the doc comment).
  const [model, setModel] = useState<string | undefined>(undefined);
  const [thoughtLevel, setThoughtLevel] = useState<string | undefined>(
    undefined,
  );
  // Only a user pick makes the model writeable: seeding must not count, or an
  // untouched save would rewrite the file's own spelling of the same model.
  const [modelTouched, setModelTouched] = useState(false);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (model !== undefined || !storedKnown || !stored) return;
    setModel(encodeSubagentModel(stored.providerId, stored.modelId));
    setThoughtLevel(thoughtWas ?? "");
  }, [model, storedKnown, stored, thoughtWas]);

  // The level vocabulary follows the model actually selected — not the one
  // the file had. A stored level the vocabulary omits (GLM-5.3's rules carry
  // none, yet the desktop writes `max`) stays selectable while the stored
  // model is selected, so the file's value survives an unrelated edit.
  const activeOption =
    model !== undefined && model !== ""
      ? options.find(
          (o) => encodeSubagentModel(o.providerId, o.modelId) === model,
        )
      : undefined;
  const onStoredModel =
    activeOption !== undefined &&
    matched !== undefined &&
    activeOption.providerId === matched.providerId &&
    activeOption.modelId === matched.modelId;
  const levelChoices = [...(activeOption?.reasoningLevels ?? [])];
  if (
    onStoredModel &&
    thoughtWas !== undefined &&
    thoughtWas !== "" &&
    !levelChoices.includes(thoughtWas)
  ) {
    levelChoices.push(thoughtWas);
  }

  const canSubmit = description.trim() !== "";

  async function submit() {
    const body: AgentUpsert = { description: description.trim() };
    if (color !== "") body.color = color;
    // An untouched model (never picked, or unmappable and shown read-only) is
    // omitted entirely — the merge semantics then leave the file's model and
    // thought level exactly as they are, whatever spelling they carry.
    if (modelTouched) {
      const chosen = model ?? "";
      if (chosen !== "") body.model = chosen;
      else if (modelWas !== undefined) body.model = null;
      const level = (thoughtLevel ?? "").trim();
      if (level !== "" && levelChoices.includes(level))
        body.thoughtLevel = level;
      else if (thoughtWas !== undefined) body.thoughtLevel = null;
    }

    setBusy(true);
    const ok = await applyConfigWrite(
      "update subagent",
      () => upsertAgent(name, body),
      ["agents"],
    );
    setBusy(false);
    if (ok) onClose();
  }

  return (
    <ConfigFormSheet
      title={`${t("zconfig.agentEdit")} · ${name}`}
      busy={busy}
      submitDisabled={!canSubmit}
      onSubmit={() => void submit()}
      onClose={onClose}
    >
      <ConfigField label={t("zconfig.agentDescription")}>
        <textarea
          value={description}
          onChange={(e) => setDescription(e.target.value)}
          rows={3}
          className={configInputClass}
        />
      </ConfigField>

      <ConfigField label={t("zconfig.agentColor")}>
        <ColorPicker value={color} onChange={setColor} />
      </ConfigField>

      {modelWas !== undefined && !storedKnown ? (
        // A model id this build cannot map onto a picker option (hand-written
        // in the file). Shown read-only, and the submit above leaves it alone:
        // substituting a different model on the user's behalf is worse than
        // keeping the value they wrote.
        <ConfigField label={t("zconfig.agentModel")}>
          <p className="mt-1 font-mono text-xs text-faint">{modelWas}</p>
        </ConfigField>
      ) : (
        <ModelPickerFields
          options={options}
          levels={levelChoices}
          model={model ?? ""}
          thoughtLevel={thoughtLevel ?? ""}
          onModelChange={(next) => {
            setModel(next);
            setModelTouched(true);
            setThoughtLevel("");
          }}
          onThoughtLevelChange={setThoughtLevel}
        />
      )}
    </ConfigFormSheet>
  );
}

/**
 * Creating an agent = PUT on a new name. The server seeds a template system
 * prompt; the footer hint points at the desktop for refining it.
 */
function NewAgentForm({ onClose }: { onClose: () => void }) {
  const { t } = useTranslation();
  const configModels = useAppStore((s) => s.configModels);
  const loadConfigSection = useAppStore((s) => s.loadConfigSection);
  const applyConfigWrite = useAppStore((s) => s.applyConfigWrite);
  const upsertAgent = useAppStore((s) => s.upsertAgent);

  useEffect(() => {
    if (configModels === null) void loadConfigSection("models");
  }, [configModels, loadConfigSection]);

  const payload = configModels as ModelsPayload | null;
  const options = modelOptions(payload);

  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [color, setColor] = useState("");
  const [model, setModel] = useState("");
  const [thoughtLevel, setThoughtLevel] = useState("");
  const [busy, setBusy] = useState(false);

  const canSubmit = isValidAgentName(name.trim()) && description.trim() !== "";

  // The levels a picker choice carries come with the option, so the level
  // field can only offer values the chosen model actually supports.
  const levels =
    options.find((o) => encodeSubagentModel(o.providerId, o.modelId) === model)
      ?.reasoningLevels ?? [];

  async function submit() {
    const body: AgentUpsert = { description: description.trim() };
    if (color !== "") body.color = color;
    if (model !== "") body.model = model;
    if (thoughtLevel.trim() !== "") body.thoughtLevel = thoughtLevel.trim();

    setBusy(true);
    const ok = await applyConfigWrite(
      "create subagent",
      () => upsertAgent(name.trim(), body),
      ["agents"],
    );
    setBusy(false);
    if (ok) onClose();
  }

  return (
    <ConfigFormSheet
      title={t("zconfig.agentsAdd")}
      busy={busy}
      submitDisabled={!canSubmit}
      onSubmit={() => void submit()}
      onClose={onClose}
    >
      <ConfigField
        label={t("zconfig.agentName")}
        hint={t("zconfig.agentNameHint")}
      >
        <input
          value={name}
          onChange={(e) => setName(e.target.value)}
          autoCapitalize="none"
          autoCorrect="off"
          spellCheck={false}
          className={configInputClass}
        />
      </ConfigField>

      <ConfigField label={t("zconfig.agentDescription")}>
        <textarea
          value={description}
          onChange={(e) => setDescription(e.target.value)}
          rows={3}
          className={configInputClass}
        />
      </ConfigField>

      <ConfigField label={t("zconfig.agentColor")}>
        <ColorPicker value={color} onChange={setColor} />
      </ConfigField>

      <ModelPickerFields
        options={options}
        levels={levels}
        model={model}
        thoughtLevel={thoughtLevel}
        onModelChange={(next) => {
          setModel(next);
          setThoughtLevel("");
        }}
        onThoughtLevelChange={setThoughtLevel}
      />
    </ConfigFormSheet>
  );
}

/**
 * Model + thought-level pickers, shared by the create and edit forms.
 *
 * The model value is the FRONTMATTER spelling (`encodeSubagentModel`), not the
 * raw `providerId/modelId`: a custom-endpoint provider id contains `/`, which
 * the runtime's own parser would split at, so the encoded form is the only one
 * that round-trips. Both forms therefore store the encoded string and let the
 * option value carry the encoding.
 *
 * `model` may arrive before the options do (the models payload is fetched on
 * open). A selected value that is not in the list would leave the select
 * showing the "inherit" option while the state still names a model, so an
 * unknown selection renders an explicit disabled row instead.
 */
function ModelPickerFields({
  options,
  levels,
  model,
  thoughtLevel,
  onModelChange,
  onThoughtLevelChange,
}: {
  options: ReturnType<typeof modelOptions>;
  levels: string[];
  model: string | undefined;
  thoughtLevel: string | undefined;
  onModelChange: (next: string) => void;
  onThoughtLevelChange: (next: string) => void;
}) {
  const { t } = useTranslation();
  const knownModel =
    model === undefined ||
    options.some((o) => encodeSubagentModel(o.providerId, o.modelId) === model);
  return (
    <>
      <ConfigField
        label={t("zconfig.agentModel")}
        hint={t("zconfig.agentInheritHint")}
      >
        <select
          value={knownModel ? (model ?? "") : ""}
          onChange={(e) => onModelChange(e.target.value)}
          className={configInputClass}
        >
          {!knownModel && (
            <option value="">{t("zconfig.agentModelUnavailable")}</option>
          )}
          <option value="">{t("zconfig.agentModelInherit")}</option>
          {options.map((o) => (
            <option
              key={`${o.providerId}/${o.modelId}`}
              value={encodeSubagentModel(o.providerId, o.modelId)}
            >
              {o.label}
            </option>
          ))}
        </select>
      </ConfigField>

      <ConfigField
        label={t("zconfig.agentThoughtLevel")}
        hint={t("zconfig.agentInheritHint")}
      >
        {levels.length === 0 ? (
          <p className="mt-1 text-xs text-faint">
            {t("zconfig.agentThoughtNone")}
          </p>
        ) : (
          <select
            value={thoughtLevel ?? ""}
            onChange={(e) => onThoughtLevelChange(e.target.value)}
            className={configInputClass}
          >
            <option value="">{t("zconfig.agentModelInherit")}</option>
            {levels.map((l) => (
              <option key={l} value={l}>
                {l}
              </option>
            ))}
          </select>
        )}
      </ConfigField>
    </>
  );
}

/** The eight server colors as swatches; the dash chip means "leave unset". */
function ColorPicker({
  value,
  onChange,
}: {
  value: string;
  onChange: (next: string) => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="mt-2 flex flex-wrap items-center gap-2">
      {AGENT_COLORS.map((c) => (
        <button
          key={c}
          type="button"
          aria-label={c}
          onClick={() => onChange(c)}
          className={`size-7 rounded-full transition ${SWATCH[c]} ${
            value === c ? "ring-2 ring-ink/50" : ""
          }`}
        />
      ))}
      <button
        type="button"
        onClick={() => onChange("")}
        className={`rounded-lg bg-raised px-3 py-1.5 text-xs ${
          value === "" ? "text-ink ring-1 ring-ink/50" : "text-faint"
        }`}
      >
        {t("zconfig.agentColorNone")}
      </button>
    </div>
  );
}
