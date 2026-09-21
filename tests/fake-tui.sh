#!/usr/bin/env bash
# A tiny full-screen application, so `capture --source history` can be tested
# against something that behaves like one.
#
# The three things that make a real agent unreadable by ordinary means, and
# readable by scrolling it:
#   - it draws on the ALTERNATE screen, which has no scrollback
#   - it asks for the mouse, so the wheel reaches it instead of the terminal
#   - it holds more lines than fit, and redraws when scrolled
#
# Deliberately NOT a general TUI: no resize handling, no colour, no cursor
# hiding. Only enough to be scrollable.
#
# usage: fake-tui.sh <visible-rows> [total-lines]

rows=${1:?usage: fake-tui.sh <rows> [total]}
total=${2:-120}

top=$(( total - rows ))
(( top < 0 )) && top=0

cleanup() { printf '\033[?1000l\033[?1006l\033[?1049l'; }
trap cleanup EXIT

# Alternate screen, SGR mouse encoding, button reporting. SGR because it has
# no 223-column ceiling, which is also why zmux prefers it.
printf '\033[?1049h\033[?1006h\033[?1000h'

draw() {
  printf '\033[2J'
  local i r=1
  for (( i = top; i < top + rows; i++ )); do
    # Absolute positioning, never a newline: a newline on the last row would
    # scroll the alternate screen and the line numbers would stop matching
    # the rows, which is the one thing this has to get right.
    printf '\033[%d;1HL%03d' "$r" "$i"
    (( r++ ))
  done
}

draw
# READY goes to the alternate screen too, so the test can tell drawing has
# happened rather than guessing with a sleep.
printf '\033[%d;40HREADY' "$rows"

buf=""
while IFS= read -rsn1 ch; do
  buf+="$ch"
  case "$buf" in
    *[Mm])
      case "$buf" in
        # SGR wheel up is button 64, wheel down 65.
        *'<64;'*) if (( top > 0 )); then (( top -= 1 )); draw; fi ;;
        *'<65;'*) if (( top < total - rows )); then (( top += 1 )); draw; fi ;;
      esac
      buf=""
      ;;
  esac
  # A stray byte must not wedge the parser forever.
  (( ${#buf} > 32 )) && buf=""
done
