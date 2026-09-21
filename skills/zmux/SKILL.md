---
name: zmux
description: "Drive zmux, a tmux-style terminal multiplexer for Windows Git Bash. Use when the user mentions zmux, or asks to inspect or control its panes, windows or sessions - splitting panes, running a command in one, starting or coordinating another agent in one, reading a pane's output, waiting for it to finish, building or installing it. Do not use merely because a task could benefit from a background terminal, a second shell, or delegation."
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
printf '%s\n' "$ZMUX_PANE"     # this pane's id
```

`ZMUX_PANE` is an **id**, not a position. Positions renumber when a pane
closes; ids never do and are never reused. `-p current` resolves to it.

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
  "window": 0, "windows": 3, "focus": 2, "focus_id": 7, "created": null,
  "panes": [
    {"position": 1, "window": 0, "id": 1, "cols": 59, "rows": 39,
     "cwd": "C:\\work", "command": "cargo test", "busy": true, "last_exit": null,
     "state": "unknown", "agent": null, "name": null,
     "quiet_ms": 84, "bells": 0, "title": null},
    {"position": 2, "window": 0, "id": 7, "cols": 60, "rows": 39,
     "cwd": "C:\\work\\api", "command": "ampere", "busy": true, "last_exit": 0,
     "state": "blocked", "agent": "ampere", "name": "reviewer",
     "quiet_ms": 11402, "bells": 1, "title": "ampere - waiting"}
  ]
}
```

**Addressing**

- `panes` covers **every window**, each pane tagged with the `window` it is
  in. You do not have to switch windows to see what is in one — and must not,
  because switching moves the user's screen.
- `position` is 1-based **within its own window**, in the order the status bar
  counts. It MOVES when a pane closes.
- `id` is stable for the pane's lifetime and never reused. Prefer it for
  anything that outlives one command: pass it as `-p @7`.
- `focus` is a position in the current window; `focus_id` is the same pane's
  id, and is the unambiguous one.
- `created` is the pane a command just made, and `null` otherwise. It is the
  only way to learn what `split-v-bg` produced, since the focus did not move.

**What the shell reported** (needs shell integration; a hint, not a contract)

- `cwd` — last directory reported, absent if none.
- `command` — the last command line the shell started. It stays after the
  command ends, so `busy: false` with a `command` and a `last_exit` reads as
  "that ran, and this is how it went". It is also how you tell whether a pane
  is running an agent at all.
- `busy` — running something **right now**.
- `last_exit` — that command's status. `0` is success, `null` means nothing has
  finished here yet.

**What the pane said about itself** — see *Agent state*, below

- `state` — `unknown`, `idle`, `working`, `blocked` or `done`.
- `agent` — what it says is running. Free text.
- `name` — a handle it claimed; usable as `-p <name>`.

**What zmux measured** — facts, not conclusions

- `quiet_ms` — milliseconds since the pane produced any output.
- `bells` — how many times it has rung the bell, ever. Compare against an
  earlier reading; an increase means it asked for attention.
- `title` — the window title the pane set, verbatim.

Re-read `info` rather than reusing a **position** across anything that could
have changed the layout. An `id` needs no such care.

## Change the layout

```bash
zmux send <session> <command> [n]
zmux send <session>              # prints the command list
```

`split-h` `split-v` `split-h-bg` `split-v-bg` `close`,
`focus-left|right|up|down`, `resize-left|right|up|down`, `new-window`
`next-window` `prev-window` `select-window <index>`, `scroll-up`
`scroll-down` `page-up` `page-down` `scroll-bottom`.

Every mutation replies with the resulting `info`, so one call tells you where
the new pane landed — read `focus` from that reply instead of querying again
and racing the user.

A command that could **not** be carried out is an error, not a reply with an
unchanged `info`: a split with no room for another pane, or `close` on the
session's last one. Check the exit status rather than diffing the layout.

That reply is a snapshot taken right after the command, not a transaction. If
somebody is attached and moving focus at that moment, `focus` can already be
theirs rather than yours. When it matters, note the new pane's `id` from the
reply and find it again by `id`, which does not move.

```bash
NEW=$(zmux send work split-v | python -c 'import json,sys;print(json.load(sys.stdin)["focus"])')
```

`split-h` and `split-v` inherit the focused pane's directory and **take the
focus**, exactly as `C-b %` does — which moves the user's cursor mid-sentence.

