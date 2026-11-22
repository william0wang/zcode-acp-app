import { pinyin } from "pinyin-pro";

// The mobile config list's search (ADR-0014 §4, the desktop palette's
// sibling): the fixed section rows only — never the user's dynamic resources
// (models, agents, MCP servers). Matching mirrors the desktop matcher
// (src-gpui settings_search.rs) semantics for semantics: normalized substring
// (lowercase, whitespace-stripped) over BOTH locales' labels, full pinyin and
// pinyin initials of the Chinese label, then a sequential-character fallback
// on the labels — so "moxing"/"mx" finds 模型 from either locale.
//
// Pure by injection: no i18n import at module level (its initializer touches
// localStorage, which a node test environment lacks) — callers hand in a
// translate that resolves a key per locale.

export interface ConfigSearchRow {
  /** The list row it names: a ConfigSection id, or "gate" for the switch row. */
  id: string;
  /** The searched i18n key — the row TITLE; hints stay unindexed. */
  key: string;
}

/** The searched rows in list order (the gate switch is a fixed row too). */
export const CONFIG_SEARCH_ROWS: ConfigSearchRow[] = [
  { id: "quota", key: "zconfig.quota" },
  { id: "models", key: "zconfig.models" },
  { id: "skills", key: "zconfig.skills" },
  { id: "mcp", key: "zconfig.mcp" },
  { id: "hooks", key: "zconfig.hooks" },
  { id: "agents", key: "zconfig.agents" },
  { id: "usage", key: "zconfig.usage" },
  { id: "backups", key: "zconfig.backups" },
  { id: "appUpdate", key: "zconfig.appUpdate" },
  { id: "workflows", key: "zconfig.workflows" },
  { id: "gate", key: "zconfig.workflowGate" },
];

/** Lowercase + drop all whitespace (label-side and query-side alike). */
export function normalizeSearchText(s: string): string {
  return s.replace(/\s+/g, "").toLowerCase();
}

/** (full pinyin, initials) of a NORMALIZED label: hanzi contribute their
 * toneless pinyin / first letter, every other character itself — pinyin-pro
 * rides non-hanzi along in both modes, matching the desktop crate. */
export function pinyinSurfaces(label: string): [string, string] {
  const full = pinyin(label, { toneType: "none", type: "array" }).join("");
  const initials = pinyin(label, {
    pattern: "first",
    toneType: "none",
    type: "array",
  }).join("");
  return [full, initials];
}

/** Every query character appears in target, in order (the sequential
 * fallback — "mdl" finds "models"). */
export function sequentialMatch(query: string, target: string): boolean {
  let rest = target;
  for (const c of query) {
    const at = rest.indexOf(c);
    if (at === -1) return false;
    rest = rest.slice(at + c.length);
  }
  return true;
}

export interface SearchSurfaces {
  zh: string;
  en: string;
  pinyinFull: string;
  pinyinInitials: string;
}

/** One row's derived match surfaces, built per search (a dozen short labels —
 * re-deriving beats a cache, same call as the desktop). */
export function labelSurfaces(zh: string, en: string): SearchSurfaces {
  const zhN = normalizeSearchText(zh);
  const [pinyinFull, pinyinInitials] = pinyinSurfaces(zhN);
  return {
    zh: zhN,
    en: normalizeSearchText(en),
    pinyinFull,
    pinyinInitials,
  };
}

/** A normalized query against one row's surfaces: substring on either label
 * or either pinyin surface, else sequential characters on the labels (not on
 * pinyin — "moxng" is a typo, not a strategy). */
export function surfaceMatches(query: string, s: SearchSurfaces): boolean {
  const q = normalizeSearchText(query);
  if (!q) return true;
  return (
    s.zh.includes(q) ||
    s.en.includes(q) ||
    s.pinyinFull.includes(q) ||
    s.pinyinInitials.includes(q) ||
    sequentialMatch(q, s.zh) ||
    sequentialMatch(q, s.en)
  );
}

/**
 * The rows matching a query, list order (stable). An empty query matches
 * everything. `translate` resolves a key per locale — the other language is
 * the alias source, so both are always indexed regardless of the active one.
 */
export function filterConfigRows(
  query: string,
  translate: (key: string, locale: "en" | "zh-CN") => string,
): ConfigSearchRow[] {
  const q = normalizeSearchText(query);
  if (!q) return CONFIG_SEARCH_ROWS;
  return CONFIG_SEARCH_ROWS.filter((row) =>
    surfaceMatches(
      q,
      labelSurfaces(translate(row.key, "zh-CN"), translate(row.key, "en")),
    ),
  );
}
