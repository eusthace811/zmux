# zmux

tmux-style panes for Windows, with persistent detachable sessions.
Panes run Git Bash. Built to run on a locked-down corporate laptop.

## Status

Phases 0–6 complete. 169 tests passing, clippy clean.

| | |
| --- | --- |
| Split panes | horizontal + vertical, BSP tiling, drag-free resize |
| Windows | several screenfuls per session; hidden ones keep running |
| Persistence | detach and reattach; layout and directories survive a daemon restart |
| Scrollback | per pane, with mouse wheel and a scroll mode |
| cwd tracking | OSC 7; new panes inherit the focused pane's directory |
| Multi-client | several clients on one session, sized to the smallest |
| Config | `%APPDATA%\zmux\config.toml`, all optional |
| Keys | xterm encoding: Alt, F1–F12, modified navigation, application cursor mode |
| Activity | the bar names each window after what is running in it, and marks panes that are busy |
| Wire | row deltas, not a full grid per frame |

## Use

```bash
./x release                    # build (see docs/DESIGN.md §7 for why ./x)
./x install                    # ...or build and copy both binaries to ~/bin
```

Then from a **Git Bash window** (installing to `~/bin` for everyday use is in
[docs/GUIDE.md](docs/GUIDE.md), along with every key and setting):

```
zmux                    attach to "default", creating it if needed
zmux work               a named session
zmux ls                 list live sessions
zmux attach work        attach without creating
zmux kill work          terminate a session
zmux config             write a commented default config, print its path
```

A session can also be driven **without attaching**, which is what a script or
a coding agent uses. These return immediately instead of taking over the
terminal:

```
zmux info work                      layout, windows and focus, as JSON
zmux send work split-v              any keybinding, by name
zmux run  work -p 2 "cargo test"    type it into pane 2 and press Enter
zmux keys work -p 2 "text"          type it, no Enter
zmux capture work -p 2 -n 20        that pane's last 20 visible lines
```

`-p N` is a pane's position in the current window and does **not** move the
focus, so a script can work in one pane while someone types in another. Every
mutation answers with the resulting `info`, so a caller learns where its new
pane landed without a second, racy query. `zmux --skill` prints the whole
agent-facing contract.

`info` also reports what each pane is doing — `"command"`, `"busy"` and
`"last_exit"` — so a script can run something and then poll until that pane
goes quiet, instead of sleeping and hoping.

| Key | |
| --- | --- |
| `C-b d` | detach (the session keeps running) |
| `C-b %` / `C-b "` | split left/right, top/bottom |
| `C-b c` | new window |
| `C-b n` / `C-b p` | next / previous window (wraps) |
| `C-b 0`…`C-b 9` | jump to that window — the numbers in the status bar |
| `C-b x` | close pane — and its window, if it was the last one |
| `C-b <arrow>` | move focus |
| `C-b C-<arrow>` | resize |
| `C-b [` | scroll mode (`q`/`Esc` to leave) |
| `C-b C-b` | send a literal `Ctrl-b` |
| mouse | click focuses, wheel scrolls |

## Layout

| File | Role |
| --- | --- |
| `src/protocol.rs` | Wire messages. Versioned, bounded, **append-only** enums. Frames and row deltas. |
| `src/ipc.rs` | Named pipe, length-prefixed framing, restricted ACL. |
| `src/pty.rs` | ConPTY via `portable-pty`; Git Bash fingerprint; reader thread; explicit teardown. |
| `src/term.rs` | `vt100` + **terminal query responder** + scrollback + what each pane is running (OSC 133/633). |
| `src/layout.rs` | BSP tiling tree. |
| `src/windows.rs` | Several layouts per session. A window **is** a `Layout`. |
| `src/render.rs` | vt100 cells → composited `Frame` → ratatui `Buffer`. |
| `src/session.rs` | Owns panes, layout and clients. Outlives any connection. |
| `src/persist.rs` | Layout + cwd to `%APPDATA%`. |
| `src/cwd.rs` | OSC 7 parsing and the bash integration snippet, which also reports what is running. |
| `src/config.rs` | `config.toml`. |
| `src/keys.rs` | Key events → xterm byte sequences, including application cursor mode. |
| `src/harden.rs` | `SetDefaultDllDirectories`, before anything can `LoadLibrary`; and the job-object check behind the daemon-survival warning. |

Everything outside `src/`:

