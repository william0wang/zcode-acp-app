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
