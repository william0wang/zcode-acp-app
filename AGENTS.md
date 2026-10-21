# Agent Instructions

## Project overview

**ZCode ACP** (repo directory `~/Develop/tools/zcode-acp-app`, GitHub
`william0wang/zcode-acp-app`) — the app of the zcode-acp
family: talks to a `zcode-acp-server` hub/bridge (ACP over the hub's
WebSocket + the bridge's Settings API) for sessions, dynamic workflows, and
settings.

Three clients share the repo:

| Directory | Client | Stack |
| --- | --- | --- |
| `src-gpui/` | Desktop workbench (the desktop product line) | Native Rust on gpui-kit |
| `src/` + `src-tauri/` | Android app | Tauri 2 + React + TypeScript |
| `src/` | Web SPA (same React source, no Tauri APIs) | Vite |

`src-gpui/` is NOT a port of the mobile UI — it has its own dock-style layout
(ADR-0009). Both lines compile the same i18n catalogs under `src/i18n/`.

Cross-repo requirement specs and ADRs live in this repo's `.zcode/docs/`
(e.g. `desktop-client-requirements.md`, `workflow-parity-backend-requirements.md`,
`push-backend-requirements.md`). The backend halves of those specs are
implemented in the bridge repo.

## Related project directories (one product family, this machine)

| Path                               | What it is                                                       |
| ---------------------------------- | ---------------------------------------------------------------- |
| `~/Develop/tools/zcode-acp-app`    | This app                                                          |
| `~/Develop/tools/zcode-acp-server` | The bridge (Node: `zcode app-server` ↔ ACP) — implements the backend halves of this repo's specs |
| `~/Develop/tools/zcode-acp-martty` | Source of the Rust `martty` TUI (the bridge's CLI frontend)       |
| `~/Develop/NoBackup/ZCode`         | Open-source ZCode upstream (protocol + desktop source)            |