| Path | Role |
| --- | --- |
| `tests/` | Integration tests. They drive a real daemon over the real pipe. |
| `examples/dump.rs` | Prints a live session's screen as text. Needs no console, so it works from WSL. |
| `probes/` | The Phase 0 go/no-go experiments, kept for **re-validation on the work laptop** (DESIGN §8.4): does a detached daemon survive its parent, does mintty give a real console. Separate crates, deliberately outside the build — `probes/RUNBOOK.md` says how to run them and has the table to fill in. |
| `skills/zmux/SKILL.md` | What a coding agent needs to know about zmux. Bundled into the binary, so `zmux --skill` always prints the one that matches the build. |
| `.claude-plugin/` | Manifests that let Claude Code install that skill as a plugin: `claude plugin marketplace add <this repo>`, then `claude plugin install zmux@zmux`. |
| `x` | Build, test and install, from WSL or Git Bash. |
| `docs/` | `GUIDE.md` to use it, `DESIGN.md` for why it is built this way. |

## Nine things that will bite you if you change them

**1. Terminal query replies are mandatory.** ConPTY is created with
`PSEUDOCONSOLE_INHERIT_CURSOR` and emits `ESC[6n` at startup. Unanswered, the
shell never initialises and teardown deadlocks. This reproduced on the first
run of the spike, on Windows 11 build 26200. `term.rs::QueryResponder` handles
it, including queries split across read boundaries.

