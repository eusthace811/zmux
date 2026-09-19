# zmux

tmux-style panes for Windows, with persistent detachable sessions.
Panes run Git Bash. Built to run on a locked-down corporate laptop.

## Status

Phases 0–5 complete. 82 tests passing, clippy clean.

| | |
| --- | --- |
| Split panes | horizontal + vertical, BSP tiling, drag-free resize |
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
```

Then from a **Git Bash window**:

```
zmux                    attach to "default", creating it if needed
zmux work               a named session
zmux ls                 list live sessions
zmux attach work        attach without creating
zmux kill work          terminate a session
```

| Key | |
| --- | --- |
| `C-b d` | detach (the session keeps running) |
| `C-b %` / `C-b "` | split left/right, top/bottom |
| `C-b x` | close pane |
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
| `src/render.rs` | vt100 cells → composited `Frame` → ratatui `Buffer`. |
| `src/session.rs` | Owns panes, layout and clients. Outlives any connection. |
| `src/persist.rs` | Layout + cwd to `%APPDATA%`. |
| `src/cwd.rs` | OSC 7 parsing and the bash integration snippet. |
| `src/config.rs` | `config.toml`. |

## Five things that will bite you if you change them

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
  pipe and the name is SID-keyed, but a local process could still pre-create
  the name. `FILE_FLAG_FIRST_PIPE_INSTANCE` would close this; `interprocess`
  does not expose it.
- **Session names are sanitised lossily**, so `a/b` and `a_b` collide.
- **Queues are bounded by message count, not bytes.**

## One more thing that will bite you

**Coalescing and deltas interact.** Frames are coalesced per client: a burst
replaces an undelivered frame rather than queueing. A delta based on a frame
the client never received is refused - and keeps being refused, because
nothing requests a resync. `publish()` therefore sends a FULL frame whenever
it overwrites an occupied slot. Remove that check and rapid input freezes the
display at whatever revision it last managed to apply.
