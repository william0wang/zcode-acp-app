#!/usr/bin/env bash
# Build an Android APK and copy it into dist/ for easy access.
# Usage: scripts/build-android.sh [--debug]
# Release builds are signed with the local key in .signing/ (gitignored).
set -euo pipefail
cd "$(dirname "$0")/.."

DEBUG_FLAG=""
BUILD_TYPE="release"
if [[ "${1:-}" == "--debug" ]]; then
  DEBUG_FLAG="--debug"
  BUILD_TYPE="debug"
fi

VERSION=$(node -e "console.log(require('./src-tauri/tauri.conf.json').version)")
APK="src-tauri/gen/android/app/build/outputs/apk/universal/${BUILD_TYPE}/app-universal-${BUILD_TYPE}.apk"
OUT="dist/ZCode-ACP-v${VERSION}-${BUILD_TYPE}.apk"

# Gradle and the Kotlin compiler start Daemon processes that outlive the
# build and sit on hundreds of MB. `gradle.properties` turns both off
# (`org.gradle.daemon=false`, in-process Kotlin compilation) because this
# project is only ever built from the CLI; this trap is the belt-and-braces
# for a run made before those settings landed, or by an IDE that overrides
# them. `--stop` talks to a running Gradle daemon; the pkill sweep covers the
# Kotlin daemon, which `--stop` does not.
ANDROID_DIR="src-tauri/gen/android"
stop_daemons() {
  local status=$?
  echo "==> stopping Gradle / Kotlin daemons"
  (cd "${ANDROID_DIR}" && ./gradlew --stop >/dev/null 2>&1) || true
  pkill -f "KotlinCompileDaemon" 2>/dev/null || true
  return $status
}
trap stop_daemons EXIT

echo "==> building ${BUILD_TYPE} APK (v${VERSION})"
# arm64-only: real devices running this app are all aarch64; the other ABIs
# (armv7/x86/x86_64) only tripled the APK size (36M -> 12M).
# dist/ is tauri's frontendDist and gets embedded into the APK whole; a
# leftover APK from an earlier build would ride along (v0.20.0 shipped the
# v0.19.0 APK inside its native lib, +4 MB).
rm -f dist/*.apk
pnpm exec tauri android build --apk --target aarch64 ${DEBUG_FLAG}

mkdir -p dist
cp "${APK}" "${OUT}"
echo "==> ${OUT}"
ls -lh "${OUT}" | awk '{print $5, $9}'
