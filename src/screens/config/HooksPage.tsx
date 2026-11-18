import { useState } from "react";
import { useTranslation } from "react-i18next";
import { ChevronDown, ChevronRight } from "lucide-react";
import { useAppStore } from "../../store/appStore";
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
import type { HookEntryPatch } from "../../lib/types";

// Hooks (ADR-0009): the seven event names, each with the entries the user
// already configured. Adding an event or a matcher is deliberately not
// offered — the bridge's write route only edits an EXISTING entry's command,
// timeout and enabled flag, and a "new hook" button would produce a request
// the API refuses. The edit covers the real remote use cases: temporarily
// disable one hook, tweak its command, adjust its timeout.
//
// The payload nests two levels: `hooks.events` is a Record keyed by event
// name, whose value is a list of matcher GROUPS, and each group carries its
// own list of hook entries. So one event can hold several matchers and each
// matcher several commands — the screen flattens that into rows and keeps the
// matcher as a label, because that is what the user actually configured.
//
// Each entry expands in place rather than opening a third screen: the form is
// three fields, and a drill-down for that is more navigation than content.

/** One flattened row: an event, its matcher group, and one hook inside it. */
export interface HookRow {
  /** `{event}/{matcherIndex}/{hookIndex}` — display key for the row. */
  key: string;
  event: string;
  /** The three coordinates `PUT /settings/hooks/{event}/{matcherIndex}` needs. */
  matcherIndex: number;
  hookIndex: number;
  matcher: string;
  /** `"process"` or `"command"` — decides which timeout unit a NEW timeout gets. */
  type?: string;
  command: string;
  enabled: boolean;
  /** Raw `timeoutMs` (milliseconds) when the entry spells it; wins over `timeout`. */
  rawTimeoutMs?: number;
  /** Raw `timeout` (seconds) when the entry spells it. */
  rawTimeoutSec?: number;
}

/**
 * Flatten the two-level hooks tree into editable rows.
 *
 * Exported so the response-shape test asserts against the REAL decode. A
 * copied version that drifted is how this screen once showed an empty list on
 * a machine with hooks configured while every test passed.
 *
 * The nesting is `events[event]` → matcher groups → hook entries, so one event
 * can hold several matchers and each matcher several commands. The indexes
 * ride along because the edit route addresses an entry by all three.
 *
 * The two timeout spellings are kept RAW rather than normalized: the edit form
 * writes back in the unit the entry already uses, so editing never adds a
 * second timeout field alongside the first.
 */
export function flattenHooks(
  events: Record<string, Array<{ matcher?: string; hooks?: unknown[] }>>,
): HookRow[] {
  const rows: HookRow[] = [];
  for (const [event, groups] of Object.entries(events)) {
    (groups ?? []).forEach((group, matcherIndex) => {
      (group.hooks ?? []).forEach((entry, hookIndex) => {
        const h = entry as {
          type?: string;
          command?: string;
          enabled?: boolean;
          timeoutMs?: number;
          timeout?: number;
        };
        rows.push({
          key: `${event}/${matcherIndex}/${hookIndex}`,
          event,
          matcherIndex,
          hookIndex,
          matcher: group.matcher ?? "",
          type: h.type,
          command: h.command ?? "",
          // Absent means on: the config spells out `enabled: false` only when
          // a hook is switched off.
          enabled: h.enabled !== false,
          rawTimeoutMs: h.timeoutMs,
          rawTimeoutSec: h.timeout,
        });
      });
    });
  }
  return rows;
}

/** The entry's timeout in milliseconds, whichever spelling it carries. */
export function timeoutMsOf(row: HookRow): number | undefined {
  return (
    row.rawTimeoutMs ??
    (row.rawTimeoutSec != null ? row.rawTimeoutSec * 1000 : undefined)
  );
}

/**
 * Whether the hooks tree is live.
 *
 * The gate is surfaced twice — the route's own top-level field and the copy
 * nested in the config — and either being false means hooks are inert. The
 * route always writes its field, so it is the authority; the nested one is the
 * fallback for an older build.
 *
 * Absent means OFF, not on: `setHooksEnabled(false)` DELETES the key rather
 * than storing false, and the runtime treats a missing `enabled` as disabled.
 * Reading it as `!== false` would show the tree as live while every hook sits
 * inert.
 */
export function hooksEnabled(
  payload: {
    enabled?: boolean;
    hooks?: { enabled?: boolean };
  } | null,
): boolean {
  return payload?.enabled ?? payload?.hooks?.enabled === true;
}

