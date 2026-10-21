# Releasing

The two product lines release differently: the Android/Web line publishes a
signed APK to GitHub Releases from the maintainer's machine, while the desktop
workbench is a local unsigned build with no artifact channel.

## Desktop workbench

No release pipeline. `scripts/build-macos-gpui.sh` builds
`dist/ZCode-acp.app` from the version in `src-gpui/Cargo.toml`
(`[workspace.package].version`), which follows the repo's main version — bump
it in the same commit as the other carriers when cutting a release so the two
lines never report different numbers. There is no tag, no upload, and no
signing: the bundle runs on the build machine and needs a one-time
right-click → Open elsewhere.

## Android / Web

Manual and local-first: release APKs are built on the maintainer's machine
(the Android signing key lives in gitignored `.signing/`), and
`scripts/release.sh` turns a version number into a tag, a push and a GitHub
Release carrying the APK. There is no CI release pipeline.

## Versioning

Three-part semver on 0.x, mirroring `zcode-acp-server` (whose release-please
maps `feat:` → minor, `fix:` → patch — here the mapping is applied by hand):

- **Feature batch** (new UI / new capability, even one commit) → bump the
  MINOR: `0.3.x → 0.4.0`.
- **Fix-only** (bug fixes, no new capability) → bump the PATCH:
  `0.3.17 → 0.3.18`.
- Never retro-edit an already-built/released version; the next release
  carries the rule.

The rule exists so the number carries information: a minor change on the
left means "new capability, worth a look", a patch means "fixes only".

The version lives in FOUR carriers, bumped together: `package.json`,
`src-tauri/tauri.conf.json`, `src-tauri/Cargo.toml` and
`src-tauri/Cargo.lock` (the lock refreshes on the next cargo build — build
before committing the bump). Android's `versionCode` is derived from the
version string by the tauri CLI, so any forward bump stays
install-order-correct across minor/patch lines.

Commit style on `main`: `feat:` / `fix:` / `chore:` / `refactor:` — the
`chore: bump version to X.Y.Z` commit is the release boundary.

## Local test build

`bash scripts/build-android.sh` (or `--debug`) builds an aarch64-only APK
from the CURRENT version string in `src-tauri/tauri.conf.json` and copies it
to `dist/ZCode-ACP-v<version>-release.apk`. No commits, no tag — for
on-device verification before cutting a release.

## Official release

`scripts/release.sh <version>` from a clean `main`:

1. Validates: tree clean, on `main`, `gh` authenticated, tag `v<version>`
   unused.
2. Bumps the four carriers, then builds the release APK FIRST — a failed
   build reverts the bump and aborts before anything is committed.
3. Commits `chore: bump version to <version>` (skipped when the version was
   already committed at that number, e.g. after a tested pre-release build),
   tags `v<version>`, pushes `main` + the tag, and creates the GitHub
   Release with the APK attached.
