# zmux

tmux-style panes for Windows with persistent, detachable sessions.
Panes run Git Bash. See `../zmux/docs/DESIGN.md` for the full plan.

## Status: Phase 1 complete

One full-screen Git Bash pane, client/daemon split, detach-safe.

```
cargo build --release
./target/release/zmux.exe [session]     # run from a Git Bash window
```

`Ctrl-b d` detaches. Run again to reattach; the shell kept running.

## Layout

| File | Role |
| --- | --- |
| `src/protocol.rs` | Wire messages. Versioned, bounded, **append-only** enums. |
| `src/ipc.rs` | Named pipe + length-prefixed framing. Adapted from herdr. |
| `src/pty.rs` | ConPTY via `portable-pty`; Git Bash fingerprint; reader thread. |
| `src/term.rs` | `vt100` parser + **terminal query responder**. |
| `src/render.rs` | vt100 cells -> wire Frame -> ratatui Buffer. |
| `src/bin/zmuxd.rs` | Daemon: owns ConPTY + terminal state. |
| `src/bin/zmux.rs` | Client: draws frames, forwards keys. |

## Two things that will bite you if you change them

**1. Terminal query replies are mandatory.** ConPTY is created with
`PSEUDOCONSOLE_INHERIT_CURSOR` and emits `ESC[6n` at startup. Leave it
unanswered and the shell never initialises and teardown deadlocks. This
reproduced on the first run of the spike, on Windows 11 build 26200.
`term.rs::QueryResponder` handles it, including queries split across read
boundaries. `tests/smoke.rs` fails if you break it.

**2. Never use WMI to spawn the daemon.** `DETACHED_PROCESS` is sufficient
(verified). WMI `Win32_Process.Create` is MITRE ATT&CK T1047 and a high-signal
EDR detection on a managed laptop. See DESIGN.md §0.2.

## Tests

```
cargo test
```

`tests/smoke.rs` drives a real Git Bash through the daemon and asserts the
rendered grid - no console needed, so it runs from WSL too.
