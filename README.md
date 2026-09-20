# zmux

tmux-style panes for Windows, with persistent detachable sessions.
Panes run Git Bash. Built to run on a locked-down corporate laptop.

## Status

Phases 0–6 complete. 137 tests passing, clippy clean.

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
| `src/term.rs` | `vt100` + **terminal query responder** + scrollback. |
| `src/layout.rs` | BSP tiling tree. |
| `src/windows.rs` | Several layouts per session. A window **is** a `Layout`. |
| `src/render.rs` | vt100 cells → composited `Frame` → ratatui `Buffer`. |
| `src/session.rs` | Owns panes, layout and clients. Outlives any connection. |
| `src/persist.rs` | Layout + cwd to `%APPDATA%`. |
| `src/cwd.rs` | OSC 7 parsing and the bash integration snippet. |
| `src/config.rs` | `config.toml`. |

## Seven things that will bite you if you change them

**1. Terminal query replies are mandatory.** ConPTY is created with
`PSEUDOCONSOLE_INHERIT_CURSOR` and emits `ESC[6n` at startup. Unanswered, the
shell never initialises and teardown deadlocks. This reproduced on the first
run of the spike, on Windows 11 build 26200. `term.rs::QueryResponder` handles
it, including queries split across read boundaries.

**2. Never spawn the daemon via WMI.** `DETACHED_PROCESS` is sufficient
(verified). WMI `Win32_Process.Create` is MITRE ATT&CK T1047 and a high-signal
EDR detection on a managed laptop. See `docs/DESIGN.md` §0.2.

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

**7. The waiter thread and `close()` share ONE child handle.** They used to
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

## One more thing that will bite you

**Coalescing and deltas interact.** Frames are coalesced per client: a burst
replaces an undelivered frame rather than queueing. A delta based on a frame
the client never received is refused - and keeps being refused, because
nothing requests a resync. `publish()` therefore sends a FULL frame whenever
it overwrites an occupied slot. Remove that check and rapid input freezes the
display at whatever revision it last managed to apply.
