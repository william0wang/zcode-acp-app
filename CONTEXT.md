# zcode-acp-remote

ZCode ACP: mobile Remote Client for zcode-acp-server. A Tauri 2 + React app
that attaches to a Hub over WebSocket and drives the same agent sessions as
the desktop editor. Android-first; iOS deferred.

## Language

Protocol terms (Hub, Bridge, Instance, Remote Client, Session, Broadcast,
Session Authority) are defined by the server glossary at
`../zcode-acp-server/CONTEXT.md`; this app inherits them unchanged.

**Connection Profile**:
The saved Hub URL and bearer token the app dials. Persisted as a multi-server
book (`zcode-acp:servers`): many saved profiles, exactly one active —
`profile` mirrors the active entry. The connect screen doubles as the server
manager (switch / edit / delete); the legacy single-profile key migrates on
first load.
_Avoid_: server config, account

**Hub Bootstrap**:
The desktop client's way of ensuring a Hub exists — nothing more. When the
app wants to connect and no Hub is listening, it spawns the installed
`zcode-acp-server` hub daemon detached, joining the same EADDRINUSE singleton
race any bridge runs, then connects to whichever daemon won the port. The Hub
is always a background task: idle-exit, respawn, and upgrade cycles belong to
the server's own machinery (bridge heartbeats re-spawn it), never to the app.
Closing the app never stops the Hub. Requires the server installed; the app
skips when present and guides installation when absent — never replaces or
bundles it.
_Avoid_: embedded server, hub host, sidecar

**Desktop Workbench**:
The desktop-only client (a separate product line, not a port of the mobile
UI): a dock-style window — sessions/files/settings as dockable panels around
a chat area, multiple sessions in tabs. The mobile and web clients remain the
universal UI and stay in low-intensity maintenance.
_Avoid_: desktop mode, desktop skin

**Parity**:
The desktop client's coverage rule against the mobile app: every mobile
capability must be reachable on the desktop — that completeness comes first,
and reshaping the interaction to desktop conventions comes second. The
mobile app is the capability reference, never the layout reference: a
missing capability is a gap by definition; a different layout is not.
_Avoid_: UI port, pixel parity, mobile skin

**Desktop Presence**:
The Hub's awareness that the desktop client is connected (identified via its
clientInfo at initialize). While a desktop client is present, every NEW
session — created from the app, mobile, or web alike — incubates in the
headless serve form: no martty terminal window opens, because the desktop
app is the presenting surface for all sessions. With no desktop client
connected, session creation keeps the visible-terminal semantics (server
ADR-0016) unchanged, and sessions that already have a window keep it.
_Avoid_: app takeover, session manager

**Active Session**:
The single session currently loaded in the chat view. Switching sessions is a
`session/load` on the same connection.
_Avoid_: open tab, current chat

**Replay**:
The tail history catch-up a `session/load` (`_meta.zcode.limit`) produces as
`session/update`s; the recovery path after any disconnect. Older history
arrives on demand as prepended `session/load_earlier` pages (cursor from the
attach response's `replayMeta`).
_Avoid_: sync, history download

**Permission Race**:
First-response-wins semantics of interaction requests across clients; losing
clients receive `$/cancel_request`.
_Avoid_: permission conflict

**Approval Card**:
The bottom-sheet surface rendering `session/request_permission` requests —
shown ONLY in the session that raised it (bridge 0.17.0 per-session
semantics); another session's request surfaces through the session list's
awaiting-confirmation badge instead. Three shapes: Plan Approval
(ExitPlanMode), Tool Permission, and Question (AskUserQuestion). Context
shown on the card comes from the tool_call matched by `toolCallId`, never
guessed from the options alone.
_Avoid_: permission dialog, confirm popup

**Remote Session Create**:
Starting a NEW CLI session from the app by picking one of the machine's known
projects (bridge 0.17.0, server ADR-0014): the hub spawns — or reuses — a
serve bridge for the workspace, and the fresh session's cwd is that project.
Since server ADR-0016 the spawn opens a VISIBLE terminal REPL on the desktop
(~20s registration budget), and the owner closing that window retires the
bridge — the app treats the vanished instance like any dead bridge. The
known-project list is the default create whitelist; the browse mode (bridge
0.65.0) walks the machine's directories via `GET /api/fs/list` and starts a
session in any of them the hub flags `creatable` (a known project, or under
a `remote.projectRoots` root configured on the hub machine — unconfigured,
only known projects are creatable). There is no free-form path entry.
_Avoid_: new tab, project open

**Session History**:
The per-project browse/resume listing of the backend session store (bridge
0.19.0, server ADR-0015) — closed conversations included, newest first,
paged via the server's composite cursor (`{before, beforeId}` passed back
verbatim, never recomputed client-side). Fetched strictly on demand for the
one chosen project: a cold project's first page incubates its serve bridge
(~12s). Resuming loads the store id via `session/load` on the listing's
instance; the same conversation may also appear in discovery under a
different (ACP) id — the surfaces are deliberately not reconciled.
_Avoid_: archive, all-sessions list

