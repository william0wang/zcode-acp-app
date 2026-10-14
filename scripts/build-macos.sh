#!/usr/bin/env bash
# Build a macOS app bundle and copy it into dist/.
# Usage: scripts/build-macos.sh [--debug]
# Unsigned (no Developer ID, no notarization): runs as built on the build
# machine; elsewhere Gatekeeper needs a one-time right-click -> Open.
# Bundles the .app only: dmg wrapping (hdiutil + Finder AppleScript) adds
# nothing over the .app and breaks in headless/sandboxed shells.
set -euo pipefail
cd "$(dirname "$0")/.."

DEBUG_FLAG=""
BUILD_TYPE="release"
if [[ "${1:-}" == "--debug" ]]; then
  DEBUG_FLAG="--debug"
  BUILD_TYPE="debug"
fi

VERSION=$(node -e "console.log(require('./src-tauri/tauri.conf.json').version)")
APP="src-tauri/target/${BUILD_TYPE}/bundle/macos/ZCode ACP.app"
OUT="dist/ZCode-ACP-v${VERSION}-macos-${BUILD_TYPE}.app"

echo "==> building ${BUILD_TYPE} macOS app (v${VERSION})"
# dist/ is tauri's frontendDist and gets embedded into the bundle whole; a
# leftover artifact from an earlier build would ride along (same reason
# build-android.sh drops stale APKs first).
rm -rf dist/*.app
pnpm exec tauri build --bundles app ${DEBUG_FLAG}

[[ -d "${APP}" ]] || { echo "error: no app bundle produced" >&2; exit 1; }
mkdir -p dist
# ditto, not cp: preserves bundle permissions/metadata the app needs.
rm -rf "${OUT}"
ditto "${APP}" "${OUT}"
echo "==> ${OUT}"
du -sh "${OUT}" | awk '{print $1, $2}'