Use `split-h-bg` / `split-v-bg` for a pane you are making to work in. They
leave the focus alone, and the reply's `created` names the pane they made:

```bash
NEW=$(zmux send work split-v-bg | python -c 'import json,sys;print(json.load(sys.stdin)["created"])')
```

Prefer splitting the pane you are in, and only when you actually need a pane.
Do not create a window or a session unless the user asked for that shape.

`close` closes the **focused** pane, and is refused when it is the session's
last one. Never close a pane you did not create.

## Run something in a pane

```bash
zmux run  <session> -p @7 "cargo test"   # types it and presses Enter
zmux keys <session> -p @7 "some text"    # types it, no Enter
zmux run  <session> -p @7 --paste "a multi-line prompt..."
```

`-p` targets a pane **without moving focus**, which is what makes background
work possible: the user keeps typing where they were. Omit it for the focused
pane. It takes four things:

| | |
| --- | --- |
| `3` | position in the current window |
| `@3` | pane id — the stable one, and the only one that reaches a background window |
| `reviewer` | a name the pane claimed for itself |
| `current` | the pane you are running in (`$ZMUX_PANE`) |

Use `@id` for anything that outlives a single command.

**`--paste` for a prompt.** Anything with a text box — an agent, an editor —
treats every newline as a submit, so a three-line prompt without `--paste`
becomes three turns, the first two of them fragments. zmux wraps the text
only when that pane's program actually asked for bracketed paste, so
`--paste` at a plain shell prompt is harmless. The text and Enter go in one
write, so the program cannot see the prompt without the keystroke that sends
it.

Escapes in the text: `\r` Enter, `\n`, `\t`, `\e` Escape, `\xNN` any byte —
`\x03` is Ctrl-C. Anything else after a backslash is left alone, so Windows
paths survive.

That unescaping happens **before** the pane sees the text, which matters if
the command you are sending contains escapes of its own. `run ... 'printf
"\e]1341;..."'` sends a raw Escape byte into the middle of the command line,
where the shell's line editor eats it. Write `\033` instead: zmux leaves it
alone, and the pane's own `printf` turns it into the escape.

Asking for a pane that does not exist is **refused**, not redirected — a
wrong `-p` never lands in the wrong shell.

## Wait for a pane

```bash
zmux wait <session> -p @7 --until settled --timeout 120000
```

| `--until` | |
| --- | --- |
| `settled` | it reported `idle`, `done` **or** `blocked`. The usual one |
| `idle` `working` `blocked` `done` | one specific reported state |
| `quiet` | no shell command running. Right for `cargo test`; **useless for an agent**, which is one long command |
| `quiet-for MS` | no output for that long. The honest "probably finished" for a pane that reports nothing |
| `output TEXT` | a literal substring of recent output. Text already there matches at once |
| `bell` | the pane rang the bell |

Prints the session state the wait ended with, plus `outcome`: `met`, `timeout`
or `gone`. **Only `met` exits 0**, so `zmux wait … && zmux capture …` does
what it looks like.

- `timeout` is a result, not a failure — the pane is still going. It does not
  prove your input was never delivered, so do not resend it blindly.
- `gone` means the pane exited. The wait follows the pane it resolved, so a
  replacement in the same position cannot answer for it.
- `--settle` defaults to 1.5s and exists for a race you will otherwise hit on
  your first try: run a command and wait for `quiet` immediately, and the
  shell has not read the line yet, so you get the quiet **before** your
  command. Pass `--settle 0` only when the pane is already working.

## Read a pane's output

```bash
zmux capture <session> -p @7                              # visible screen
zmux capture <session> -p @7 -n 20                        # its last 20 lines
zmux capture <session> -p @7 --source unwrapped -n 200    # a transcript
```

| `--source` | |
| --- | --- |
| `screen` | the rows on screen (the default) |
| `recent` | recent output, reaching back into scrollback |
| `unwrapped` | the same, with lines joined where the pane was too narrow to hold them |

**Use `unwrapped` to read an answer.** A pane is only as wide as its share of
the window, so long lines come back broken at that width, and those breaks are
the layout rather than the text.

Nothing about `capture` changes what the user sees, including the scrollback
sources: the pane's scroll position is put back before anyone can observe it.

If a larger read still does not show the whole answer — an alternate-screen
TUI keeps its own history that no terminal can recover — ask the agent to
write the answer to a file and reply with the path, then read that file
directly. Use this as a fallback, not in the first prompt.

