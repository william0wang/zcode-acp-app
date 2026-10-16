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
