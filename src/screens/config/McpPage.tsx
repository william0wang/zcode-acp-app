import { useEffect, useState } from "react";
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
  FormDeleteButton,
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

/** One server's live health out of the mcp-status body. */
export interface McpServerHealth {
  status?: string;
  toolCount?: number;
  failureKind?: string;
  authorizationUrl?: string;
}

/**
 * The per-instance mcp-status body (the bridge's proxy of the backend
 * `mcp/list` mode:"status" RPC). `available:false` is NOT an error — an
 * older backend or a timed-out RPC means "status unknown", and the page
 * renders that hint instead of failing. Exported so the response-shape test
 * asserts the REAL decode.
 */
export function mcpStatuses(
  payload: unknown,
): { available: boolean; statuses: Record<string, McpServerHealth> } {
  const body = payload as {
    available?: boolean;
    statuses?: Record<string, McpServerHealth>;
  } | null;
  return { available: body?.available === true, statuses: body?.statuses ?? {} };
}

/** Known backend verdicts localize; an unknown spelling renders verbatim. */
const STATUS_LABEL_KEYS: Record<string, string> = {
  connected: "zconfig.mcpStatusConnected",
  connecting: "zconfig.mcpStatusConnecting",
  disconnected: "zconfig.mcpStatusDisconnected",
  disabled: "zconfig.disabled",
  failed: "zconfig.mcpStatusFailed",
  untrusted: "zconfig.mcpStatusUntrusted",
};

/** The dot's color family — the same tones the desktop row paints. */
function statusTone(status: string): string {
  switch (status) {
    case "connected":
      return "bg-success";
    case "failed":
      return "bg-danger";
    case "untrusted":
      return "bg-warning";
    case "connecting":
      return "bg-info";
    default:
      return "bg-faint";
  }
}

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
  const mcpStatus = useAppStore((s) => s.configMcpStatus);
  const supported = useAppStore((s) => s.configSupported);
  const loading = useAppStore((s) => s.configLoading);
  const error = useAppStore((s) => s.configError);
  const loadConfigSection = useAppStore((s) => s.loadConfigSection);
  const loadMcpStatus = useAppStore((s) => s.loadMcpStatus);

  const rows = mcpServers(mcp);
  const { available, statuses } = mcpStatuses(mcpStatus);

  // Best-effort dots: probed on every mount (and via the header refresh) so
  // an enable flip's aftermath is visible without re-entering the page.
  useEffect(() => {
    void loadMcpStatus();
  }, [loadMcpStatus]);

  const [editing, setEditing] = useState<McpServerRow | null>(null);
  const [adding, setAdding] = useState(false);

  return (
    <ConfigPageFrame
      title={t("zconfig.mcp")}
      onBack={() => useAppStore.getState().backFromConfigSection()}
      onRefresh={() => {
        void loadConfigSection("mcp");
        void loadMcpStatus();
      }}
      refreshing={loading}
      unsupported={supported === false}
      error={error}
      hasLoaded={mcp !== null}
    >
      {mcpStatus !== null && !available && (
        <p className="px-4 pt-2 text-2xs text-faint">
          {t("zconfig.mcpStatusUnavailable")}
        </p>
      )}
      {rows.length === 0 ? (
        <ConfigEmpty text={t("zconfig.mcpEmpty")} />
      ) : (
        <ConfigBlock>
          {rows.map((s) => {
            const name = s.name ?? "";
            const health = statuses[name];
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
                  {health?.status && (
                    <span className="mt-0.5 flex items-center gap-1.5 text-2xs text-faint">
                      <span
                        className={`size-1.5 shrink-0 rounded-full ${statusTone(health.status)}`}
                      />
                      <span>
                        {STATUS_LABEL_KEYS[health.status]
                          ? t(STATUS_LABEL_KEYS[health.status])
                          : health.status}
                      </span>
                      {!!health.toolCount &&
                        t("zconfig.mcpTools", { count: health.toolCount })}
                      {health.failureKind && (
                        <span className="truncate font-mono">
                          {health.failureKind}
                        </span>
                      )}
                    </span>
                  )}
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
  const removeMcp = useAppStore((s) => s.removeMcp);

  // An unknown transport type is kept verbatim (and shown as its own option
  // below) so the write never silently downgrades a server this build has not
  // heard of. Only the two remote transports are folded into the http branch.
  const initialType = target?.type ?? "stdio";
  const initialCommand = target?.command ?? "";
  const initialArgs = (target?.args ?? []).join("\n");
  const initialEnv = kvToLines(target?.env);
  const initialUrl = target?.url ?? "";
  const initialHeaders = kvToLines(target?.headers);
  const initialEnabled = target?.enabled !== false;

  const [name, setName] = useState(target?.name ?? "");
  const [type, setType] = useState(initialType);
  const [command, setCommand] = useState(initialCommand);
  const [argsText, setArgsText] = useState(initialArgs);
  const [envText, setEnvText] = useState(initialEnv);
  const [url, setUrl] = useState(initialUrl);
  const [headersText, setHeadersText] = useState(initialHeaders);
  const [enabled, setEnabled] = useState(initialEnabled);
  const [busy, setBusy] = useState(false);

  const nameOk = name.trim() !== "" && !/[\\/]/.test(name.trim());
  const remote = type !== "stdio";
  const typeOk = remote ? url.trim() !== "" : command.trim() !== "";
  const canSubmit = nameOk && typeOk;
  // The form contract's dirty gate: a create is dirty whenever valid; an
  // edit compares every field against the opened values.
  const dirty = !target
    ? canSubmit
    : type !== initialType ||
      command.trim() !== initialCommand.trim() ||
      argsText !== initialArgs ||
      envText !== initialEnv ||
      url.trim() !== initialUrl.trim() ||
      headersText !== initialHeaders ||
      enabled !== initialEnabled;

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

  // The edit form's footer delete (the form contract) — the config entry is
  // gone with no undo, so it stays two-tap.
  async function remove() {
    const name = target?.name;
    if (!target || !name) return;
    setBusy(true);
    const ok = await applyConfigWrite(
      "delete MCP server",
      () => removeMcp(name),
      ["mcp"],
    );
    setBusy(false);
    if (ok) onClose();
  }

  return (
    <ConfigFormSheet
      title={target ? t("zconfig.mcpEdit") : t("zconfig.mcpAdd")}
      busy={busy}
      submitDisabled={!canSubmit || !dirty}
      dirty={dirty}
      footerLeft={
        target ? (
          <FormDeleteButton
            label={t("zconfig.mcpDelete")}
            busy={busy}
            onConfirm={() => void remove()}
          />
        ) : undefined
      }
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
            autoFocus
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
              autoFocus={!!target}
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
              autoFocus={!!target}
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