export function HooksPage() {
  const { t } = useTranslation();
  const hooks = useAppStore((s) => s.configHooks);
  const supported = useAppStore((s) => s.configSupported);
  const loading = useAppStore((s) => s.configLoading);
  const error = useAppStore((s) => s.configError);
  const loadConfigSection = useAppStore((s) => s.loadConfigSection);
  const applyConfigWrite = useAppStore((s) => s.applyConfigWrite);
  const setHooksEnabled = useAppStore((s) => s.setHooksEnabled);
  const updateHookEntry = useAppStore((s) => s.updateHookEntry);
  const [open, setOpen] = useState<string | null>(null);
  const [editing, setEditing] = useState<HookRow | null>(null);

  const payload = hooks as {
    ok?: boolean;
    enabled?: boolean;
    hooks?: {
      enabled?: boolean;
      events?: Record<string, Array<{ matcher?: string; hooks?: unknown[] }>>;
    };
  } | null;

  const rows = flattenHooks(payload?.hooks?.events ?? {});
  // Group by event for the collapsible list, preserving the server's order.
  const byEvent = new Map<string, HookRow[]>();
  for (const row of rows) {
    const list = byEvent.get(row.event) ?? [];
    list.push(row);
    byEvent.set(row.event, list);
  }
  const enabled = hooksEnabled(payload);

  // One write per flip, needs-restart — the same class as every other hook
  // edit on this screen. `enabled: true` removes the pin server-side, so the
  // two directions are not symmetric on disk but read the same here.
  async function toggleHook(row: HookRow, enable: boolean) {
    await applyConfigWrite(
      enable ? "enable hook" : "disable hook",
      () =>
        updateHookEntry(row.event, row.matcherIndex, row.hookIndex, {
          enabled: enable,
        }),
      ["hooks"],
    );
  }

  return (
    <ConfigPageFrame
      title={t("zconfig.hooks")}
      onBack={() => useAppStore.getState().openConfig(null)}
      onRefresh={() => void loadConfigSection("hooks")}
      refreshing={loading}
      unsupported={supported === false}
      error={error}
      hasLoaded={hooks !== null}
    >
      {rows.length === 0 ? (
        <ConfigEmpty text={t("zconfig.hooksEmpty")} />
      ) : (
        <>
          {/* The whole tree's switch. One write, needs-restart — the bridge
              re-reads hooks only when its backend respawns. */}
          <div className="flex items-center gap-3 px-4 py-3">
            <span className="flex-1 text-sm text-dim">
              {t("zconfig.hooksAll")}
            </span>
            <button
              role="switch"
              aria-checked={enabled}
              aria-label={t("zconfig.hooksAll")}
              onClick={() =>
                void applyConfigWrite(
                  enabled ? "disable hooks" : "enable hooks",
                  () => setHooksEnabled(!enabled),
                  ["hooks"],
                )
              }
              className={`h-6 w-10 shrink-0 rounded-full transition ${
                enabled ? "bg-success/80" : "bg-active"
              }`}
            >
              <span
                className={`block size-5 rounded-full bg-white transition ${
                  enabled ? "translate-x-4.5" : "translate-x-0.5"
                }`}
              />
            </button>
          </div>

          {[...byEvent].map(([event, entries]) => {
            const isOpen = open === event;
            return (
              <ConfigBlock key={event} divided>
                <button
                  onClick={() => setOpen(isOpen ? null : event)}
                  className="flex w-full items-center gap-2 px-4 py-3 text-left hover:bg-hover active:bg-active"
                >
                  {isOpen ? (
                    <ChevronDown className="size-4 shrink-0 text-faint" />
                  ) : (
                    <ChevronRight className="size-4 shrink-0 text-faint" />
                  )}
                  <span className="min-w-0 flex-1">
                    <span className="block font-mono text-sm text-ink">
                      {event}
                    </span>
                    <span className="block text-2xs text-faint">
                      {t("zconfig.hooksEntryCount", { count: entries.length })}
                    </span>
                  </span>
                </button>

                {isOpen &&
                  (entries.length === 0 ? (
                    <p className="px-4 pb-3 text-xs text-faint">
                      {t("zconfig.hooksNoEntries")}
                    </p>
                  ) : (
                    <ul className="pb-2">
                      {entries.map((h) => {
                        const ms = timeoutMsOf(h);
                        return (
                          <li
                            key={h.key}
                            className="mx-4 mb-1.5 flex items-start gap-2 rounded-lg bg-hover py-2 pl-3 pr-2"
                          >
                            <button
                              onClick={() => setEditing(h)}
                              className="min-w-0 flex-1 text-left active:opacity-80"
                            >
                              <div className="flex items-baseline justify-between gap-2">
                                <span className="font-mono text-2xs text-dim">
                                  {h.matcher || t("zconfig.hooksAnyMatcher")}
                                </span>
                                <span
                                  className={`shrink-0 text-2xs ${
                                    h.enabled ? "text-faint" : "text-warning"
                                  }`}
                                >
                                  {h.enabled
                                    ? ms != null
                                      ? `${ms}ms`
                                      : ""
                                    : t("zconfig.disabled")}
                                </span>
                              </div>
                              <p className="pt-0.5 font-mono text-2xs break-all text-faint">
                                {h.command}
                              </p>
                            </button>
                            <button
                              role="switch"
                              aria-checked={h.enabled}
                              aria-label={t("zconfig.hookEnabled")}
                              onClick={() => void toggleHook(h, !h.enabled)}
                              className={`mt-0.5 h-6 w-10 shrink-0 rounded-full transition ${
                                h.enabled
                                  ? "bg-success/80"
                                  : "bg-active"
                              }`}
                            >
                              <span
                                className={`block size-5 rounded-full bg-white transition ${
                                  h.enabled
                                    ? "translate-x-4.5"
                                    : "translate-x-0.5"
                                }`}
                              />
                            </button>
                          </li>
                        );
                      })}
                    </ul>
                  ))}
              </ConfigBlock>
            );
          })}

          <p className="px-4 pt-3 text-center text-2xs text-faint">
            {t("zconfig.hooksReadOnly", { count: rows.length })}
          </p>
        </>
      )}

      {editing && (
        <HookForm
          key={editing.key}
          target={editing}
          onClose={() => setEditing(null)}
        />
      )}
    </ConfigPageFrame>
  );
}

