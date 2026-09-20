---
name: zmux
description: "Drive zmux, a tmux-style terminal multiplexer for Windows Git Bash. Use when the user mentions zmux, or asks to inspect or control its panes, windows or sessions - splitting panes, running a command in one, reading a pane's output, building or installing it. Do not use merely because a task could benefit from a background terminal or a second shell."
---

# zmux

zmux splits a Git Bash window into panes. A background daemon
(`zmuxd.exe`) owns the shells and the layout; the client (`zmux.exe`) is a
terminal attached to it. One daemon per named session.

A session can be driven from outside without attaching, which is what this
skill is for.

## The one rule that matters

`zmux`, `zmux <session>` and `zmux attach <session>` **start a
full-screen TUI**. They take over the terminal, enter raw mode and do not
return until the user detaches.

Never run them. In a non-interactive shell they hang until the command times
out, and everything typed afterwards is swallowed by a terminal nobody is
reading.

Every command below returns immediately instead.

## Find the session

```bash
zmux ls          # live session names, one per line, or "no live sessions"
```

Inside a pane, the shell already knows:

```bash
test "${ZMUX:-}" = 1           # true: this shell IS a zmux pane
printf '%s\n' "$ZMUX_SESSION"  # the session that owns it
```

`ZMUX` unset means an ordinary Git Bash window; then `zmux ls` is the only
way to find a session, and if it lists none there is nothing to drive. Do not
create a session to have something to work with — starting one needs the TUI.

## Read the state

```bash
zmux info <session>
```

JSON, and the authority for everything else:

```json
{
  "session": "work", "cols": 120, "rows": 40,
  "window": 0, "windows": 3, "focus": 2,
  "panes": [
    {"position": 1, "id": 1, "cols": 59, "rows": 39, "cwd": "C:\\work"},
    {"position": 2, "id": 7, "cols": 60, "rows": 39, "cwd": "C:\\work\\api"}
  ]
}
```

- `panes` holds the **current window only** — a background window's panes are
  not listed, and not reachable until you switch to it.
- **`position` is what every command takes**, 1-based, in the order the status
  bar counts them. `id` is the daemon's internal handle; never pass it as a
  position, and never assume the two match — after a few splits and closes
  they diverge.
- `focus` is the position of the focused pane, i.e. where the user's
  keystrokes go.
- `window` is 0-based, matching the status bar and `select-window`.
- `cwd` is the last directory the shell reported; absent if it reported none.

Positions shift when a pane closes. Re-read `info` rather than reusing a
position across anything that could have changed the layout.

## Change the layout

```bash
zmux send <session> <command> [n]
zmux send <session>              # prints the command list
```

`split-h` `split-v` `close`, `focus-left|right|up|down`,
`resize-left|right|up|down`, `new-window` `next-window` `prev-window`
`select-window <index>`, `scroll-up` `scroll-down` `page-up` `page-down`
`scroll-bottom`.

Every mutation replies with the resulting `info`, so one call tells you where
the new pane landed — read `focus` from that reply instead of querying again
and racing the user.

That reply is a snapshot taken right after the command, not a transaction. If
somebody is attached and moving focus at that moment, `focus` can already be
theirs rather than yours. When it matters, note the new pane's `id` from the
reply and find it again by `id`, which does not move.

```bash
NEW=$(zmux send work split-v | python -c 'import json,sys;print(json.load(sys.stdin)["focus"])')
```

A split inherits the focused pane's directory and **takes the focus**, exactly
as `C-b "` does. That moves the user's cursor. Split only when the user asked
for a pane; to run something in the background, use a pane that already
exists.

`close` closes the **focused** pane. Never close a pane you did not create.

## Run something in a pane

```bash
zmux run <session> -p 2 "cargo test"     # types it and presses Enter
zmux keys <session> -p 2 "some text"     # types it, no Enter
```

`-p N` targets a pane by position **without moving focus**, which is what
makes background work possible: the user keeps typing where they were. Omit
`-p` to target the focused pane.

