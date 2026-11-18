// i18n catalog parity check (requirements 1.11 / 2.15): the two catalogs
// (`src/i18n/en.json`, `src/i18n/zh-CN.json`) are compiled into BOTH clients
// (TS i18next + the desktop's rust-i18n embed), so key drift surfaces as
// leaked keys or leaked English long after the edit that caused it.
//
// Rules:
//   1. identical flattened key sets across locales;
//   2. count keys pluralize ONLY through `_one`/`_other` pairs — a bare base
//      key next to suffixed siblings, or a `_one` without its `_other`
//      (either direction), is rejected;
//   3. every key interpolates the same `{{placeholder}}` set in both locales
//      (rust-i18n's `fill` and i18next both replace blindly, so a missing
//      placeholder renders literally).
//
// Run via `pnpm i18n:check` (no deps — plain node).

import { readFileSync } from "node:fs";

const CATALOGS = ["en", "zh-CN"];
const dir = new URL("../src/i18n/", import.meta.url);

/** Flattens nested objects to dotted i18next key paths. */
function flatten(obj, prefix = "") {
  const out = {};
  for (const [k, v] of Object.entries(obj)) {
    const key = prefix ? `${prefix}.${k}` : k;
    if (v && typeof v === "object") Object.assign(out, flatten(v, key));
    else out[key] = v;
  }
  return out;
}

function placeholders(value) {
  return [...String(value).matchAll(/\{\{\s*(\w+)\s*\}\}/g)].map((m) => m[1]).sort();
}

const catalogs = Object.fromEntries(
  CATALOGS.map((locale) => {
    const raw = JSON.parse(readFileSync(new URL(`${locale}.json`, dir), "utf8"));
    return [locale, flatten(raw)];
  }),
);

const problems = [];

// 1. Key parity.
const keySets = CATALOGS.map((l) => new Set(Object.keys(catalogs[l])));
for (const [locale, keys] of keySets.entries()) {
  for (const other of keySets.keys()) {
    if (other === locale) continue;
    const missing = [...keys].filter((k) => !keySets[other].has(k));
    for (const key of missing) {
      problems.push(`${CATALOGS[other]}: missing key "${key}" (present in ${CATALOGS[locale]})`);
    }
  }
}

// 2. Plural family shape, per locale.
for (const locale of CATALOGS) {
  for (const key of Object.keys(catalogs[locale])) {
    const base = key.replace(/_(one|other)$/, "");
    if (base === key) {
      // A bare key that has suffixed siblings breaks the all-suffixed
      // convention (i18next zh would answer the bare key while en answers
      // the suffixed one — the exact 1.11 drift).
      if (
        Object.prototype.hasOwnProperty.call(catalogs[locale], `${base}_one`) ||
        Object.prototype.hasOwnProperty.call(catalogs[locale], `${base}_other`)
      ) {
        problems.push(`${locale}: "${key}" carries _one/_other siblings — fold it into the pair`);
      }
      continue;
    }
    if (key.endsWith("_one") && !Object.hasOwn(catalogs[locale], `${base}_other`)) {
      problems.push(`${locale}: "${key}" has no "${base}_other" sibling`);
    }
    if (key.endsWith("_other") && !Object.hasOwn(catalogs[locale], `${base}_one`)) {
      problems.push(`${locale}: "${key}" has no "${base}_one" sibling`);
    }
  }
}

// 3. Placeholder parity.
for (const key of keySets[0]) {
  const sets = CATALOGS.map((l) =>
    Object.hasOwn(catalogs[l], key) ? placeholders(catalogs[l][key]) : null,
  );
  if (sets.some((s) => s && JSON.stringify(s) !== JSON.stringify(sets[0]))) {
    problems.push(`placeholders differ for "${key}": ${CATALOGS.map((l, i) => `${l}=[${sets[i]}]`).join(" ")}`);
  }
}

if (problems.length > 0) {
  console.error(`i18n:check failed (${problems.length} problem${problems.length === 1 ? "" : "s"}):`);
  for (const p of problems) console.error(`  - ${p}`);
  process.exit(1);
}

const total = keySets[0].size;
console.log(`i18n:check ok — ${total} keys in lockstep across ${CATALOGS.join(" + ")}`);
