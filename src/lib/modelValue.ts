// Session configOption values spell a model two ways: the bridge's
// `providerId\modelId` (backslash) and the agent-definition
// `custom:<encoded provider>:<model>`. A current value that missed the option
// list must still read as a name — dumping the raw pair is how the same model
// came to display as a full id string in one place and a name in another.

/** The bare model id out of a configOption value, whichever spelling it uses. */
export function bareModelIdFromConfigValue(value: string): string {
  const custom = /^custom:([^:]+):(.+)$/.exec(value);
  if (custom) {
    try {
      return decodeURIComponent(custom[2]!);
    } catch {
      return custom[2]!;
    }
  }
  const idx = value.lastIndexOf("\\");
  return idx >= 0 ? value.slice(idx + 1) : value;
}

/**
 * The provider id as a MATCH key: coding-plan ids normalize to the legacy
 * `builtin:` spelling. The bridge builds the option values from config.json
 * while the runtime may push a mid-turn currentValue in the registry's
 * `account:` spelling — legal wire data, just a different dialect of the same
 * provider. Display-side matching normalizes so the choice name (and the
 * picker's checkmark) survive the push; the wire value itself stays as sent.
 * (Port of the desktop workbench's `config_value_match_key` companion,
 * `matchProviderId` — same regex as the config pages' join.)
 */
export function matchProviderId(providerId: string | undefined): string {
  // Undefined (a rule without a provider id) collapses to "" so two undefined
  // ids compare equal rather than to everything.
  if (!providerId?.startsWith("account:")) return providerId ?? "";
  const m =
    /^account:([a-z0-9]+)-(?:(?:individual|team|start)-)?coding-plan$/.exec(
      providerId,
    );
  return m ? `builtin:${m[1]}-coding-plan` : providerId;
}

/**
 * A config-option value as a MATCH key: the provider spelling normalized
 * (`account:…` → `builtin:…`), the model part untouched. Model ids never
 * carry `\`; the legacy bare spelling (and `custom:…`) compares as-is.
 */
export function configValueMatchKey(value: string): string {
  const idx = value.lastIndexOf("\\");
  if (idx < 0) return value;
  return `${matchProviderId(value.slice(0, idx))}\\${value.slice(idx + 1)}`;
}

/** The choice a configOption's currentValue names: exact, else under the
 *  provider-normalized match key — the option's NAME wins over any raw
 *  spelling (desktop workbench `config_option_view` parity). */
export function currentChoice<T extends { value: string }>(
  option: { options?: T[] | null; currentValue?: string | null },
): T | undefined {
  const choices = option.options;
  if (!choices || !option.currentValue) return undefined;
  const exact = choices.find((v) => v.value === option.currentValue);
  if (exact) return exact;
  const key = configValueMatchKey(option.currentValue);
  return choices.find((v) => configValueMatchKey(v.value) === key);
}