Escapes in the text: `\r` Enter, `\n`, `\t`, `\e` Escape, `\xNN` any byte —
`\x03` is Ctrl-C. Anything else after a backslash is left alone, so Windows
paths survive.

There is no "wait until it finishes": a pane is a shell, and zmux does not
track what is running in it. Poll `capture` for something the command prints,
or for the prompt returning. A command that needs a machine-readable result is
better told to write a file that you then read directly.

Asking for a pane that does not exist is **refused**, not redirected — a
wrong `-p` never lands in the wrong shell.

## Read a pane's output

```bash
zmux capture <session> -p 2            # that pane's visible screen
zmux capture <session> -p 2 -n 20      # its last 20 lines
zmux capture <session>                 # the focused pane
```

Plain text. Only what is **on screen**: scrollback is not captured, so output
that scrolled past is gone. A pane is only as wide as its share of the window
— long lines wrapped at that width, and the wrapping is in the text.

Nothing about `capture` changes what the user sees; it does not move the
pane's scroll position.

## Safety

- Never attach. Never run bare `zmux`, `zmux <session>` or `zmux attach`.
- Never `zmux kill` a session the user did not name — it ends every shell in
  it immediately, losing whatever was running. Inside a pane, killing
  `$ZMUX_SESSION` kills you too.
- Do not close panes or windows you did not create.
- Take positions from `info`, never from the sidebar-free guess that pane 2 is
  "the one on the right" — it is whatever `info` says it is.
- Prefer `-p N` over focus commands. Moving focus moves the user's cursor.
- The daemon runs the user's real shells, in the user's real directories.
  Treat a pane as their terminal, because it is.

## What zmux still does not do

Say so rather than inventing a command:

- **No scrollback capture** — visible rows only.
- **No agent awareness.** zmux does not detect, name or track what runs in a
  pane. There is no lifecycle state, no "is it done".
- **No pane creation without focus change**, and no way to reach a background
  window's panes without switching to it.
- **No remote.** One machine, named pipes, no SSH.
- **No session creation** from the CLI. A session begins when someone attaches.

## Build and install

`./x` at the repo root is the only supported way to build.

```bash
./x test              # cargo test
./x release           # optimised build
./x install [dir]     # release build, then BOTH binaries into dir (~/bin)
./x clippy            # anything else is passed through to cargo
```

Run it from the repo folder, in WSL or in Git Bash; when the repo is on the
WSL share both work and share one build cache. Do not run `cargo` directly
against a repo on that share: `target/` would land on 9p, which has no file
locks, and `//wsl.localhost/...` object paths reach `link.exe`, which reads
the leading `/` as an option and fails with *LNK1561: entry point must be
defined*.

`install` copies **both** binaries, because the client starts the daemon by
looking next to itself. It refuses while any `zmux` process is alive —
Windows will not replace a running `.exe` — and prints the sessions to end.
Report that list to the user; do not kill their sessions to unblock a build.

## Failure modes worth recognising

- **"the daemon ... did not answer ... probably an older build"** — the
  session predates the control connection (protocol v5). It must be killed and
  started again; a running daemon cannot be upgraded in place.
- **"protocol mismatch"** on attach — same cause, reported by the client.
- **Blank panes** — started from WSL or another non-console host. zmux needs
  a real Windows console, which the Git Bash window provides.
- **"Access is denied" during a copy** — a `zmux` process is holding the
  `.exe`.
- **"no such session"** — `zmux attach` never creates. That is deliberate.

## Where state lives

| | |
| --- | --- |
| `%APPDATA%\zmux\config.toml` | settings; read by the daemon at session creation |
| `%APPDATA%\zmux\session-<name>.json` | saved layout and per-pane directories |
| `\\.\pipe\zmux-<sid>-<session>` | the live session, for exactly as long as the daemon runs |

A config change takes effect for a session only after it is killed and started
again. `prefix` is the exception: the client reads it at startup.