/**
 * Edit sheet for ONE existing hook entry.
 *
 * The bridge deliberately offers no add/remove (an insert shifts every later
 * index and races a concurrent edit of the same event), so the sheet edits
 * what the entry already carries: its command and its timeout.
 *
 * The timeout field works in SECONDS — the desktop form's unit — and writes
 * back in the unit the entry already spells, so editing never adds a second
 * timeout field next to the first. An entry with no timeout gets the unit its
 * type implies: `process` entries carry `timeoutMs`, `command` entries
 * `timeout` seconds. Blank means unchanged.
 */
function HookForm({
  target,
  onClose,
}: {
  target: HookRow;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const applyConfigWrite = useAppStore((s) => s.applyConfigWrite);
  const updateHookEntry = useAppStore((s) => s.updateHookEntry);

  const [command, setCommand] = useState(target.command);
  const [timeoutText, setTimeoutText] = useState(() => {
    const ms = timeoutMsOf(target);
    return ms != null ? String(ms / 1000) : "";
  });
  const [busy, setBusy] = useState(false);

  // The server accepts any positive number in either unit, but not zero or a
  // non-numeric string — and an empty command is a malformed request outright.
  const seconds = timeoutText.trim();
  const timeoutOk =
    seconds === "" || (Number(seconds) > 0 && Number.isFinite(Number(seconds)));
  const canSubmit = command.trim() !== "" && timeoutOk;

  async function submit() {
    setBusy(true);
    // Write back in the entry's own spelling; a timeout-less entry takes the
    // unit its type uses on disk (process → ms, command → seconds).
    const msSpelled =
      target.rawTimeoutMs != null ||
      (target.rawTimeoutMs == null &&
        target.rawTimeoutSec == null &&
        target.type !== "command");
    const patch: HookEntryPatch = {
      command: command.trim(),
      ...(seconds !== ""
        ? msSpelled
          ? { timeoutMs: Number(seconds) * 1000 }
          : { timeout: Number(seconds) }
        : {}),
    };
    const ok = await applyConfigWrite(
      "update hook",
      () =>
        updateHookEntry(
          target.event,
          target.matcherIndex,
          target.hookIndex,
          patch,
        ),
      ["hooks"],
    );
    setBusy(false);
    if (ok) onClose();
  }

  return (
    <ConfigFormSheet
      title={t("zconfig.hookEdit")}
      busy={busy}
      submitDisabled={!canSubmit}
      onSubmit={() => void submit()}
      onClose={onClose}
    >
      <p className="mt-3 font-mono text-2xs text-faint">
        {target.event} · {target.matcher || t("zconfig.hooksAnyMatcher")}
      </p>

      <ConfigField label={t("zconfig.hookCommand")}>
        <input
          value={command}
          onChange={(e) => setCommand(e.target.value)}
          autoCapitalize="none"
          autoCorrect="off"
          spellCheck={false}
          className={configInputClass}
        />
      </ConfigField>

      <ConfigField
        label={t("zconfig.hookTimeout")}
        hint={t("zconfig.hookTimeoutHint")}
      >
        <input
          value={timeoutText}
          onChange={(e) => setTimeoutText(e.target.value)}
          inputMode="decimal"
          autoCapitalize="none"
          autoCorrect="off"
          spellCheck={false}
          placeholder="60"
          className={configInputClass}
        />
      </ConfigField>
    </ConfigFormSheet>
  );
}
