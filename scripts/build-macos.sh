#!/usr/bin/env bash
# Build a macOS app bundle and copy it into dist/.
# Usage: scripts/build-macos.sh
# Unsigned (no Developer ID, no notarization): runs as built on the build
# machine; elsewhere Gatekeeper needs a one-time right-click -> Open.
# Bundles the .app only: dmg wrapping (hdiutil + Finder AppleScript) adds
# nothing over the .app and breaks in headless/sandboxed shells.
set -euo pipefail
cd "$(dirname "$0")/.."

APP="src-tauri/target/release/bundle/macos/ZCode ACP.app"
# Stable name (no version suffix): each build overwrites the previous one,
# so the app replaces cleanly in /Applications / dock / Launchpad.
OUT="dist/ZCode-acp.app"

echo "==> building macOS app"
# dist/ is tauri's frontendDist and gets embedded into the bundle whole; a
# leftover artifact from an earlier build would ride along (same reason
# build-android.sh drops stale APKs first).
rm -rf dist/*.app
pnpm exec tauri build --bundles app

[[ -d "${APP}" ]] || { echo "error: no app bundle produced" >&2; exit 1; }
mkdir -p dist
# ditto, not cp: preserves bundle permissions/metadata the app needs.
ditto "${APP}" "${OUT}"
echo "==> ${OUT}"
du -sh "${OUT}" | awk '{print $1, $2}'
