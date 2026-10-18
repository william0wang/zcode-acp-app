#!/usr/bin/env bash
# Automated acceptance (验收): run EVERY implemented feature's guard tests
# and print a per-feature verdict. The feature inventory lives in
# scripts/acceptance-manifest.txt; the verdict rule: every guard filter
# needs >= 1 passing test and zero failures, else FAIL / NO COVERAGE.
#
# Stages: 1 cargo test --workspace, 2 vitest, 3 tsc, 4 macOS release build,
# 5 smoke launch, 6 pixel golden (fixture screenshots vs committed
# baselines) — then scripts/acceptance-report.cjs assembles the report
# (stdout table + .zcode/scratch/acceptance-report-<date>.md).
#
# Env: ACCEPTANCE_SKIP_BUILD=1 skips stages 4-6 (marking them SKIPPED);
# GOLDEN_UPDATE=1 re-baselines the pixel goldens (after an intended visual
# change or a macOS upgrade).
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "== [1/6] Rust suites (cargo test --workspace)"
(cd "$ROOT/src-gpui" && RUST_MIN_STACK=16777216 cargo test --workspace) >"$TMP/cargo.log" 2>&1
CARGO_RC=$?
if ! grep -q "^test result:" "$TMP/cargo.log"; then
  echo "   WARNING: no test results parsed — tail of the log:" && tail -n 5 "$TMP/cargo.log" | sed 's/^/   /'
fi
grep -E "^test result:" "$TMP/cargo.log" | sed 's/^/   /'

echo "== [2/6] Mobile suites (vitest run)"
pnpm vitest run --reporter=json --outputFile="$TMP/vitest.json" >"$TMP/vitest.log" 2>&1
VITEST_RC=$?
if [ ! -s "$TMP/vitest.json" ]; then
  echo "   vitest json missing — tail of the log:" && tail -n 5 "$TMP/vitest.log" | sed 's/^/   /'
fi

echo "== [3/6] Mobile types (tsc --noEmit)"
./node_modules/.bin/tsc --noEmit -p tsconfig.json >"$TMP/tsc.log" 2>&1
TSC_RC=$?
[ "$TSC_RC" -ne 0 ] && tail -n 5 "$TMP/tsc.log" | sed 's/^/   /'

: >"$TMP/steps.env"
if [ "$TSC_RC" -eq 0 ]; then echo "tsc=pass" >>"$TMP/steps.env"; else echo "tsc=fail" >>"$TMP/steps.env"; fi

if [ "${ACCEPTANCE_SKIP_BUILD:-0}" = "1" ]; then
  echo "== [4-6/6] build + smoke + pixel golden skipped (ACCEPTANCE_SKIP_BUILD=1)"
  echo "build=skip" >>"$TMP/steps.env"
  echo "smoke=skip" >>"$TMP/steps.env"
  echo "golden=skip" >>"$TMP/steps.env"
