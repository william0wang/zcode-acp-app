#!/usr/bin/env bash
# Automated acceptance (验收): run EVERY implemented feature's guard tests
# and print a per-feature verdict. The feature inventory lives in
# scripts/acceptance-manifest.txt; the verdict rule: every guard filter
# needs >= 1 passing test and zero failures, else FAIL / NO COVERAGE.
#
# Stages: 1 cargo test --workspace, 2 vitest, 3 tsc, 4 macOS release build,
# 5 smoke launch — then scripts/acceptance-report.cjs assembles the report
# (stdout table + .zcode/scratch/acceptance-report-<date>.md).
#
# Env: ACCEPTANCE_SKIP_BUILD=1 skips stages 4-5 (marking them SKIPPED).
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "== [1/5] Rust suites (cargo test --workspace)"
(cd "$ROOT/src-gpui" && RUST_MIN_STACK=16777216 cargo test --workspace) >"$TMP/cargo.log" 2>&1
CARGO_RC=$?
if ! grep -q "^test result:" "$TMP/cargo.log"; then
  echo "   WARNING: no test results parsed — tail of the log:" && tail -n 5 "$TMP/cargo.log" | sed 's/^/   /'
fi
grep -E "^test result:" "$TMP/cargo.log" | sed 's/^/   /'

echo "== [2/5] Mobile suites (vitest run)"
pnpm vitest run --reporter=json --outputFile="$TMP/vitest.json" >"$TMP/vitest.log" 2>&1
VITEST_RC=$?
if [ ! -s "$TMP/vitest.json" ]; then
  echo "   vitest json missing — tail of the log:" && tail -n 5 "$TMP/vitest.log" | sed 's/^/   /'
fi

echo "== [3/5] Mobile types (tsc --noEmit)"
./node_modules/.bin/tsc --noEmit -p tsconfig.json >"$TMP/tsc.log" 2>&1
TSC_RC=$?
[ "$TSC_RC" -ne 0 ] && tail -n 5 "$TMP/tsc.log" | sed 's/^/   /'

: >"$TMP/steps.env"
if [ "$TSC_RC" -eq 0 ]; then echo "tsc=pass" >>"$TMP/steps.env"; else echo "tsc=fail" >>"$TMP/steps.env"; fi

if [ "${ACCEPTANCE_SKIP_BUILD:-0}" = "1" ]; then
  echo "== [4-5/5] build + smoke skipped (ACCEPTANCE_SKIP_BUILD=1)"
  echo "build=skip" >>"$TMP/steps.env"
  echo "smoke=skip" >>"$TMP/steps.env"
else
  echo "== [4/5] macOS release build"
  if bash scripts/build-macos-gpui.sh >"$TMP/build.log" 2>&1; then
    echo "build=pass" >>"$TMP/steps.env"
    tail -n 2 "$TMP/build.log" | sed 's/^/   /'
  else
    echo "build=fail" >>"$TMP/steps.env"
    tail -n 10 "$TMP/build.log" | sed 's/^/   /'
  fi

  echo "== [5/5] smoke launch (dist/ZCode-acp.app)"
  APP="$ROOT/dist/ZCode-acp.app"
  # Pinned to the DIST path: a bare "ZCode-acp.app" pattern would also
  # match an installed /Applications copy the user may be running.
  MATCH="$ROOT/dist/ZCode-acp.app/Contents/MacOS"
  if ! grep -q "^build=pass" "$TMP/steps.env"; then
    echo "smoke=fail" >>"$TMP/steps.env"
    echo "   build failed — refusing to smoke the stale dist"
  elif [ ! -d "$APP" ]; then
    echo "smoke=fail" >>"$TMP/steps.env"
    echo "   $APP missing"
  elif pgrep -f "$MATCH" >/dev/null 2>&1; then
    # Never kill or reuse a pre-existing instance (it may be a live
    # acceptance pass of its own): `open` would just activate it, and a
    # quit attempt could hit the quit-guard dialog.
    echo "smoke=fail" >>"$TMP/steps.env"
    echo "   a dist instance is already running — quit it and re-run"
  else
    open "$APP"
    NEW_PIDS=""
    for _ in 1 2 3 4 5 6 7 8 9 10; do
      NEW_PIDS="$(pgrep -f "$MATCH" | sort | tr '\n' ' ')"
      [ -n "$NEW_PIDS" ] && break
      sleep 1
    done
    if [ -n "$NEW_PIDS" ]; then
      echo "smoke=pass" >>"$TMP/steps.env"
      echo "   launched — terminating"
      # SIGTERM, not AppleScript quit: the quit guard pops a native
      # confirm dialog while a session runs, and osascript would hang on
      # it silently (a leaked instance, in practice).
      for pid in $NEW_PIDS; do kill -TERM "$pid" 2>/dev/null || true; done
      for _ in 1 2 3 4 5 6 7 8 9 10; do
        pgrep -f "$MATCH" >/dev/null 2>&1 || break
        sleep 1
      done
      pgrep -f "$MATCH" >/dev/null 2>&1 && for pid in $NEW_PIDS; do kill -9 "$pid" 2>/dev/null || true; done
    else
      echo "smoke=fail" >>"$TMP/steps.env"
      echo "   no app process appeared"
    fi
  fi
fi

echo ""
echo "== acceptance report"
REPORT=".zcode/scratch/acceptance-report-$(date +%F).md"
node scripts/acceptance-report.cjs \
  --manifest scripts/acceptance-manifest.txt \
  --cargo "$TMP/cargo.log" \
  --vitest "$TMP/vitest.json" \
  --steps "$TMP/steps.env" \
  --out "$REPORT"
RC=$?
# The report covers the manifest; the raw suite exit codes are the
# backstop for failures no filter names (a red suite must never exit 0).
if [ "$CARGO_RC" -ne 0 ] || [ "$VITEST_RC" -ne 0 ]; then
  echo "note: underlying suite exit codes — cargo=$CARGO_RC vitest=$VITEST_RC"
  RC=1
fi
exit $RC