## Agent state

zmux never guesses what a pane is doing by reading its screen. `state` is
what the pane **said**, through one escape written to its own terminal:

```
ESC ] 1341 ; zmux ; state=working ; agent=ampere ; name=reviewer ; seq=12 ESC \
```

`examples/report-state.sh` in the repo is a shell helper plus the few lines to
add to an agent you own. From a script whose output is captured:

```bash
zmux report --state working --agent ampere   # defaults to YOUR pane
zmux report --check                          # does the channel work here?
```

Rules worth knowing before you act on a state:

- **`unknown` means nobody said.** It does **not** mean idle, and it does not
  mean done. Most panes are `unknown`, because most programs do not report.
- **`blocked` means it stopped to ask a person.** Sending it a prompt answers
  the wrong question. Read the pane and ask the user.
- A claim is cleared when the command that made it ends, so a `name` lives
  exactly as long as the process that claimed it.
- A pane's report is only as trustworthy as what runs in that pane. Hold an
  `@id` for anything that matters; no report can influence one.
- Report only for your own pane. Reporting for somebody else's makes their
  state a lie.

**For a pane that reports nothing**, use what zmux measured instead:
`command` says whether an agent is running there at all, `quiet_ms` how long
it has been silent, `bells` whether it asked for attention, and `title` what
it says it is doing. `--until quiet-for 3000` is the usual substitute for
`settled`.

## Coordinating another agent

Putting it together, from inside a pane, without moving the user's cursor:

```bash
S=$ZMUX_SESSION
NEW=$(zmux send "$S" split-v-bg | python -c 'import json,sys;print(json.load(sys.stdin)["created"])')
zmux run  "$S" -p "@$NEW" "ampere"
zmux wait "$S" -p "@$NEW" --until settled --timeout 60000   # it came up
zmux run  "$S" -p "@$NEW" --paste "Review the current diff. Report only what is actionable."
zmux wait "$S" -p "@$NEW" --until settled --timeout 600000 &&
  zmux capture "$S" -p "@$NEW" --source unwrapped -n 200
```

- Check `state` before prompting. Prompting a `blocked` pane answers its
  question instead of asking yours.
- A successful `run` means the bytes were written. It does not prove the agent
  started a turn — that is what the wait is for.
- Do not start an agent in a pane that is already running one. `info` tells
  you: `command`, `agent` and `busy`.

## Safety

- Never attach. Never run bare `zmux`, `zmux <session>` or `zmux attach`.
- Never `zmux kill` a session the user did not name — it ends every shell in
  it immediately, losing whatever was running. Inside a pane, killing
  `$ZMUX_SESSION` kills you too.
- Do not close panes or windows you did not create.
- Take targets from `info`, never from the sidebar-free guess that pane 2 is
  "the one on the right" — it is whatever `info` says it is.
- Prefer `-p` over focus commands, and `-bg` splits over plain ones. Moving
  focus moves the user's cursor.
- Report state only for your own pane.
- The daemon runs the user's real shells, in the user's real directories.
  Treat a pane as their terminal, because it is.

## What zmux still does not do

Say so rather than inventing a command:

- **No agent detection.** zmux does not read a pane's screen to work out
  what is running in it or whether it has finished. A program that does not
  report is `unknown`, and `unknown` is not a synonym for done.
- **No event subscription.** `zmux wait` is the only way to block on a
  pane; there is no stream of changes.
- **No regex.** `--until output` takes a literal substring. Pipe `capture`
  through `grep` for anything more.
- **No pane creation inside a background window**, and no way to start a
  session from the CLI — a session begins when someone attaches.
- **No remote.** One machine, named pipes, no SSH.
- **Nothing installs or edits an agent's configuration.** Reporting is wired
  up by hand, once; see `examples/report-state.sh`.

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
- **`state` is always `unknown`** — nothing in that pane reports. Expected
  unless the program was wired up for it. Use `quiet_ms`, `command` and
  `bells`; `zmux report --check` says whether the channel itself works.
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
| `$ZMUX_PANE` | this pane's id, set by the daemon when the pane is created |
| `%APPDATA%\zmux\session-<name>.json` | saved layout and per-pane directories |
| `\\.\pipe\zmux-<sid>-<session>` | the live session, for exactly as long as the daemon runs |

A config change takes effect for a session only after it is killed and started
again. `prefix` is the exception: the client reads it at startup.