**Session Activity**:
The per-session display state in the session list: awaiting confirmation,
running, just finished (60s window after a turn ends), or idle. Derived from
bridge-wide broadcasts; not persisted and unknown for other sessions until the
first broadcast after a reconnect.
_Avoid_: session status, online status

**Session Root**:
The directory a session was created or loaded with; the anchor every
session-file path resolves against. Absolute paths must land inside it;
escapes are rejected server-side.
_Avoid_: workspace, project folder

**Session Files**:
The read-only, hub-proxied view of the active session's files. Breadcrumb
navigation over single-level listings; dotfiles hidden by default with a
toggle. Requires the bridge to advertise the fs capability.
_Avoid_: file manager, explorer

**File Viewer**:
The full-screen reader for one session file: syntax-highlighted text loaded
in line windows, image preview, or a download fallback for other kinds.
_Avoid_: file preview dialog

**Attachment**:
A user-supplied image riding a prompt into the session (gallery pick, camera
shot, or web paste). Travels in-band as an ACP image block, unlike Session
Files which the agent reads and writes in the Session Root.
_Avoid_: upload, media message

## ZCode Configuration

Terms for the screens that read and write the machine's ZCode configuration
(ADR-0009). The data is machine-level — one hub, one configuration — so these
screens are independent of any session.

**Configuration Screen**:
The full-screen section of the app that manages the machine's ZCode
configuration: models, skills, MCP servers, hooks, subagents, plan quota with
its reset cards, usage statistics, config backups and app updates. Reached
from the settings panel and from the instance picker; its sections are
themselves full-screen pages, not rows in one long list.
_Avoid_: settings page, admin panel

**Effect Class**:
What a configuration write costs to take effect. `immediate` writes apply at
once; `needs-restart` writes only land after the bridge's backend is
restarted, which cancels in-flight turns. Every write response carries one,
and the app surfaces it instead of assuming.
_Avoid_: restart flag, dirty state

**Backend Restart**:
The bridge-side operation that applies a `needs-restart` write: cancel the
running turns, close the backend subprocess, let the next use respawn it.
Addressed per instance, because restarting requires naming the bridge to
disturb. Never triggered automatically by a write.
_Avoid_: reload, refresh backend

**Plan Quota**:
The account's coding-plan allowance: the 5-hour and weekly window
percentages, plus the reset cards that clear them. Distinct from Usage
Statistics, which counts tokens this machine actually spent.
_Avoid_: quota, credits, balance

**Reset Card**:
A single consumable that clears one quota window, carrying an expiry date and
nothing else — the API exposes no card number or denomination. Spending one is
irreversible, so it is gated on the highest quota window reaching 90%,
confirmed with the card's type and expiry, and sent with a fresh status nonce
and an idempotency key.
_Avoid_: quota reset, refill, coupon

**Reset Opportunity**:
The backend's answer to whether a reset would be granted right now. A denial
is a normal answer carrying the next time to try, not an error — the screen
shows a countdown.
_Avoid_: reset eligibility, reset permission

**Usage Statistics**:
The settings page with two tabs. Local machine usage: per-model token counts
read from the machine's agent database, bucketed by day. Platform usage: the
account-level monitor data (365-day activity heatmap, streaks, credits with
cache-hit trends, per-model and per-tool usage) the bridge aggregates from
the BigModel/Z.AI monitor API — the same data the desktop app's Coding Plan
usage tab shows.
_Avoid_: usage quota, token quota

**Config Backup**:
A timestamped copy the bridge keeps before rewriting one of its configuration
files, restorable through the same API. Scoped per file — restoring one
restores that file's state, not a machine-wide snapshot.
_Avoid_: snapshot, restore point
