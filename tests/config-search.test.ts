// @vitest-environment node
// The config list search (ADR-0014 §4): the pure matcher mirrors the desktop
// palette semantics — normalized substring over BOTH locales' labels, full
// pinyin + pinyin initials of the Chinese label, sequential-character
// fallback — and the fixed row list stays healthy against both catalogs.
import { describe, expect, it } from "vitest";
import en from "../src/i18n/en.json";
import zhCN from "../src/i18n/zh-CN.json";
import {
  CONFIG_SEARCH_ROWS,
  filterConfigRows,
  labelSurfaces,
  normalizeSearchText,
  pinyinSurfaces,
  sequentialMatch,
  surfaceMatches,
} from "../src/lib/configSearch";

// Resolve "zconfig.models" against a nested catalog the way i18next does —
// the tests never touch the i18n singleton (its initializer reads
// localStorage, absent under node).
function translate(key: string, locale: "en" | "zh-CN"): string {
  const value = key.split(".").reduce<unknown>(
    (node, part) => (node as Record<string, unknown>)?.[part],
    locale === "en" ? en : zhCN,
  );
  return typeof value === "string" ? value : key;
}

const ids = (query: string) =>
  filterConfigRows(query, translate).map((row) => row.id);

describe("config search matcher", () => {
  it("normalization drops case and whitespace", () => {
    expect(normalizeSearchText("  Mo dels ")).toBe("models");
    expect(normalizeSearchText("套餐 额度")).toBe("套餐额度");
  });

  it("pinyin surfaces cover full and initials, non-hanzi riding along", () => {
    expect(pinyinSurfaces("模型")).toEqual(["moxing", "mx"]);
    // Mixed labels keep their latin part on both surfaces.
    expect(pinyinSurfaces("osc52模型")).toEqual(["osc52moxing", "osc52mx"]);
  });

  it("sequential matches in-order characters only", () => {
    expect(sequentialMatch("mdl", "models")).toBe(true);
    expect(sequentialMatch("mdx", "models")).toBe(false);
    expect(sequentialMatch("lmd", "models")).toBe(false);
  });

  it("matches substring, pinyin, initials, and the alias locale", () => {
    // English substring and Chinese label substring.
    expect(ids("model")).toContain("models");
    expect(ids("模型")).toContain("models");
    // Full pinyin and initials (desktop acceptance scenario 3).
    expect(ids("moxing")).toContain("models");
    expect(ids("mx")).toContain("models");
    // The en label doubles as the alias from the zh side, and vice versa.
    expect(ids("quota")).toContain("quota");
    expect(ids("taocan")).toContain("quota");
    expect(ids("gzl")).toContain("workflows");
    expect(ids("zidaili")).toContain("agents");
    // The gate row is a fixed row and searchable like any section.
    expect(ids("开关")).toContain("gate");
    expect(ids("switch")).toContain("gate");
  });

  it("an empty query lists every row and a miss lists none", () => {
    expect(ids("")).toHaveLength(CONFIG_SEARCH_ROWS.length);
    expect(ids("zzzz")).toEqual([]);
  });

  it("every searched key resolves in both catalogs", () => {
    for (const row of CONFIG_SEARCH_ROWS) {
      for (const locale of ["en", "zh-CN"] as const) {
        const label = translate(row.key, locale);
        expect(label, `${row.key} in ${locale}`).not.toBe(row.key);
        expect(label.length).toBeGreaterThan(0);
      }
    }
  });

  it("surfaces match a query built from either direction", () => {
    // labelSurfaces is the single place both labels meet; spot-check the
    // sequential fallback rides on labels but never on pinyin.
    const s = labelSurfaces("模型", "Models");
    expect(surfaceMatches("moxng", s)).toBe(false);
    expect(surfaceMatches("mo", s)).toBe(true);
  });
});
