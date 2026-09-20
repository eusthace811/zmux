#!/usr/bin/env bash
#
# The zmux reporting contract, in the smallest form that is still correct.
#
# Source this from an agent's shell wrapper, or copy the three lines that
# matter into the agent itself - which is better, because the agent knows its
# own turn boundaries and a wrapper does not.
#
#   . report-state.sh
#   zmux_report state=working
#   ...
#   zmux_report state=idle
#
# Everything here is a no-op outside a zmux pane, so a script using it still
# runs in an ordinary terminal.

# Monotonic per pane. Two reports arriving out of order would otherwise leave
# the pane reading `working` after it went idle, and zmux drops any report at
# or below the last sequence number it accepted.
__zmux_seq=0

# Report whatever you pass: state=..., agent=..., name=...
#
# Written to /dev/tty rather than stdout ON PURPOSE. A hook or subprocess
# usually has its stdout captured by whatever called it, and a report that
# lands in a captured pipe never reaches the terminal. /dev/tty is the pane.
zmux_report() {
  [ "${ZMUX:-}" = 1 ] || return 0
  __zmux_seq=$((__zmux_seq + 1))
  local params=""
  local kv
  for kv in "$@"; do params="$params;$kv"; done
  printf '\033]1341;zmux%s;seq=%d\033\\' "$params" "$__zmux_seq" > /dev/tty 2>/dev/null
}

# If /dev/tty is not reachable - some sandboxes, some hook runners - fall back
# to the control pipe, which needs zmux on PATH and costs a process per
# report. `zmux report --check` tells you which of the two is working.
zmux_report_via_pipe() {
  [ "${ZMUX:-}" = 1 ] || return 0
  command -v zmux >/dev/null 2>&1 || return 0
  __zmux_seq=$((__zmux_seq + 1))
  zmux report --seq "$__zmux_seq" "$@" >/dev/null 2>&1 || true
}

# ---------------------------------------------------------------------------
# Wiring an agent you own
# ---------------------------------------------------------------------------
#
# Emit the escape at the three moments that change what a caller should do.
# In any language it is one line; this is the shape, not the syntax:
#
#   turn starts          \033]1341;zmux;state=working;seq=N\033\
#   stops to ask         \033]1341;zmux;state=blocked;seq=N\033\
#   turn ends            \033]1341;zmux;state=idle;seq=N\033\
#
# Once at startup, so a caller can see what a pane holds and name it:
#
#   \033]1341;zmux;state=idle;agent=ampere;name=reviewer;seq=1\033\
#
# Notes that save time later:
#
#   - Write it to the terminal the agent already owns. Inside a TUI, do it
#     where you would write any other escape, and redraw afterwards if your
#     renderer tracks the cursor itself.
#   - `blocked` is worth emitting. `zmux wait --until settled` wakes on it,
#     so a caller finds out in a second that you need a person, rather than
#     sitting out its whole timeout.
#   - Do not bother clearing state on exit. zmux clears everything a pane
#     claimed when the foreground command ends (OSC 133;D), so a claim cannot
#     outlive the process that made it.
#   - `agent=` is free text. zmux has no list of known agents to be added to.
#   - If you are testing this by TYPING it into a pane with `zmux run`,
#     write \033 and not \e. `run` unescapes \e itself, so the Escape byte
#     lands in the command line and the shell's line editor eats it before
#     your printf ever runs.
#
# Check it with, from inside the pane:
#
#   zmux report --check
#   zmux info "$ZMUX_SESSION"    # look for "state" on your pane
