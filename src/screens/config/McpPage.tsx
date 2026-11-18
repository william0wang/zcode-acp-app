import { useState } from "react";
import { useTranslation } from "react-i18next";
import { ChevronRight, Plus } from "lucide-react";
import { useAppStore } from "../../store/appStore";
import type { McpServerUpsert } from "../../lib/types";
import {
  ConfigBlock,
  ConfigEmpty,
  ConfigPageFrame,
} from "../../components/config/ConfigPage";
import {
  ConfigField,
  ConfigFormSheet,
  ConfigToggle,
  configInputClass,
  kvToLines,
  parseKvLines,
  splitLines,
} from "../../components/config/ConfigFormSheet";

// MCP servers (ADR-0009). Every write here is `needs-restart` — the bridge has
// to respawn its backend for a server list change to reach the agent — which
// `applyConfigWrite` surfaces and the restart affordance on the entry screen
// resolves.
//
// Adding and editing share `PUT /settings/mcp/{name}`, which MERGES the body
// into the existing entry. The form therefore sends only the fields its type
// actually uses (a stdio body never touches url/headers, and vice versa) and
// lets the server keep whatever else the entry carried.

/** One configured server as the bridge reports it (`readMcpView`). */
export interface McpServerRow {
  name?: string;
  type?: string;
  command?: string;
  args?: string[];
  url?: string;
  env?: Record<string, string>;
  headers?: Record<string, string>;
  enabled?: boolean;
}

/**
 * The server list out of the route's payload.
 *
 * The route wraps it (`{ok, mcp:{servers}}`). Reading `servers` off the top
 * level yields undefined, `.length` is 0, and the page renders its empty state
 * on a machine that has servers configured — indistinguishable from a genuinely
 * empty list. Exported so the response-shape test asserts the REAL decode.
 */
export function mcpServers(payload: unknown): McpServerRow[] {
  const wrapped = payload as { mcp?: { servers?: McpServerRow[] } } | null;
  return wrapped?.mcp?.servers ?? [];
}

/** Transports the form offers; anything else an entry carries rides along. */
const TYPES = ["stdio", "http", "sse"];

/**
 * The PUT body for one server, built from the form's raw fields.
 *
 * A body carries ONLY its transport's fields: the route MERGES, so a `url`
 * landing on a stdio server (or a `command` on an http one) is written next to
 * the real transport instead of replacing it. An EMPTY env/headers map is
 * omitted, matching the desktop form (whose JSON textarea drops a field that
 * does not parse): a blank textarea means "keep what the entry had", while
 * sending `{}` would clear it — or shadow a legacy `http_headers` block the
 * runtime merges with `??`.
 */
export function mcpUpsertBody(f: {
  type: string;
  enabled: boolean;
  remote: boolean;
  url: string;
  headersText: string;
  command: string;
  argsText: string;
  envText: string;
}): McpServerUpsert {
  const kv = parseKvLines(f.remote ? f.headersText : f.envText);
  const kvPresent = Object.keys(kv).length > 0;
  return {
    type: f.type,
    enabled: f.enabled,
    ...(f.remote
      ? {
          url: f.url.trim(),
          ...(kvPresent ? { headers: kv } : {}),
        }
      : {
          command: f.command.trim(),
          // Blank args ARE sent as `[]` — the desktop form's own spelling for
          // "this server takes no arguments" — unlike env, which omits.
          args: splitLines(f.argsText),
          ...(kvPresent ? { env: kv } : {}),
        }),
  };
}

