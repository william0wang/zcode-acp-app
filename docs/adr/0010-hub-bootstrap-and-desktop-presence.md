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

## Amendment: the app re-spawns a hub that died mid-session

Hub Bootstrap originally ran once per lifecycle: a hub that went dark
AFTER the app connected (idle-exit once its last client left, a kill, a
crash) left the app retrying a dead port forever — only a manual restart
or a settings-driven lifecycle restart re-ran the bootstrap. The desktop
app is the presenting surface, so while it runs it must bring its own
backend back (user, 2026-11-13: "another client started the hub; when
that client exits the hub closes, and the desktop app should bring it
back up"). Decision:

- **The discovery poll doubles as the hub-liveness probe.** A
  network-level miss (the hub answered nothing) counts as one dark poll;
  an answered error — including the hub's own 500s — is a LIVE hub and
  resets the streak, so the supervisor never arms off the hub's own
  restart window more than the threshold allows.
- **Re-run Hub Bootstrap, not the lifecycle.** After
  `HUB_RESPAWN_AFTER_STREAK` (2) consecutive dark polls the driver
  re-runs `ensure_hub` on a spawned task — same EADDRINUSE singleton
  race, same adjudication — without closing tabs or resetting pages: the
  dial target (host/port/token) is the config the lifecycle already
  resolved, so the tabs' reconnect ladders and the next discovery poll
  recover on their own once the port answers.
- **Guard rails.** Local-hub only (a remote Connection Profile is
  dialed, never spawned — unchanged); one attempt in flight; attempts
  spaced by `HUB_RESPAWN_MIN_INTERVAL_MS` (15s) so a hub that dies on
  arrival cannot loop the spawn; a failed bootstrap surfaces in the
  sidebar (`hub restart failed: …`) instead of failing silently. The
  two-poll threshold also rides through the hub's self-upgrade restart
  (~2s dark), which would otherwise pay a pointless daemon spawn that
  the singleton race would have to clean up after.
