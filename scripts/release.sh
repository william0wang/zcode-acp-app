#!/usr/bin/env bash
# One-shot release flow: bump version -> build APK -> commit + tag -> push -> GitHub Release.
# Usage: scripts/release.sh <version>   (e.g. scripts/release.sh 0.2.16)
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="${1:?usage: scripts/release.sh <version> (e.g. 0.2.16)}"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "error: '$VERSION' is not X.Y.Z" >&2; exit 1; }

# Preconditions: releases are cut from a clean main, tag must be fresh, gh must work.
if [[ -n "$(git status --porcelain)" ]]; then
  echo "error: working tree is dirty; commit or stash first" >&2
  git status --short >&2
  exit 1
fi
[[ "$(git rev-parse --abbrev-ref HEAD)" == "main" ]] || { echo "error: not on main" >&2; exit 1; }
command -v gh >/dev/null || { echo "error: gh CLI not installed" >&2; exit 1; }
TAG="v${VERSION}"
git fetch --tags origin 2>/dev/null || true
if git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null; then
  echo "error: tag ${TAG} already exists" >&2
  exit 1
fi

# Bump the 6 version carriers: package.json, tauri.conf.json, Cargo.toml,
# Cargo.lock, plus the desktop line's src-gpui workspace + its lock (the app
# reports this number in its bundle Info.plist, so a drifted pair would ship
# two different versions of one release).
# Regex, not JSON rewrite, keeps the files byte-identical apart from the version.
node - "$VERSION" <<'EOF'
const fs = require("fs");
const v = process.argv[2];
const bump = (path, pattern) => {
  const text = fs.readFileSync(path, "utf8");
  // Only a missing version field is fatal; a file already at the target
  // version (e.g. the feature commit bumped it ahead of the release) is fine.
  if (!pattern.test(text)) throw new Error(`no version found in ${path}`);
  fs.writeFileSync(path, text.replace(pattern, `$1"${v}"`));
};
bump("package.json", /("version"\s*:\s*)"[^"]*"/);
bump("src-tauri/tauri.conf.json", /("version"\s*:\s*)"[^"]*"/);
bump("src-tauri/Cargo.toml", /(^version\s*=\s*)"[^"]*"/m);
bump("src-tauri/Cargo.lock", /(name = "app"\nversion = )"[^"]*"/);
bump("src-gpui/Cargo.toml", /(^version\s*=\s*)"[^"]*"/m);
bump("src-gpui/Cargo.lock", /(name = "app"\nversion = )"[^"]*"/);
bump("src-gpui/Cargo.lock", /(name = "protocol"\nversion = )"[^"]*"/);
EOF

# Build before any commit/tag lands: a failed build reverts the bump and aborts.
APK="dist/ZCode-ACP-v${VERSION}-release.apk"
if ! pnpm build:android; then
  git checkout -- package.json src-tauri/tauri.conf.json src-tauri/Cargo.toml src-tauri/Cargo.lock src-gpui/Cargo.toml src-gpui/Cargo.lock
  echo "error: build failed; version bump reverted" >&2
  exit 1
fi

git add package.json src-tauri/tauri.conf.json src-tauri/Cargo.toml src-tauri/Cargo.lock src-gpui/Cargo.toml src-gpui/Cargo.lock
# The version may already sit at the target (e.g. the fix/feature commit was
# tested at that version) — nothing to stage then, and a bare `git commit`
# would abort the whole flow. Skip the commit in that case.
if ! git diff --cached --quiet; then
  git commit -m "chore: bump version to ${VERSION}"
fi
git tag "${TAG}"
git push origin main "${TAG}"
# A GITHUB_TOKEN/GH_TOKEN env var overrides gh's keyring login; if that PAT
# lacks this repo the release call dies with "Could not resolve to a
# Repository". Drop it so the interactive `gh auth` credentials win.
unset GITHUB_TOKEN GH_TOKEN
gh release create "${TAG}" "${APK}" --title "${TAG}" --generate-notes
# Only reached on a successful release (set -e bails out above otherwise).
# The Rust target dir is disposable build state that grows to gigabytes;
# dropping it also forces the next release to compile from scratch.
rm -rf src-tauri/target
echo "==> released ${TAG}: ${APK}"
echo "==> cleaned src-tauri/target"