export function McpPage() {
  const { t } = useTranslation();
  const mcp = useAppStore((s) => s.configMcp);
  const supported = useAppStore((s) => s.configSupported);
  const loading = useAppStore((s) => s.configLoading);
  const error = useAppStore((s) => s.configError);
  const loadConfigSection = useAppStore((s) => s.loadConfigSection);

  const rows = mcpServers(mcp);

  const [editing, setEditing] = useState<McpServerRow | null>(null);
  const [adding, setAdding] = useState(false);

  return (
    <ConfigPageFrame
      title={t("zconfig.mcp")}
      onBack={() => useAppStore.getState().openConfig(null)}
      onRefresh={() => void loadConfigSection("mcp")}
      refreshing={loading}
      unsupported={supported === false}
      error={error}
      hasLoaded={mcp !== null}
    >
      {rows.length === 0 ? (
        <ConfigEmpty text={t("zconfig.mcpEmpty")} />
      ) : (
        <ConfigBlock>
          {rows.map((s) => {
            const name = s.name ?? "";
            return (
              <button
                key={name}
                onClick={() => setEditing(s)}
                className="flex w-full items-center gap-3 px-4 py-3 text-left hover:bg-hover active:bg-active"
              >
                <span className="min-w-0 flex-1">
                  <span className="block text-sm text-ink">{name}</span>
                  <span className="block truncate font-mono text-2xs text-faint">
                    {s.command
                      ? [s.command, ...(s.args ?? [])].join(" ")
                      : (s.url ?? s.type ?? "")}
                  </span>
                  {s.enabled === false && (
                    <span className="block text-2xs text-faint">
                      {t("zconfig.disabled")}
                    </span>
                  )}
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
          {t("zconfig.mcpAdd")}
        </button>
      </div>

      {editing && (
        <McpForm
          key={editing.name}
          target={editing}
          onClose={() => setEditing(null)}
        />
      )}
      {adding && (
        <McpForm key="add" target={null} onClose={() => setAdding(false)} />
      )}
    </ConfigPageFrame>
  );
}

/**
 * Add/edit form for one MCP server.
 *
 * The name is the entry's identity (it travels in the PUT path), so it is
 * editable only while adding — the server rule is non-empty, no path
 * separators, mirrored here so the error never has to make the round trip.
 */
function McpForm({
  target,
  onClose,
}: {
  target: McpServerRow | null;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const applyConfigWrite = useAppStore((s) => s.applyConfigWrite);
  const upsertMcp = useAppStore((s) => s.upsertMcp);

  const [name, setName] = useState(target?.name ?? "");
  // An unknown transport type is kept verbatim (and shown as its own option
  // below) so the write never silently downgrades a server this build has not
  // heard of. Only the two remote transports are folded into the http branch.
  const initialType = target?.type ?? "stdio";
  const [type, setType] = useState(initialType);
  const [command, setCommand] = useState(target?.command ?? "");
  const [argsText, setArgsText] = useState((target?.args ?? []).join("\n"));
  const [envText, setEnvText] = useState(kvToLines(target?.env));
  const [url, setUrl] = useState(target?.url ?? "");
  const [headersText, setHeadersText] = useState(kvToLines(target?.headers));
  const [enabled, setEnabled] = useState(target?.enabled !== false);
  const [busy, setBusy] = useState(false);

  const nameOk = name.trim() !== "" && !/[\\/]/.test(name.trim());
  const remote = type !== "stdio";
  const typeOk = remote ? url.trim() !== "" : command.trim() !== "";
  const canSubmit = nameOk && typeOk;

  async function submit() {
    setBusy(true);
    const ok = await applyConfigWrite(
      target ? "update MCP server" : "add MCP server",
      () =>
        upsertMcp(
          name.trim(),
          mcpUpsertBody({
            type,
            enabled,
            remote,
            url,
            headersText,
            command,
            argsText,
            envText,
          }),
        ),
      ["mcp"],
    );
    setBusy(false);
    if (ok) onClose();
  }

  return (
    <ConfigFormSheet
      title={target ? t("zconfig.mcpEdit") : t("zconfig.mcpAdd")}
      busy={busy}
      submitDisabled={!canSubmit}
      onSubmit={() => void submit()}
      onClose={onClose}
    >
      <ConfigField
        label={t("zconfig.mcpName")}
        hint={target ? undefined : t("zconfig.mcpNameHint")}
      >
        {target ? (
          <p className="mt-1 font-mono text-sm text-ink">{target.name}</p>
        ) : (
          <input
            value={name}
            onChange={(e) => setName(e.target.value)}
            autoCapitalize="none"
            autoCorrect="off"
            spellCheck={false}
            className={configInputClass}
          />
        )}
      </ConfigField>

      <ConfigField label={t("zconfig.mcpType")}>
        <select
          value={type}
          onChange={(e) => setType(e.target.value)}
          className={configInputClass}
        >
          {/* An entry with a type this build does not know stays selectable
              and unchanged unless the user actively picks another. */}
          {target?.type && !TYPES.includes(target.type) && (
            <option value={target.type}>{target.type}</option>
          )}
          {TYPES.map((v) => (
            <option key={v} value={v}>
              {v}
            </option>
          ))}
        </select>
      </ConfigField>

      {/* Remote transports (http/sse) edit url/headers; stdio edits
          command/args/env. The branches must mirror `mcpUpsertBody` — the
          fields the user SEES are the fields the write sends. */}
      {remote ? (
        <>
          <ConfigField label={t("zconfig.mcpUrl")}>
            <input
              value={url}
              onChange={(e) => setUrl(e.target.value)}
              inputMode="url"
              autoCapitalize="none"
              autoCorrect="off"
              spellCheck={false}
              placeholder="https://example.com/mcp"
              className={configInputClass}
            />
          </ConfigField>
          <ConfigField
            label={t("zconfig.mcpHeaders")}
            hint={t("zconfig.kvPerLine")}
          >
            <textarea
              value={headersText}
              onChange={(e) => setHeadersText(e.target.value)}
              rows={3}
              autoCapitalize="none"
              autoCorrect="off"
              spellCheck={false}
              className={configInputClass}
            />
          </ConfigField>
        </>
      ) : (
        <>
          <ConfigField label={t("zconfig.mcpCommand")}>
            <input
              value={command}
              onChange={(e) => setCommand(e.target.value)}
              autoCapitalize="none"
              autoCorrect="off"
              spellCheck={false}
              placeholder="npx"
              className={configInputClass}
            />
          </ConfigField>
          <ConfigField label={t("zconfig.mcpArgs")} hint={t("zconfig.perLine")}>
            <textarea
              value={argsText}
              onChange={(e) => setArgsText(e.target.value)}
              rows={3}
              autoCapitalize="none"
              autoCorrect="off"
              spellCheck={false}
              className={configInputClass}
            />
          </ConfigField>
          <ConfigField
            label={t("zconfig.mcpEnv")}
            hint={t("zconfig.kvPerLine")}
          >
            <textarea
              value={envText}
              onChange={(e) => setEnvText(e.target.value)}
              rows={3}
              autoCapitalize="none"
              autoCorrect="off"
              spellCheck={false}
              className={configInputClass}
            />
          </ConfigField>
        </>
      )}

      <div className="mt-4 flex items-center justify-between">
        <span className="text-xs font-medium text-dim">
          {t("zconfig.enabled")}
        </span>
        <ConfigToggle
          checked={enabled}
          label={t("zconfig.mcpEnabled")}
          onChange={setEnabled}
        />
      </div>
    </ConfigFormSheet>
  );
}