else
  echo "== [4/6] macOS release build"
  if bash scripts/build-macos-gpui.sh >"$TMP/build.log" 2>&1; then
    echo "build=pass" >>"$TMP/steps.env"
    tail -n 2 "$TMP/build.log" | sed 's/^/   /'
  else
    echo "build=fail" >>"$TMP/steps.env"
    tail -n 10 "$TMP/build.log" | sed 's/^/   /'
  fi

  echo "== [5/6] smoke launch (dist/ZCode-acp.app)"
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

  echo "== [6/6] pixel golden (fixture windows → self-capture → diff)"
  # The paint gate's pixel layer: each fixture renders a canned snapshot in
  # a PINNED 1200x800 window at (120,120) (src-gpui golden_fixtures.rs) and
  # the app SELF-CAPTURES its own window (TCC-exempt — no Screen Recording
  # permission needed, unlike `screencapture` from a terminal) writing the
  # PNG to --golden-out, then exits. Baselines live in scripts/golden/ and
  # are machine/OS-specific — re-baseline deliberately with GOLDEN_UPDATE=1
  # after a macOS upgrade or an intended visual change.
  if ! grep -q "^smoke=pass" "$TMP/steps.env"; then
    echo "golden=fail" >>"$TMP/steps.env"
    echo "   smoke did not pass — refusing the pixel stage"
  else
    GOLDEN_FAIL=0
    GOLDEN_SKIP=0
    BIN="$APP/Contents/MacOS/zcode-acp"
    for name in chat workflows settings; do
      CAP="$TMP/golden-$name.png"
      # Bounded run: the fixture self-exits (capture written or refused),
      # but a hung capture path must never wedge the whole acceptance.
      "$BIN" --golden-fixture "$name" --golden-out "$CAP" >"$TMP/golden-$name.log" 2>&1 &
      GOLDEN_PID=$!
      RC=""
      for _ in $(seq 1 25); do
        if ! kill -0 "$GOLDEN_PID" 2>/dev/null; then
          wait "$GOLDEN_PID"
          RC=$?
          break
        fi
        sleep 1
      done
      if [ -z "$RC" ]; then
        echo "   $name: fixture did not exit within 25s — killing"
        kill -9 "$GOLDEN_PID" 2>/dev/null || true
        wait "$GOLDEN_PID" 2>/dev/null || true
        RC=1
      fi
      if [ "$RC" -eq 3 ]; then
        # The Metal surface refused readback — fall back to a region shot of
        # the pinned window frame (1200x800 + the 28pt titlebar). This route
        # needs the terminal to hold Screen Recording permission; without it
        # screencapture errors out or strips the windows.
        "$BIN" --golden-fixture "$name" >>"$TMP/golden-$name.log" 2>&1 &
        GOLDEN_PID=$!
        sleep 2
        screencapture -x -R120,120,1200,828 "$CAP" 2>>"$TMP/golden-$name.log" || true
        kill -TERM "$GOLDEN_PID" 2>/dev/null || true
        wait "$GOLDEN_PID" 2>/dev/null || true
      fi
      if [ ! -s "$CAP" ]; then
        if [ "$RC" -eq 3 ]; then
          echo "   $name: capture unavailable — self-capture came back empty and screencapture produced nothing"
          echo "     (grant Screen Recording to this terminal in System Settings → Privacy & Security to enable the fallback)"
          GOLDEN_SKIP=1
        else
          echo "   $name: no capture appeared — log:"
          tail -n 3 "$TMP/golden-$name.log" | sed 's/^/     /'
          GOLDEN_FAIL=1
        fi
        continue
      fi
      UPDATE_FLAG=""
      [ "${GOLDEN_UPDATE:-0}" = "1" ] && UPDATE_FLAG="--update"
      node scripts/golden-diff.cjs \
        --baseline "scripts/golden/$name.png" \
        --capture "$CAP" \
        --diff "$TMP/golden-$name-diff.png" \
        $UPDATE_FLAG
      DIFF_RC=$?
      if [ "$DIFF_RC" -eq 0 ]; then
        echo "   $name: ok"
      elif [ "$DIFF_RC" -eq 3 ]; then
        echo "   $name: capture unavailable (Metal readback refused and/or no Screen Recording for this terminal) — skipped"
        GOLDEN_SKIP=1
      elif [ "$DIFF_RC" -eq 1 ]; then
        echo "   $name: DIFF — $TMP/golden-$name-diff.png (GOLDEN_UPDATE=1 re-baselines)"
        GOLDEN_FAIL=1
      else
        echo "   $name: diff tool error (rc $DIFF_RC)"
        tail -n 3 "$TMP/golden-$name.log" | sed 's/^/     /'
        GOLDEN_FAIL=1
      fi
    done
    if [ "$GOLDEN_FAIL" -eq 1 ]; then
      echo "golden=fail" >>"$TMP/steps.env"
    elif [ "${GOLDEN_SKIP:-0}" -eq 1 ]; then
      # Environment precondition, not a regression: grant Screen Recording
      # to the terminal (System Settings → Privacy & Security) to turn this
      # into a real gate on this machine.
      echo "golden=skip" >>"$TMP/steps.env"
    else
      echo "golden=pass" >>"$TMP/steps.env"
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
