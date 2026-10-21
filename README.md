# ZCode ACP

[简体中文](README.zh-CN.md) | English

The app for [zcode-acp](https://github.com/william0wang/zcode-acp): drive the
agent sessions your machine runs, from a native desktop workbench, an Android
phone, or any browser. Every surface attaches to a zcode-acp Hub over
WebSocket; the transport contract is the server's
[REMOTE-CLIENTS.md](https://github.com/william0wang/zcode-acp/blob/main/docs/REMOTE-CLIENTS.md).

Two identifiers deliberately keep the historical `zcode-acp-remote` spelling
and must not be renamed casually: the desktop client's wire identity
(`zcode-acp-remote-desktop`, matched by the hub) and the desktop config
directory (`~/.config/zcode-acp-remote`).

## Surfaces

| Surface | Stack | Where it runs |
| --- | --- | --- |
| **Desktop workbench** (`src-gpui/`) | Native Rust, gpui-kit | macOS (unsigned `.app`) |
| **Android** (`src/` + `src-tauri/`) | Tauri 2 + React + TypeScript | Android APK |
| **Web** (same `src/`) | Plain SPA, no Tauri APIs | Any browser |

The desktop workbench is a dock-style window: a rail of panel toggles, the
sessions and configuration panels docked beside a tabbed chat area, and the
active session's file browser on the right. The Android and web clients share
one React codebase; the desktop client is its own product line, not a port of
it (ADR-0009). Both compile the same i18n catalogs.

## Features

Shared by every surface:

- Attach to existing sessions, or start new ones in any of the machine's known
  projects (bridge 0.17.0+), or browse the machine's directories for a target
  (bridge 0.65.0+)
- Streaming assistant replies with Markdown, syntax-highlighted code, and diff
  views
- Approve tool permissions and answer AskUserQuestion forms in the session that
  raised them — including requests that fired while the client was offline
  (re-delivered after reconnect)
- Per-session prompt queue whose drafts survive app restarts
- Session files: breadcrumb browser over the active session's directory tree,
  with a line-windowed text viewer, image preview, and a download fallback
- Configuration pages for the machine: models, usage statistics, plan quota and
  its reset cards, system status, skills, MCP servers, hooks, subagents,
  backups, dynamic workflows, and app updates
- Plan-quota allowance windows with reset cards, and the account's usage
  statistics
- Reconnect with replay catch-up; transient drops stay quiet
- English / 简体中文 UI

Desktop-only:

- One tab per session, each with its own connection; ⌘1–9 switch tabs, ⌘K
  searches sessions, Esc stops the running turn
- Hub bootstrap — when no hub is listening the app spawns the installed
  `zcode-acp-server` daemon and connects to it (ADR-0010)
- Desktop Presence — while the workbench is connected, new sessions incubate
  headless instead of opening terminal windows (ADR-0010)
- Open the browsed folder or file in Zed, when the hub is local and Zed is
  installed (the action is absent otherwise)

## Install

**Android** — grab the latest APK from [Releases](../../releases), sideload
it, and enter your Hub URL + token.

**Desktop** — build it yourself (no prebuilt download), see
[Desktop workbench](#desktop-workbench).

**Web** — deploy the SPA yourself, see
[Standalone web deployment](#standalone-web-deployment).

## Server setup

Every surface is a client; you need a running zcode-acp-server hub:

1. Install and run [zcode-acp-server](https://github.com/william0wang/zcode-acp)
   on the machine whose agent sessions you want to drive.
2. Set a remote token (`ZCODE_ACP_REMOTE_TOKEN`) when starting the bridge if the
   hub is reachable from other machines.
3. Expose the hub over `https://` (a tunnel works — the app upgrades https→wss
   itself) or use plain `http://` on your LAN.

On the machine running the workbench the hub may be local: the desktop app
starts it when nothing is listening. A token is still required — when neither
the config file nor `ZCODE_ACP_REMOTE_TOKEN` carries one, the app shows a
guided setup card and writes the token it generates.

## Development

```bash
pnpm install
pnpm dev                # browser at http://localhost:5173 (fastest loop)
pnpm tauri android dev  # on a device/emulator via adb
pnpm build              # type-check + bundle to dist/
pnpm test               # vitest
```

Desktop workbench:

```bash
cargo check  --manifest-path src-gpui/Cargo.toml
cargo test   --manifest-path src-gpui/Cargo.toml -p app
```

## Android APK

```bash
pnpm build:android          # release APK -> dist/, needs a signing key
pnpm build:android:debug    # debug APK -> dist/, no key needed
```

Release builds are signed with a key in `.signing/` (gitignored; create your
own `keystore.jks` + `keystore.properties` there). Identifier: `app.zcode.acp`
(immutable once installed).

The icon set is generated from `scripts/gen-icon.mjs` (SVG via `sharp` →
1024px source) — rerun
`node scripts/gen-icon.mjs && pnpm tauri icon src-tauri/icons/app-icon.png`
after changing the design.

## Desktop workbench

```bash
./scripts/build-macos-gpui.sh   # native gpui build -> dist/ZCode-acp.app
pnpm build:macos                # WebView shell build -> dist/ZCode-acp.app
```

Two different macOS builds write the same bundle path and replace each other by
design: the native workbench (gpui, the desktop product line) and the Tauri
WebView shell (the same React UI the phone runs). Neither is signed (no
Developer ID, no notarization): it runs as built on your machine; on another
machine Gatekeeper needs a one-time right-click → Open.

## Standalone web deployment

The React client is a plain SPA — no Tauri APIs are used — so the web build
runs on iOS, desktop, and any other browser. Deploy it yourself; no backend of
your own is needed: the hub serves `Access-Control-Allow-Origin: *`.

**One click (Netlify)** — forks the repo into your own GitHub account and
builds it for you:

[![Deploy to Netlify](https://www.netlify.com/img/deploy/button.svg)](https://app.netlify.com/start/deploy?repository=https://github.com/william0wang/zcode-acp-app)

**Cloudflare Pages** — two ways:

- Git integration: fork the repo, then in the Pages dashboard connect it
  with build command `pnpm build`, output directory `dist`, and environment
  variable `NODE_VERSION=22`.
- Direct upload from your machine: `pnpm deploy:web` builds `dist/` and
  pushes it to a Pages project in **your own** Cloudflare account — no Git
  integration needed. Copy `.env.local.example` to `.env.local` (gitignored)
  to set the API token and project name; without a token, wrangler's own
  login flow (`pnpm dlx wrangler login`) is used instead.

**Any static host** — the Netlify config ships in `netlify.toml`; Vercel
detects the Vite build automatically; or build locally and upload:

```bash
pnpm build              # -> dist/ (static, self-contained)
pnpm exec vite preview  # local smoke test of the built bundle
```

**Served by the hub (same origin)** — with static hosting enabled
(`ZCODE_ACP_WEB_DIR` or `remote.webDir` in the server config), the hub
serves the build itself: one address for the page, the REST API, and the
WebSocket — no CORS, no mixed-content limits, no separate static host; behind
a tunnel a single https URL covers everything.

```bash
pnpm build
ZCODE_ACP_WEB_DIR="$PWD/dist" zcode-acp hub   # then open http://localhost:8377
```

On the connect screen leave the hub URL empty — it defaults to the page's
origin, so only the token is needed.

Caveats:

- On a separately deployed page, `https://` can only open `wss://` — enter
  the hub's tunneled `https://` URL.
- The token is stored in the browser's localStorage: don't use a shared
  machine, and prefer a private/incognito window for one-off access.

## Releasing

`pnpm release <version>` bumps the version carriers, builds the Android APK,
commits, tags, pushes, and publishes the GitHub Release in one shot. The
desktop workbench is built and distributed separately (it has no signed
artifact channel). Details: `docs/RELEASING.md`.

## Docs

- `README.zh-CN.md` — 简体中文文档
- `CONTEXT.md` — glossary for this context
- `docs/adr/` — architecture decisions

## License

[MIT](LICENSE)
