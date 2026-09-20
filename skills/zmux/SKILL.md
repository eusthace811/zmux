---
name: zmux
description: "Work with zmux, a tmux-style terminal multiplexer for Windows Git Bash. Use only when the user explicitly mentions zmux - listing or ending sessions, building or installing it, or understanding what a pane shell is running inside. Do not use merely because a task could benefit from a background terminal or a second shell."
---

# zmux

zmux splits a Git Bash window into panes. A background daemon
(`zmuxd.exe`) owns the shells and the layout; the client (`zmux.exe`) is a
terminal attached to it. One daemon per named session.

## The one rule that matters

`zmux`, `zmux <session>` and `zmux attach <session>` **start a
full-screen TUI**. They take over the terminal, enter raw mode and do not
return until the user detaches.

Never run them. In an agent's non-interactive shell they will hang until the
command times out, and everything you type afterwards is swallowed by a
terminal nobody is reading.

Only these subcommands return:

```bash
zmux ls        # live session names, one per line, or "no live sessions"
zmux kill <session>
zmux config    # writes the default config if absent; prints its path
zmux --help
zmux --skill   # this file
```

They are plain text on stdout, not JSON. `ls` is the only way to inspect a
session from outside; there is no protocol-level query for panes, windows or
pane contents (see *What zmux cannot do*).

## Know where you are

Every pane shell inherits two variables from its daemon:

```bash
test "${ZMUX:-}" = 1          # true: this shell is a zmux pane
printf '%s\n' "$ZMUX_SESSION" # the session name that owns it
```

`ZMUX` unset means you are in an ordinary Git Bash window. Both cases are
fine; they change what is safe to do:

- **Inside a pane**, never `zmux kill "$ZMUX_SESSION"` — you would kill
  the session you are running in, and yourself with it. Do not start a nested
  session either; zmux does not block nesting, so it will "work" and leave
  the user with two layouts fighting over one window.
- **Outside**, `zmux ls` tells you what is live before you touch anything.

## Ending a session

`zmux kill <session>` terminates the daemon and every shell in it,
immediately and without asking. Unsaved work in those shells is lost.

Only run it when the user asked for that session by name, or when they asked
for something that cannot proceed otherwise — and then say which sessions you
are about to end and why, before doing it.

The layout is saved, not the processes: a killed session's panes and
directories come back on the next attach, with fresh shells.

## Build and install

`./x` at the repo root is the only supported way to build; it works out which
host it is on. The build needs the Windows MSVC toolchain even when the source
lives in WSL.

```bash
./x test              # cargo test
./x release           # optimised build
./x install [dir]     # release build, then BOTH binaries into dir (~/bin)
./x clippy            # anything else is passed through to cargo
```

Run it from the repo folder. When the repo is on the WSL share, either shell
works — from WSL, or from Git Bash at the `//wsl.localhost/...` path — and
both keep `target/` in the Windows home so they share one build cache.

Do not run `cargo` directly against a repo on the WSL share: the 9p share has
no file locks, and `//wsl.localhost/...` object paths reach `link.exe`, which
reads the leading `/` as an option and fails with *LNK1561: entry point must
be defined*. That failure means `cargo` was used instead of `./x`.

`install` copies **both** binaries, because the client starts the daemon by
looking next to itself. It refuses to run while any `zmux` process is alive,
because Windows will not replace a running `.exe`, and it prints the sessions
to end. Report that list to the user and let them decide — do not kill their
sessions to unblock a build.

## What zmux cannot do

Say so plainly rather than inventing a command:

- **No control CLI.** Splitting, focusing, resizing, switching windows and
  scrolling exist only as `C-b` keystrokes handled inside an attached client.
  There is no `zmux split`, `zmux send-keys`, or equivalent.
- **No output capture.** Nothing reads a pane's screen or scrollback from
  outside. The daemon composites frames for attached clients only.
- **No agent awareness.** zmux does not detect, name or track what runs in
  a pane. `ZMUX_SESSION` is the whole of the introspection surface.
- **No remote.** One machine, named pipes, no SSH.

If a task needs a background process, run it as a background process. Do not
try to drive zmux as a job runner; it is a terminal for a human to type in.

## Where state lives

| | |
| --- | --- |
| `%APPDATA%\zmux\config.toml` | settings; read by the daemon at session creation |
| `%APPDATA%\zmux\session-<name>.json` | saved layout and per-pane directories |
| `\\.\pipe\zmux-<sid>-<session>` | the live session, for exactly as long as the daemon runs |

A config change takes effect for a session only after that session is killed
and started again. `prefix` is the exception: the client reads it, so a new
`zmux` picks it up.

## Failure modes worth recognising

- **"protocol mismatch"** — a daemon from an older build still serves that
  session. It names the session; `zmux kill` it and attach again.
- **Blank panes** — started from WSL or another non-console host. zmux needs
  a real Windows console, which the Git Bash window provides.
- **"Access is denied" during a copy** — a `zmux` process is holding the
  `.exe`. `zmux ls`, then end those sessions.
- **"no such session"** — `zmux attach` never creates. That is deliberate.