**2. Never spawn the daemon via WMI.** `DETACHED_PROCESS` is sufficient on this
PC (verified). WMI `Win32_Process.Create` is MITRE ATT&CK T1047 and a
high-signal EDR detection on a managed laptop. See `docs/DESIGN.md` §0.2.
Because `DETACHED_PROCESS` leaves the console but **not** the job object,
`harden::job_kills_members_on_close()` checks for
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` before spawning and warns when the daemon
will not outlive whoever owns that job. It reads the *immediate* job only —
jobs nest and an ancestor cannot be read from inside — so a `true` is
trustworthy and a `false` is not a guarantee. herdr asks the same question and
falls back to WMI; we only report.

**3. Panes launch with `--rcfile`, NOT `-l`.** A login shell *ignores*
`--rcfile`, so `-l` and OSC 7 cwd tracking are mutually exclusive. The rcfile
performs login startup itself; a test asserts git is still on `PATH`. Long
options must also precede short ones — `bash -l --rcfile X` is rejected.

**4. `close()` must drop the pty master before joining.** The reader thread
blocks in `read()` until the pseudoconsole closes. Joining first deadlocks.

**5. `harden::restrict_dll_search_path()` runs before anything else.**
`portable-pty` probes for `conpty.dll` by bare name, and unlike `kernel32.dll`
that is not a KnownDLL — a planted file would execute in-process. A test proves
the mitigation rather than assuming it.

**6. A ConPTY eats the first byte written after a resize.** Not ours: measured
at the pty layer with no daemon and no query replies in flight, 4 of 16
resizes lost it and 0 of 16 did with a sacrificial NUL. It needs a pause after
the resize, so it lands exactly on someone who splits a pane and then types.
`Pane::resize` spends one NUL — the conventional terminal no-op filler — every
time. Remove it and every fourth split eats a keystroke.

**7. A pane's environment is scrubbed on purpose.** `CommandBuilder::new`
merges the two Windows `Environment` registry keys over the process
environment, and a value that is not `REG_SZ`/`REG_EXPAND_SZ` arrives with its
separator NULs intact. One embedded NUL ends the `CreateProcessW` environment
block early, so every variable sorting after that name vanishes — a shell with
no `PATH` — or the call fails outright with `ERROR_INVALID_PARAMETER` (87) and
no pane starts. Group policy writes into exactly those keys, so the managed
laptop is where it is likeliest to fire. `pty::pane_command` clears the merge
and puts this process's own environment back, which is what should reach a
pane anyway. herdr vendors a patched `portable-pty` for the same bug.

**8. The shell integration reports commands from `PS0`, not a `DEBUG` trap.**
A `DEBUG` trap fires for every simple command — including the *other* entries
in `PROMPT_COMMAND`, and Git Bash installs one in `git-prompt.sh` — so the bar
would sit showing somebody's prompt helper as the running command. `PS0` is
expanded exactly once per command line, after it is read and before it runs,
and never for a `PROMPT_COMMAND` entry. Measured against a real Git Bash: the
trap version also reported only `echo a` for `echo a; echo b`, where this
reports the whole line. `__zmux_precmd` is **prepended** to `PROMPT_COMMAND`
for the same class of reason — `$?` has to be the user's command's status, and
anything running before us replaces it.

**9. The waiter thread and `close()` share ONE child handle.** They used to
hold separate slots, and the constructor filled `Pane::child` by *taking* the
child out of the waiter's — so the waiter found `None` on its first tick and
returned, and `PtyEvent::Exited` could never fire for any pane. Typing `exit`
left a dead pane on screen and a session whose last shell had gone lingered
until its idle timeout. Nothing typed `exit` in the suite, so it went unseen;
two tests now do.

## Tests

```bash
./x test
```

Integration tests drive a **real Git Bash through ConPTY** and assert on the
rendered grid. They need no console, so they run from WSL too.

`cargo run --example dump -- <session>` prints a live session's screen as text,
which is the quickest way to see what a running session actually looks like.

`tests/stress.rs` covers the adversarial cases: a client that stops reading,
sustained output, attach/detach churn, many splits, and malformed input.

## Known limitations

- **Pre-24H2 `ClosePseudoConsole`** can block; teardown is bounded and gives up
  rather than hanging, so a pane may be abandoned rather than reaped.
- **A client that handshakes and then stops reading wedges itself** - the pipe
  fills with frames it never drains. The daemon stays healthy, which is what
  matters, but that client will not recover.
- **No server identity check on connect.** The DACL restricts who can use the
  pipe and the name is SID-keyed, but a *hostile* local process could still
  pre-create the name. A named mutex now stops two of OUR OWN daemons racing
  to serve one session, which is the case that actually happened;
  `FILE_FLAG_FIRST_PIPE_INSTANCE` would close the rest, and `interprocess`
  does not expose it.
- **Session names are sanitised lossily**, so `a/b` and `a_b` collide on one
  pipe. The client compares the name it asked for against the one the daemon
  reports and refuses the mismatch, so a collision is an error rather than
  silently joining the wrong session.
- **Windows are positional**, so closing one renumbers those after it — and
  `C-b <digit>` renumbers with them. tmux leaves a gap instead. There are no
  window names, and only the first ten windows have a digit.
- **Queues are bounded by message count, not bytes.**
- **A client that stops reading is contained, not dropped.** Frames coalesce,
  so it costs no memory, and its wedged writer thread takes nothing else with
  it. But a blocking write on a Windows pipe never times out and the serve
  loop only gives up when the writer *finishes*, so the peer stays registered:
  it holds the idle timer open, and its viewport stays in the `min()` that
  sizes the session for everyone. herdr drops such a peer with a write timeout
  that resets whenever sending makes progress
  (`server/client_transport.rs`); the Windows equivalent is a watchdog that
  closes the handle out from under a writer that has not moved.
- **A pane whose shell has stopped reading its input can stall layout, and
  teardown with it.** `Pane::resize` ends in a one-byte write to the pty, and
  a pty write blocks once ConPTY's input pipe is full. Reflow applies resizes
  with the pane map released, so typing into other panes is unaffected, but
  it still holds the geometry lock — so splits, resizes and window switches
  wait, and so does `shutdown`, which needs that lock before it can save and
  drain. A permanently wedged write has always been able to keep the daemon
  from exiting; since the save now waits for the lock rather than racing it,
  a stall also delays the final save rather than overlapping it.

  Fixing this does **not** need a bounded write, as this list previously
  claimed. herdr runs the same blocking `portable-pty` writer on Windows and
  does not stall, because one dedicated thread per pane owns the writer and
  every other path — user input, terminal-query replies, **and resize** —
  posts to it over a channel and returns (`src/pty/actor.rs`, `run_writer`).
  The wedge is then confined to that pane's writer thread instead of reaching
  the geometry lock. Adopting that shape is the open item; it changes input
  from "queued indefinitely" to "refused when the pane is not draining", which
  is a behaviour change, not just a refactor.
- **A control mutation's reply is a snapshot, not a transaction.** It is read
  immediately after the command, but a human attached at the same moment can
  move the focus in between. `panes[].id` survives that; `focus` is only as
  fresh as when it was read.

## One more thing that will bite you

**Coalescing and deltas interact.** Frames are coalesced per client: a burst
replaces an undelivered frame rather than queueing. A delta based on a frame
the client never received is refused - and keeps being refused, because
nothing requests a resync. `publish()` therefore sends a FULL frame whenever
it overwrites an occupied slot. Remove that check and rapid input freezes the
display at whatever revision it last managed to apply.
