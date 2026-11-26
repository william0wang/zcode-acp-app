# ZCode ACP App

The client family (desktop workbench, Android app, web SPA) for zcode-acp
sessions over the hub. This glossary captures the domain language as it is
resolved in design conversations; terms land here when a decision firms them
up, not before.

## Remote Commands

**Remote Terminal**:
The persistent pairing between the mobile app's command-line view and ONE
Session Terminal pane on the desktop workbench: input lines are sent down,
per-turn results come back (input and result only — process output never
streams). The carrier (workbench) owns the event log with cursors; the hub
only authenticates and relays, so a faster transport later streams the same
events.
_Avoid_: remote shell, terminal relay, command box

**Command Turn**:
One input line plus its completion result (exit code + output tail) within a
Remote Terminal. Turns are serial — the shell is busy until a turn settles.
_Avoid_: remote command, exec request

**Session Terminal**:
The terminal dock owned by a session tab in the desktop workbench; its cwd is
the session's working directory.
_Avoid_: session command line

**Execution Carrier**:
Where a Remote Terminal's pane lives: the desktop workbench (v1), or an
incubated OS terminal emulator when the workbench is not running (v2 fallback).
_Avoid_: executor

**Result Feedback**:
The completion report of a Command Turn — exit code plus the tail of the
output; process output is never streamed while the command runs.
_Avoid_: output streaming, output relay

**Terminal Mirror**:
The state-only projection of the desktop workbench's open terminal panes onto
a remote client: the pane list, open/close lifecycle, and Busy State sync
live. Pane content (scrollback, running output) never syncs, and control over
a mirrored pane stops at interrupting the running command.
_Avoid_: terminal sync, pane clone, remote desktop terminals

**Busy State**:
One terminal pane's running signal — busy or idle, the current command line,
and when it started; unknown when the pane's shell cannot report it.
_Avoid_: running flag, process status

## Session Re-entry

**Session Re-entry**:
Coming back to a conversation this client already showed — after the phone
wakes, after the connection dropped, or after switching back from another
conversation. The bridge answers with a Tail Replay; the client reconciles it
with the Painted View instead of starting over.
_Avoid_: reload, refresh, re-attach (as words for the user-facing behavior)

**Painted View**:
The conversation as this client last showed it: the messages on screen, or the
snapshot kept when the user switched away. A Re-entry always starts from it.
_Avoid_: cache, stale view

**Settled View**:
A Painted View captured between turns. It is complete by construction, so an
unchanged conversation tail proves nothing was missed — a view captured
mid-turn proves nothing and must be reconciled.
_Avoid_: cached state, final view

**Tail Replay**:
The bridge re-sending a conversation's newest messages when a client attaches.
It reaches back a bounded distance, and it is the only way a client learns
what happened while it was away.
_Avoid_: full replay, history load

**Anchor**:
A message the Painted View and the Tail Replay both carry under an identity
they share: assistant text and thoughts keep the backend's message id from the
live stream into replay, and a tool card keeps its call id. A prompt sent from
this device never anchors — live it is the sender's own bubble, replayed it is
a stored message.
_Avoid_: matching message, common message

**Seam**:
The newest Anchor. Everything above it stays as painted; everything below it
is replaced by the replay.
_Avoid_: merge point, diff position

**Far Behind**:
A Painted View that shares no Anchor with the Tail Replay: more happened while
away than one replay reaches back. Only a Far Behind view is reloaded from
scratch — a reload is the exception, not the re-entry.
_Avoid_: stale, out of date

**In-flight Content**:
The output of the turn that was still running when the client looked away. The
backend records a step's text only once the step finishes, so no replay
contains it yet.
_Avoid_: partial message, streaming leftovers

**Carry-over**:
Keeping In-flight Content through a Re-entry while the turn is still running.
It is only ever done mid-turn: once no turn is running, the replay is the
whole truth.
_Avoid_: preserving draft output, keeping stale messages
