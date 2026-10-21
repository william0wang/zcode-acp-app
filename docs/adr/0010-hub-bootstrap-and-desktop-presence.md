# Hub Bootstrap and Desktop Presence

The desktop client must work without a terminal running the hub, but the
hub's lifecycle machinery (detached singleton daemon, EADDRINUSE spawn race,
idle-exit, bridge-heartbeat respawn, upgrade cycles) already lives in the
server and must not be duplicated or changed. We decided:

- **Hub Bootstrap**: the app requires `zcode-acp-server` installed (skips
  when present, guides installation when absent — never bundles or replaces
  it). When no hub is listening, the app spawns the hub daemon detached,
  joining the same singleton race any bridge runs, then connects to whoever
  won the port. The hub is always a background task; closing the app never
  stops it, and a hub found already listening is simply connected to.
- **Desktop Presence**: the hub recognizes connected desktop clients by
  clientInfo. While one is present, every NEW session — from the app, mobile,
  or web alike — incubates in the headless serve form (no martty terminal
  window): the desktop app is the presenting surface, so a terminal window
  would re-render the same stream for nobody. With no desktop client
  connected, visible-terminal semantics are unchanged (server ADR-0016), and
  windowed sessions that already exist keep their windows.

Desktop Presence is an amendment to server ADR-0016 and requires a
server-side change (client identity at initialize + an incubation branch);
martty itself is NOT headless-ized — the existing serve bridge already is
the no-renderer form, so reworking martty's render-coupled App state would
duplicate it.

## Amendment: presence outlives the session connection; one bridge per project

Connection presence alone had a gap: it ended with the app's last proxied
ACP connection, and a serve bridge self-exits once its last session closes
(server ADR-0006/0016). A running app with no open tab — the user closed
their conversations, or the bridge just retired — was therefore invisible
to the hub, and the next phone-originated create or resume opened a martty
terminal window next to the app the user was looking at. Two changes:

- **Three presence signals, not one.** Besides the proxied connection, the
  hub counts the app's own stamped HTTP requests (`x-zcode-client`, every
  hub call) — and keeps the app present for 45s after the last authorized
  one. The app's unconditional 15s discovery poll (`DISCOVERY_REFRESH_MS`,
  runtime.rs) is the steady source, so presence now means "the app process
  is running", independent of open sessions. The TTL is three poll
  intervals; the poll cadence is pinned to it (see the constant's comment).
- **Under presence, reuse the live bridge.** With no window to be the
  feature, the pre-ADR-0016 reuse is right again: a create or resume (from
  the app, the phone, or the web) answers with the workspace's live
  serve-origin bridge (`reused: true`, nothing spawned) — a resume with the
  instance already holding that session live, whatever its origin — and
  only a workspace without a live bridge spawns a headless one. One bridge
  and one `zcode app-server` process per project instead of one per
  session; the app's `open_tab(created.id, …)` and the phone's
  `connectInstance` + `session/new` / `session/load` already handled
  `reused` instances.

Without any desktop signal the ADR-0016 window semantics are unchanged.
