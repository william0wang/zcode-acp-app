#!/usr/bin/env bash
# Build the Desktop Workbench (src-gpui) macOS app bundle into dist/.
# Usage: scripts/build-macos-gpui.sh   (also `pnpm build:macos`)
# Unsigned (no Developer ID, no notarization): runs as built on the build
# machine; elsewhere Gatekeeper needs a one-time right-click -> Open.
# Bundles the .app only: dmg wrapping adds nothing over the .app and breaks
# in headless/sandboxed shells.
# Product name: ZCode-acp.app, no desktop suffix — the gpui workbench IS the
# desktop product line (requirements .zcode/docs/desktop-parity-requirements.md);
# the old Tauri WebView shell build is retired (Android is the Tauri target).
set -euo pipefail
cd "$(dirname "$0")/.."

BIN="src-gpui/target/release/app"
# Stable name (no version suffix): each build overwrites the previous one,
# so the app replaces cleanly in /Applications / dock / Launchpad.
OUT="dist/ZCode-acp.app"
# Follows the repo's main version (src-gpui workspace.package.version).
VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' src-gpui/Cargo.toml | head -1)"

echo "==> building Desktop Workbench (release)"
cargo build --manifest-path src-gpui/Cargo.toml --release

[[ -x "${BIN}" ]] || { echo "error: no workbench binary produced" >&2; exit 1; }
rm -rf "${OUT}"
mkdir -p "${OUT}/Contents/MacOS" "${OUT}/Contents/Resources"
# ditto, not cp: preserves bundle permissions/metadata the app needs.
# The cargo binary is plain `app`; the bundle renames it to the product name.
ditto "${BIN}" "${OUT}/Contents/MacOS/zcode-acp"
# icon-macos.icns (gen-icon.mjs): the padded macOS variant — full-bleed
# icon.icns renders a ring larger than every neighbor in the Dock.
ditto src-tauri/icons/icon-macos.icns "${OUT}/Contents/Resources/AppIcon.icns"
cat > "${OUT}/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key>
	<string>ZCode ACP</string>
	<key>CFBundleDisplayName</key>
	<string>ZCode ACP</string>
	<key>CFBundleExecutable</key>
	<string>zcode-acp</string>
	<key>CFBundleIdentifier</key>
	<string>app.zcode.acp</string>
	<key>CFBundleVersion</key>
	<string>${VERSION}</string>
	<key>CFBundleShortVersionString</key>
	<string>${VERSION}</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleIconFile</key>
	<string>AppIcon</string>
	<key>LSMinimumSystemVersion</key>
	<string>11.0</string>
	<key>NSHighResolutionCapable</key>
	<true/>
</dict>
</plist>
PLIST
echo "==> ${OUT} (${VERSION})"
du -sh "${OUT}" | awk '{print $1, $2}'
# The bundle is self-contained, but the cargo tree behind it is not small
# (debug + release easily 6G+) and fully regenerable — drop it so a build
# leaves only the installable artifact behind. `set -e` keeps this on the
# success path only; the cost is a cold (full-deps) rebuild next time.
cargo clean --manifest-path src-gpui/Cargo.toml
echo "==> cleaned src-gpui/target"
