#!/usr/bin/env node
// Assembles the per-feature acceptance report from the raw suite logs.
// Inputs come from scripts/acceptance.sh; the verdict rule lives in
// scripts/acceptance-manifest.txt (every filter must have >= 1 passing
// test, zero failures, or the feature reports NO COVERAGE / FAIL).
//
// Usage: node scripts/acceptance-report.cjs \
//   --manifest scripts/acceptance-manifest.txt \
//   --cargo /tmp/cargo.log --vitest /tmp/vitest.json \
//   --steps /tmp/steps.env --out .zcode/scratch/acceptance-report.md

"use strict";

const fs = require("fs");

function parseArgs(argv) {
  const args = {};
  for (let i = 2; i < argv.length; i += 2) {
    const key = argv[i].replace(/^--/, "");
    args[key] = argv[i + 1];
  }
  return args;
}

// cargo test's plain reporter: "test path::name ... ok|FAILED|ignored".
function parseCargoLog(path) {
  const results = new Map(); // full test name -> "pass" | "fail" | "ignored"
  if (!path || !fs.existsSync(path)) return results;
  const text = fs.readFileSync(path, "utf8");
  for (const match of text.matchAll(/^test ([\w:]+)\s+\.\.\.\s+(ok|FAILED|ignored)/gm)) {
    results.set(match[1], match[2] === "ok" ? "pass" : match[2] === "FAILED" ? "fail" : "ignored");
  }
  return results;
}

// vitest --reporter=json: testResults[].name is the file path, status per file.
function parseVitestJson(path) {
  const files = new Map(); // file name -> { passed, failed }
  if (!path || !fs.existsSync(path)) return files;
  let parsed;
  try {
    parsed = JSON.parse(fs.readFileSync(path, "utf8"));
  } catch {
    return files;
  }
  for (const suite of parsed.testResults || []) {
    const name = (suite.name || "").split("/").pop();
    let passed = 0;
    let failed = 0;
    for (const test of suite.assertionResults || []) {
      if (test.status === "passed") passed += 1;
      else if (test.status === "failed") failed += 1;
    }
    files.set(name, { passed, failed });
  }
  return files;
}

function parseSteps(path) {
  const steps = {};
  if (!path || !fs.existsSync(path)) return steps;
  for (const line of fs.readFileSync(path, "utf8").split("\n")) {
    const match = line.match(/^(\w+)=(\w+)$/);
    if (match) steps[match[1]] = match[2];
  }
  return steps;
}

function main() {
  const args = parseArgs(process.argv);
  const cargo = parseCargoLog(args.cargo);
  const vitest = parseVitestJson(args.vitest);
  const steps = parseSteps(args.steps);
  const manifest = fs
    .readFileSync(args.manifest || "scripts/acceptance-manifest.txt", "utf8")
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line && !line.startsWith("#"));

  const rows = [];
  for (const line of manifest) {
    // Split on the padded separator only: rust filters carry their own
    // "::" module paths (e.g. rust:session::tests).
    const parts = line.split(/\s+::\s+/).map((s) => s.trim());
    if (parts.length !== 3) {
      rows.push({ id: "?", feature: `BAD LINE: ${line}`, verdict: "NO COVERAGE", detail: "" });
      continue;
    }
    const [id, feature, guardList] = parts;
    const guards = guardList.split(",").map((g) => g.trim());
    const details = [];
    let verdict = "PASS";
    for (const guard of guards) {
      // Split on the FIRST colon only: filters carry rust module paths of
      // their own (rust:session::tests must not degrade to filter
      // "session", which would match half the suite).
      const sep = guard.indexOf(":");
      const kind = sep === -1 ? guard : guard.slice(0, sep);
      const filter = sep === -1 ? "" : guard.slice(sep + 1);
      if (kind === "rust") {
        let pass = 0;
        let fail = 0;
        for (const [name, status] of cargo) {
          if (name.includes(filter)) {
            if (status === "pass") pass += 1;
            else if (status === "fail") fail += 1;
          }
        }
        if (fail > 0) {
          verdict = "FAIL";
          details.push(`${filter}: ${fail} failing`);
        } else if (pass === 0) {
          if (verdict !== "FAIL") verdict = "NO COVERAGE";
          details.push(`${filter}: no matching test`);
        } else {
          details.push(`${filter}: ${pass}`);
        }
      } else if (kind === "vitest") {
        let pass = 0;
        let fail = 0;
        for (const [file, counts] of vitest) {
          if (file.includes(filter)) {
            pass += counts.passed;
            fail += counts.failed;
          }
        }
        if (fail > 0) {
          verdict = "FAIL";
          details.push(`${filter}: ${fail} failing`);
        } else if (pass === 0) {
          if (verdict !== "FAIL") verdict = "NO COVERAGE";
          details.push(`${filter}: no matching file`);
        } else {
          details.push(`${filter}: ${pass}`);
        }
      } else if (kind === "step") {
        const status = steps[filter];
        if (status === "pass") {
          details.push(filter);
        } else if (status === "skip") {
          if (verdict === "PASS") verdict = "SKIPPED";
          details.push(`${filter} skipped`);
        } else {
          verdict = "FAIL";
          details.push(`${filter}: ${status || "did not run"}`);
        }
      }
    }
    rows.push({ id, feature, verdict, detail: details.join("; ") });
  }

  const counts = rows.reduce(
    (acc, row) => {
      acc[row.verdict] = (acc[row.verdict] || 0) + 1;
      return acc;
    },
    { PASS: 0 },
  );
  const failed = rows.filter((r) => r.verdict === "FAIL" || r.verdict === "NO COVERAGE");

  const table = rows
    .map(
      (r) =>
        `| ${r.verdict} | ${r.id} | ${r.feature.replace(/\|/g, "/")} | ${r.detail.replace(/\|/g, "/")} |`,
    )
    .join("\n");
  const report = [
    "# Automated acceptance report",
    "",
    `Date: ${new Date().toISOString().slice(0, 19)} (local run of \`scripts/acceptance.sh\`)`,
    "",
    `**${counts.PASS || 0} PASS / ${counts.FAIL || 0} FAIL / ${counts["NO COVERAGE"] || 0} NO COVERAGE / ${counts.SKIPPED || 0} SKIPPED**`,
    "",
    "| Verdict | Feature | Description | Guards |",
    "| --- | --- | --- | --- |",
    table,
    "",
    failed.length
      ? `## Needs attention\n\n${failed.map((r) => `- **${r.id}** (${r.verdict}): ${r.detail}`).join("\n")}\n`
      : "All features guarded and green.\n",
  ].join("\n");

  const mark = (verdict) =>
    verdict === "PASS" ? "✓" : verdict === "SKIPPED" ? "–" : "✗";
  for (const row of rows) {
    console.log(`${mark(row.verdict)} ${row.id.padEnd(22)} ${row.verdict.padEnd(13)} ${row.detail}`);
  }
  console.log(
    `\n${counts.PASS || 0} PASS / ${counts.FAIL || 0} FAIL / ${counts["NO COVERAGE"] || 0} NO COVERAGE / ${counts.SKIPPED || 0} SKIPPED`,
  );

  if (args.out) {
    fs.mkdirSync(require("path").dirname(args.out), { recursive: true });
    fs.writeFileSync(args.out, report);
    console.log(`report written: ${args.out}`);
  }
  process.exit(failed.length ? 1 : 0);
}

main();
