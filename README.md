# zcode-acp-remote

[简体中文](README.zh-CN.md) | English

Mobile remote client for [zcode-acp-server](https://github.com/william0wang/zcode-acp):
attach to your Hub over WebSocket from an Android phone and drive the same
agent sessions as your editor. See the server's
[REMOTE-CLIENTS.md](https://github.com/william0wang/zcode-acp/blob/main/docs/REMOTE-CLIENTS.md)
for the transport contract this app implements.

Attach to existing sessions, or start new ones in any of the machine's known
projects (bridge 0.17.0+) — the hub spawns a headless serve bridge for the
project and the session runs there.

## Features

- Session list with live activity badges (running / idle / just finished),
  long-press to retire a session remotely
- Start new sessions remotely: pick one of the machine's known projects and a
  CLI session starts there (bridge 0.17.0+)
- Chat with streaming assistant replies, Markdown + code highlight, diff views
- Approve tool permission requests and answer AskUserQuestion forms in the
  session that raised them — even ones that fired while the phone was offline
  (re-delivered after reconnect)
- Prompt queue per session — drafts survive app restarts
- Slash command completion, including `$`-prefixed skills
- Usage quota card with green→yellow→red heat colors
- Reconnect with replay catch-up; transient connection drops stay quiet
- English / 简体中文 UI

## Install

Grab the latest APK from [Releases](../../releases), sideload it, and enter
your Hub URL + remote token in the app.

The app is also a plain SPA — no Tauri APIs are used — so the web build runs
on iOS, desktop, and any other browser. See
[Standalone web deployment](#standalone-web-deployment).

## Server setup

This app is a client; it needs a running zcode-acp-server hub:

1. Install and run [zcode-acp-server](https://github.com/william0wang/zcode-acp)
   in your editor (it bridges the agent over a Hub WebSocket).
2. Set a remote token (`ZCODE_ACP_REMOTE_TOKEN`) when starting the bridge.
3. Expose the hub over `https://` (a tunnel works — the app upgrades
   https→wss itself) or use plain `http://` on your LAN.

## Stack

- Tauri 2 shell (no custom Rust code) — `docs/adr/0001`
- React + TypeScript + Vite, Tailwind CSS v4
- assistant-ui + shadcn-style custom components — `docs/adr/0002`
- Zustand store, native WebSocket + custom reconnect manager
- i18next (default `en`, ships `zh-CN`)

## Development

```bash
pnpm install
pnpm dev                # browser at http://localhost:5173 (fastest loop)
pnpm tauri android dev  # on a device/emulator via adb
pnpm build              # type-check + bundle to dist/
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

## macOS app (build it yourself)

```bash
pnpm build:macos          # unsigned .app -> dist/ZCode-acp.app
```

No prebuilt macOS download is provided — anyone can build one from source.
The bundle is unsigned (no Developer ID, no notarization): it runs as built
on your machine; on another machine Gatekeeper needs a one-time
right-click → Open. The embedded Info.plist allows plain http://ws:// hubs
(same policy as Android's cleartext traffic).

Releasing (maintainers): `pnpm release <version>` bumps all version files,
builds the APK, commits, tags, pushes, and publishes the GitHub Release in
one shot.

## Standalone web deployment

The app is a plain SPA — no Tauri APIs are used — so the web build runs on
iOS, desktop, and any other browser. Deploy it yourself; no backend of your
own is needed: the hub serves `Access-Control-Allow-Origin: *`.

**One click (Netlify)** — forks the repo into your own GitHub account and
builds it for you:

[![Deploy to Netlify](https://www.netlify.com/img/deploy/button.svg)](https://app.netlify.com/start/deploy?repository=https://github.com/william0wang/zcode-acp-remote)

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

## Docs

- `README.zh-CN.md` — 简体中文文档
- `CONTEXT.md` — glossary for this context
- `docs/adr/` — architecture decisions

## License

[MIT](LICENSE)
