# zmux — design and implementation record (v5)

tmux-style panes for Windows with persistent, detachable sessions.
Git Bash is the pane shell. A locked-down corporate laptop is the runtime
target.

Rule: **do what herdr does, reuse herdr's code, build nothing new unless
forced.** Divergences are justified inline.

> **v5 — BUILT.** Phases 0–5 are implemented and 57 tests pass. This document
> is now a record rather than a plan: it keeps the reasoning, the measurements,
> and the conclusions that measurement overturned (marked **[CORRECTED]**),
> because the reasoning is what makes the code changeable later. See
> `../README.md` for how to use what was built.

## Plain summary

**What we're building:** tmux for Windows. Split one terminal window into
several panes, each running Git Bash. Close the window, reopen it later, and
your panes are still there with everything still running.

**How it works — two programs, not one:**

Think of a TV studio and a TV set. The *studio* keeps broadcasting whether or
not anyone is watching; the *TV* just shows the picture and can be switched off
and on freely.

- **`zmuxd.exe` — the studio (daemon).** Runs invisibly in the background.
  It owns the actual Git Bash sessions and remembers the pane layout. It keeps
  running after you close your terminal window. This is what makes sessions
  persistent.
- **`zmux.exe` — the TV (client).** What you actually run. It draws the panes
  on screen and sends your keystrokes to the studio. Closing it loses nothing.

They talk over a **named pipe** — Windows' version of a private local channel
between two programs.

**The four building blocks, and why we didn't write them:**

| Piece | What it does | Why not build it |
| --- | --- | --- |
| `portable-pty` | Starts Git Bash and captures its output | herdr uses it too. Writing raw ConPTY is re-solving a solved problem. |
| `vt100` | Understands terminal output — turns the raw bytes a program prints into "row 5, column 10 is a red X" | Needed because to draw several panes side by side, you must know what each one *looks like*, not just its bytes |
| `ratatui` | Arranges panes on screen and works out what changed since last frame, so we only redraw that | Redrawing everything every frame flickers |
| herdr's code | Battle-tested Windows-specific bits | Someone already hit these bugs so we don't have to |

**Where Git Bash fits — two different questions people confuse:**

1. *What runs inside each pane?* **Git Bash.** Settled, that's the whole point.
2. *Where do you type `zmux` to start it?* **The Git Bash window.** Probe D
   proved mintty gives a real console, so no workarounds are needed (§0.1).

**Phase 0 status: green enough to start Phase 1.** Dependencies available,
ConPTY + Git Bash works, mintty is a usable host, and the background program
survives on this machine. One item carries a caveat: persistence is confirmed
*here* but must be re-confirmed on the work laptop and under Windows Terminal
(§0.2).

---

## 0. Decisions

### 0.1 Host terminal: **mintty. Your original choice was right.** ✅ RESOLVED

v2 overrode your preference for running inside the Git Bash window. **That
override was wrong and is reverted.** Probe D, 2026-09-19, zero-dependency
native Rust binary run inside a real mintty window:

```
MSYS="" TERM="xterm-256color"
GetConsoleWindow      : non-null
GetConsoleMode(stdin) : OK 0x01f7 VT_INPUT=false
GetConsoleMode(stdout): OK 0x0007 VT_OUTPUT=true
console size          : 100x27          <- matches the mintty window
>> REAL CONSOLE - crossterm raw mode WILL work.
```

**mintty gives a native `.exe` a real console.** MSYS2 ConPTY bridging is on
**by default** — identical results with `MSYS` unset and with
`MSYS=enable_pcon`, confirming the Oct-2022 MSYS2 default is live in Git for
Windows 2.55.

All of v2's mintty workarounds are **deleted, not deferred**:

- No `stty raw -echo` bash shim.
- No `CSI 18t` size polling — `GetConsoleScreenBufferInfo` works.
- No winpty. Measured: winpty is *worse* — VT_OUTPUT=false under it.
- Harvested `input/parse.rs` / `input/encode.rs` work as designed, on a real
  console, exactly as herdr intended.

Detail: `VT_INPUT=false` (mode `0x01f7` lacks `ENABLE_VIRTUAL_TERMINAL_INPUT`).
We set it via `SetConsoleMode`, as herdr does at `client/terminal_setup.rs:259`.
`GetConsoleMode` succeeding means `SetConsoleMode` will too.

> **Methodology warning.** Two earlier attempts returned a false "NO_CONSOLE".
> Causes: (1) shell-redirecting the probe's stdout to a file, which *makes*
> stdout a pipe and destroys the thing being measured; (2) `MSYS=enable_pcon`
> set in WSL never reaches a Windows process without `WSLENV`. The probe must
> write its own report file and leave stdio attached.

Windows Terminal stays supported, tested second.


### 0.2 No WMI. ✅ PASS here — ⚠️ must be re-confirmed on the work laptop

herdr escapes the console job object with WMI `Win32_Process.Create`
(`windows.rs:1207`). On a managed laptop that is MITRE ATT&CK **T1047**, a
high-signal EDR detection. Policy: do not use it.

**Result, 2026-09-19 — with the mechanism now understood, not just observed:**

```
SPAWNER job: NOT in any job
DAEMON  job: NOT in any job
...daemon keeps heart-beating after the terminal dies
```

On this Windows 11 machine, launching Git Bash — through `mintty.exe`
directly **and** through the `git-bash.exe` launcher that the Start-menu
shortcut uses — puts processes in **no job object at all**. With no job, there
is no `KILL_ON_JOB_CLOSE`, so plain `DETACHED_PROCESS` survives trivially.
`CREATE_BREAKAWAY_FROM_JOB` is also permitted as a fallback rung. **The WMI
rung is not needed here.**

> **CORRECTION — my first Probe C run was a vacuous test.** I originally killed
> the terminal with `taskkill /F`, saw the daemon survive, and declared this
> resolved. Codex (`gpt-5.6-sol`) pointed out the flaw and was right:
> `KILL_ON_JOB_CLOSE` fires only when the **last handle** to the job closes, so
> `taskkill /F` proves nothing unless that exact process held the last handle —
> and `/F` skips the terminal's cleanup path, which can produce a **false
> survival**. Adding `IsProcessInJob` instrumentation showed the test never
> exercised the hazard at all: there was no job.
>
> The conclusion happens to stand, but only because of *why*, which observation
> alone could not have told us. Keep the instrumentation; it is the deliverable.

**What is still unverified — do not treat this as closed:**

| Gap | Why it matters |
| --- | --- |
| **Work laptop** | Group policy, AppLocker or EDR can impose job objects that a personal PC does not have. This is exactly the configuration herdr's WMI path exists for. |
| **Windows Terminal as host** | Not tested. `wt.exe` brokers the request to an existing WT process, so a WSL-launched test does not reflect the real job chain (Codex predicted this; it reproduced). |
| **Hand-clicked window close** | `CloseMainWindow()` was attempted; mintty ignored WM_CLOSE while a foreground command was running. A graceful close still deserves one manual confirmation. |

**`probe-c.exe` now self-diagnoses** — it reports `IsProcessInJob` plus
`KILL_ON_JOB_CLOSE` / `BREAKAWAY_OK` / `SILENT_BREAKAWAY` flags. On the work
laptop you get the answer directly instead of inferring it from survival. Note
the standing caveat: `QueryInformationJobObject(NULL, …)` reports only the
**immediate** job, so an ancestor carrying `KILL_ON_JOB_CLOSE` still yields a
false negative.

### 0.3 VT engine: `vt100` 0.16.2 — verified

herdr's engine is `libghostty-vt`, vendored Zig, built by `build.rs` calling
`zig build`. A Zig toolchain on a locked-down laptop is a non-starter. Verified
against crates.io:

| Crate | Verdict |
| --- | --- |
| **`vt100` 0.16.2** | **Chosen.** 3 runtime deps (`itoa`, `unicode-width`, `vte`), no platform code, no build script. |
| `alacritty_terminal` 0.26.0 | Rejected — 17 runtime deps incl. its own PTY stack (`rustix-openpty`, `miow`, `polling`). Fights `portable-pty`. |
| `wezterm-term` | **Does not exist on crates.io** (`{"errors":[{"detail":"crate \`wezterm-term\` does not exist"}]}`). Only wezterm's *support* crates are published. It therefore cannot be in Artifactory either — a crates.io remote is a caching proxy. The only published relative is `tattoy-wezterm-term 0.1.0-fork.5`, an unofficial prerelease fork: worse for a security review than `vt100`. |

**`vt100` is adequate, not complete.** Honest gaps (`src/cell.rs`):

- Cells preserve bold/dim/italic/underline/inverse/colors and basic wide state.
  **Not** underline *styles*, blink, hidden, or strikeout.
- A cell stores ≤22 UTF-8 bytes and takes width from its first character, so
  complex emoji/ZWJ clusters are not handled like a modern engine.
- No `KeyboardProtocol` type — Kitty keyboard push/pop must be tracked from
  `unhandled_csi` ourselves before calling herdr's `encode_terminal_key`.
- `Callbacks` gives `unhandled_osc`, `set_window_title`, `audible_bell`,
  `unhandled_csi`. Useful, but OSC 52 clipboard and OSC 8 hyperlinks are a
  **security decision**, not a free feature. Decide explicitly.

Good enough for shells and TUI programs. Not a Ghostty replacement.

### 0.4 Rendering: ratatui composites and diffs; do not harvest the blitter **[CORRECTED]**

v2 claimed `vt100`'s `rows_diff` emits absolute CUP in pane-local coordinates
and therefore cannot composite. **That is wrong.** Verified in upstream source:

```rust
// vt100-0.16.2 src/screen.rs:355  — rows_diff call site
row.write_contents_diff(&mut contents, prev_row, start, width, i,
    false, false,
    crate::grid::Pos { row: i, col: start },   // <-- cursor starts at row start
    crate::attrs::Attrs::default());
```

Each row's virtual cursor is seeded at that row's own start, and the column
loop only ever advances forward (the deferred-`erase` flush targets `prev_col`,
which is always ≥ the last written column). So `MoveFromTo` always takes
`from.row == to.row && from.col < to.col` → **`MoveRight` (`ESC[nC`, relative)**.
The absolute `MoveTo` branch is unreachable. `rows_diff` **is** offset-safe.

`contents_diff` (whole-screen) is *not* offset-safe — that part v2 had right.
It also does not restore the application cursor; it writes cell and attribute
differences only, never `cursor_state_formatted()`.

**So a row-positioned renderer is viable.** But we are not using it, for one
concrete reason: **`rows_diff` emits pre-baked ANSI, so you cannot overlay
anything on it.** Copy-mode selection highlight, search highlight and the drawn
cursor are overlays. Phase 4 needs them. Building on `rows_diff` means
rewriting at exactly the point the tool becomes useful.

**Chosen pipeline — and it needs no harvested blitter at all:**

```
vt100::Screen (per pane)
   │  screen.cell(row, col)        ← read cells, never vt100's ANSI output
   ▼
ratatui::Buffer                    ← panes into sub-Rects, borders + status as widgets,
   │                                 selection/cursor as style overrides on cells
   ▼
Buffer::diff(&prev) → changed cells only     (ratatui-core 0.1.2, public API)
   ▼
crossterm backend → terminal
```

`Buffer::diff()`, `diff_iter()`, `cell_mut()` and `set_style()` are all public.
**ratatui already is the diff-and-blit engine.** herdr wrote `render_ansi.rs`
because it ships frames over a wire to a thin client and adds OSC 8 hyperlinks,
Kitty graphics and drawn cursors — none of which we need in v1.

**Consequence: `render_ansi.rs` comes off the harvest list** (~1,030 production
lines). Revisit only if ratatui's blitting flickers, in which case the fix is
likely just wrapping the draw in synchronized output (`ESC[?2026h` / `l`).

Two contracts that path still imposes, both learned from herdr:

- **Disable host autowrap** (`ESC[?7l`). herdr notes "clients disable host
  autowrap" at `render_ansi.rs:986`, set up separately in
  `client/terminal_setup.rs:114`. Without it, writing the last column of a full
  pane wraps the cursor and corrupts the next row.
- **Draw our own cursor** as a reverse-video cell. ConPTY exposes intermediate
  cursor positions during repaint; herdr confirmed this is unfixable and
  reproduces in every Windows multiplexer.

---

## 1. Phase 0 — probe before building (1 day, was ½)

No production code. **Development happens on a personal Windows 11 PC; the
result is handed to the work laptop later (§8).** That splits the probes in
two:

- **Answerable now** (C, D, E) — run them, they gate the design.
- **A is ANSWERED** (2026-09-19): the corporate Artifactory carries a Cargo
  remote and **all 76 crates in the closure resolve**, including
  `shared_library`. It is a caching proxy, not a curated allowlist. Per-crate
  availability is no longer a question and §6.1's tiering is now only a
  historical record.

**B — ConPTY + Git Bash.** ~60 lines: `native_pty_system()` → spawn
`C:\Program Files\Git\bin\bash.exe -l` → echo output, forward stdin, one resize.
Also exercise **close/teardown** (see Phase 1 note on `ClosePseudoConsole`).

**C — Daemon survival. THE go/no-go. Test the real spawn path, not a proxy.** v2's probe used
`Start-Process -WindowStyle Hidden`, which can stay console-associated and so
tests console lifecycle rather than job lifecycle. Instead build a stub that
spawns itself with:
1. `DETACHED_PROCESS`
2. `DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB`

and for each, close **a tab** and **the whole window**, from **both** Windows
Terminal and mintty. If neither survives, the core requirement is unavailable
under the no-WMI policy — decide before Phase 1, not after.

**D — mintty viability (§0.1).** In a Git Bash window:
```sh
powershell.exe -NoProfile -Command "[Console]::WindowWidth"   # a number = real console
MSYS=enable_pcon powershell.exe -NoProfile -Command "[Console]::WindowWidth"
```
Then check crossterm raw mode, a resize event, and SGR mouse through that path.

**E — target Windows build.** Record it. ConPTY behaviour differs materially
before/after Win11 24H2 and on older Win10.

---

## 2. What we harvest from herdr

### 2.1 The v2 harvest metric was invalid **[CORRECTED]**

v2 claimed ~7,350 lines liftable "nearly verbatim", measured by counting
`crate::` references. **That test is wrong.** It misses `super::` imports,
`use crate::` *type* dependencies, ambient module-level imports, external setup
contracts, and test padding. Verified counterexamples:

| v2 claim | Reality |
| --- | --- |
| `render_ansi.rs` — "profiling only" | `render_ansi.rs:34` production `use crate::protocol::{underline_style_from_modifier, CellData, CursorState, FrameData, PaneSurfacePatchRow}` |
| `input/encode.rs` — "self refs only" | `encode.rs:3-6` needs `super::model::KITTY_FLAG_REPORT_ALL_KEYS`, `KeyboardProtocol`, `MouseProtocolEncoding`, `TerminalKey` → pulls much of `input/model.rs` |
| `layout.rs` — "only `ratatui::Rect`" | `layout.rs:5` `use ratatui::{layout::{Direction, Rect}, widgets::Borders}` |
| `ipc.rs` — deps listed | `ipc.rs:144` adds an unlisted direct `widestring` dependency |
| "210-line job-object extraction" | Zero `crate::` refs, but depends on module-level Windows imports hundreds of lines away |
| Line counts | Heavily test-padded. `render_ansi.rs` production ends ~line 1030 of 2,355. |

**Revised, honest inventory:**

| File | Status |
| --- | --- |
| `pty/actor.rs` (Windows block), `pty/backend.rs` | **Genuinely liftable.** Low coupling confirmed. This is the real prize. |
| `layout.rs` | **Adapt.** Good BSP structure, but see §2.2 bugs. Keeps ratatui. |
| `ipc.rs` | **Adapt.** Add `widestring`; correct the DACL description. |
| `input/parse.rs`, `input/encode.rs` | **Adapt** — bring `input/model.rs` too. |
| job-object detection, Git Bash fingerprint | **Copy as logic**, with the nested-job fix (§0.2). |
| `render_ansi.rs` | **Dropped** — ratatui does it (§0.4). |

Treat everything except the PTY actor as **"adaptable reference"**, not
verbatim harvest. The value is still real — it is debugged Windows behaviour —
but budget integration time rather than copy time.

### 2.2 Bugs to fix in `layout.rs` when adapting

- `PaneId::from_raw()` (`layout.rs:19-31`) does **not** advance `NEXT_PANE_ID`.
  Restore panes 1,2,3 in a fresh process and the next `alloc()` returns 1 again
  → **ID collision**. Persistence is Phase 3; the allocator must be fixed in
  Phase 2 when the API is designed.
- `Node` / `TileLayout` do not derive serde despite serialization comments.
- `from_saved()` (`layout.rs:336`) does not validate that focus exists.
- Ratios are bounded; **cell dimensions are not** (`layout.rs:691`) — small
  terminals can yield zero-width/height panes.
- It does not compute border insets; that is left to rendering (`layout.rs:483`).

### 2.3 `portable-pty`: sufficient, but be honest about why

Stock `portable-pty` is a reasonable ConPTY abstraction and we will use it. But
v2's framing was misleading:

- It *does* call `CreatePseudoConsole` directly
  (`vendor/portable-pty/src/win/psuedocon.rs:306`). "herdr doesn't touch
  ConPTY" was wrong; herdr doesn't touch it *itself*.
- herdr does **not** use crates.io `portable-pty`. `Cargo.toml:51` patches it to
  a vendored fork that loads an app-local, hash-verified ConPTY. Stock 0.9.0
  does a **relative `conpty.dll` load** — undesirable DLL-search behaviour for a
  corporate app.
- herdr bundles a modern ConPTY because old Win10 system ConPTY drops keyboard
  protocol sequences (`windows-beta.mdx:104`). **We will not bundle binaries**
  (§6.2), so we accept the system ConPTY floor.

Using stock `portable-pty` is an explicit compatibility/security **choice**,
not "doing what herdr does". Probe E records which floor you are on.

### 2.4 Git Bash launch

Fingerprint (`windows.rs:1947`) — require `bash.exe`, absolute, real file,
parent dir `bin`; from the root (stepping past `usr`) require
`<root>/usr/bin/msys-2.0.dll` **and** `<root>/cmd/git.exe`. Verified at
`C:\Program Files\Git`. **Open: discovering non-default / per-user installs.**

Launch with `-l` and nothing else — `windows_login_shell_args` (`pane.rs:1856`)
returns exactly `["-l"]`. Bash is already interactive when stdin is a pty, and
`-l -i` changes which startup files run.

### 2.5 ConPTY realities you inherit

1. **Cursor flicker** — intermediate positions during repaint. Unfixable; draw
   our own cursor.
2. **Delivery lag / burst buffering** — `app/api/agents.rs:18` notes a timing
   assumption that "fails when ConPTY delivery lags it."
3. **Shutdown can deadlock.** `portable-pty` creates ConPTY with
   `PSEUDOCONSOLE_INHERIT_CURSOR`. Closing such a pseudoconsole without
   answering its cursor query can hang, notably before Win11 24H2, and close
   may emit a final frame — so keep draining output during teardown.
   **Terminal query replies are a Phase 1 teardown requirement, not a Phase 4
   nicety.**

   > **CONFIRMED EMPIRICALLY, 2026-09-19 — and my earlier claim here was
   > wrong.** v3 said build 26200 (25H2) "will very likely never reproduce"
   > this. **It reproduced on the very first run of Probe B.**
   >
   > Spawning `bash -l` in a ConPTY, the pseudoconsole emits **`ESC[6n`
   > (DSR-CPR)** during startup — the `PSEUDOCONSOLE_INHERIT_CURSOR`
   > handshake. With nobody answering:
   >
   > ```
   > [probe-b] ConPTY created 80x24
   > [probe-b] spawned bash -l
   > ^[[6n                          <-- unanswered
   > [probe-b] --- resize to 100x30 ---
   > (hangs forever; killed at 45s)
   > ```
   >
   > The shell never finishes initialising, never processes `exit`, and
   > teardown is never reached. Answering with `ESC[1;1R` fixes it outright:
   >
   > ```
   > [probe-b] answered DSR-CPR #1
   > [probe-b] child exited: Ok(ExitStatus { code: 0 })
   > [probe-b] ClosePseudoConsole returned in 40.7µs
   > [probe-b] reader thread joined in 1.53ms (596 bytes, 1 CPR replies)
   > ```
   >
   > **The startup CPR handshake is universal, not version-gated.** What may
   > vary pre/post-24H2 is `ClosePseudoConsole` behaviour once you *have*
   > answered. So this is not a "works on my machine" trap that hides until
   > the work laptop — it fails immediately, everywhere, and must be handled
   > in the first 50 lines of Phase 1.
   >
   > Two details the probe surfaced:
   > - A query can straddle a read boundary. Scan a rolling buffer, not each
   >   chunk in isolation.
   > - ConPTY also emits `ESC[?9001h` (win32-input-mode) and `ESC[?1004h`
   >   (focus reporting) at startup. Decide what to do with both.
   > - Writing to the pane before the shell has finished initialising loses
   >   bytes — the probe's `echo` arrived as `cho`. Gate first input on
   >   readiness.

### 2.6 Licensing

Apache-2.0, no `NOTICE`. Retain the license, attribute, state changes. Adapted
files go in `src/vendor/herdr/` with provenance headers.

---

## 3. Architecture

```
mintty (probe D) or Windows Terminal
 └── zmux.exe              client — crossterm; ratatui Buffer + diff
      │   \\.\pipe\zmux-<user>-<session>
      ▼
   zmuxd.exe               daemon — detached, job-escaped (§0.2)
      ├── Pane × N              EVERY window's panes, in one flat map
      │    ├── ConPTY (portable-pty) → bash.exe --rcfile (§2.4)
      │    ├── vt100::Parser (grid + scrollback + Callbacks)
      │    └── reader thread → mpsc
      ├── Windows               Vec<Layout>, one current (§9)
      └── session.json → %APPDATA%\zmux\
```

One crate, two bins. **Windows only — no `cfg(unix)` anywhere.**

**Concurrency: thread-based, not async.** herdr's `ipc.rs` is synchronous
(`create_sync()`), and `portable-pty`'s Windows reader blocks, forcing a thread
per pane. `tokio` is for channels/timers only. Do not force pipe or PTY I/O
into async.

**IPC contract — underspecified in v2. Required from Phase 1:**

- Protocol **version negotiation** (`wire.rs:19`) — "append enum variants only"
  is *not* a compatibility strategy; an old bincode decoder still cannot decode
  a new variant.
- **Max frame size checked before allocation** (`wire.rs:22`).
- Length-prefixing is **mandatory**: herdr consumes `interprocess::local_socket`
  as a byte stream with arbitrary-size `read()` and `PeekNamedPipe`
  (`ipc.rs:178`). Do not assume message boundaries.
- Partial-read / partial-frame handling.
- **Bounded writer queue + frame coalescing**, so a slow or suspended client
  cannot block pane processing.
- **Generation counter + forced full-frame recovery** after a dropped delta.

**DACL correction:** herdr's SDDL is `D:P(A;;GA;;;SY)(A;;GA;;;OW)` —
SYSTEM + OBJECT OWNER (`ipc.rs:149`), **not** a literal current-user SID as v2
said. Probably fine; document what it actually does.

---

## 4. Phases — resequenced

Codex's sequencing critique is accepted: **detach/reattach is an architectural
constraint, not polish.** Full-frame resync on attach shapes the protocol, so
it must land before splits multiply the surface.

### Phase 1 — one pane, correct (4–5 days)
- [ ] Skeleton, two bins, `src/vendor/herdr/`.
- [ ] Lift `pty/actor.rs` + `pty/backend.rs` (the genuine harvest).
- [ ] Versioned, bounded, length-prefixed IPC (§3) — not a stub.
- [ ] ConPTY create → **and clean close**, incl. cursor-inheritance reply,
      CPR/DSR and DA responses, teardown while the child is writing, child
      reaping, thread termination. Test before and after Win11 24H2 if possible.
- [ ] vt100 cells → ratatui Buffer → `Buffer::diff` → crossterm (§0.4).
      Disable host autowrap. Correct cursor placement.
- [ ] Auto-spawn daemon; poll pipe for readiness.
- **Done when:** a full-screen Git Bash you can work in, that exits cleanly.

### Phase 2 — detach & reattach (3 days)
- [ ] `Detach`; daemon survives; terminal fully restored.
- [ ] `zmux attach` / `ls` / `kill-session`.
- [ ] Per-client **full-frame resync**; reattach at a different size.
- [ ] Apply the Phase 0 probe-C answer.
- **Done when:** closing the window and reopening gets your pane back.

### Phase 3 — splits (4 days)
- [ ] Adapt `layout.rs` **with §2.2 fixes**: stable IDs, focus validation,
      minimum pane geometry, serde derives.
- [ ] Prefix state machine (`Ctrl-b`), split/focus/close/resize.
- [ ] Per-pane ConPTY resize; borders + status as ratatui widgets.
- **Done when:** it replaces tmux for split work.

### Phase 4 — daily-driver (4–5 days)
- [ ] Scrollback view (`screen.set_scrollback`) + selection **overlay** — the
      reason we composite in a Buffer (§0.4).
- [ ] `session.json`: layout + cwd + command per pane. Processes don't survive;
      cwd does.
- [ ] **OSC 7 cwd — design it properly**, it is not a one-liner:
      `$PWD` is an MSYS path (`/c/work`) but `portable-pty` needs a Windows cwd;
      URI-escape spaces and non-ASCII; `PROMPT_COMMAND` may be an array, not a
      scalar; `--rcfile` suppresses normal startup unless it sources the user's
      files, and its interaction with `-l` must be tested.
- **Done when:** you stop reaching for anything else.

### Phase 5 — optional
Mouse · TOML config · Kitty keyboard negotiation · OSC 52/OSC 8 policy ·
adapt `render_ansi.rs` only if ratatui's blitting proves inadequate.

### Phase 6 — windows (§9)
`C-b c` / `C-b n` / `C-b p`. Several layouts per session, one composited.

**Total: ~3 weeks.** Up from v2's 2–2.5 — that estimate assumed a verbatim
harvest that does not exist (§2.1) and deferred work that cannot be deferred.

---

## 5. Explicitly NOT building

| Not building | Why |
| --- | --- |
| Custom ConPTY wrapper | `portable-pty` (§2.3) |
| Custom VT engine | `vt100` (§0.3) |
| Custom diff blitter | `Buffer::diff` (§0.4) |
| Custom layout solver | Adapted from herdr |
| Cross-platform abstraction | Windows only |
| Bundled ConPTY runtime | §6.2 — no shipped binaries |
| Installer / updater / telemetry | §6.2 |
| Named windows, window indices with gaps | §9 — windows are positional |
| Agent detection, workspaces, plugins, remote | herdr's product |
| Scrollback capture, session creation from the CLI | §11 |

---

## 6. Corporate constraints

### 6.1 Artifactory — RESOLVED: all 76 available

**Checked on the corporate network 2026-09-19: a Cargo remote exists and every
crate in the closure resolves.** Caching proxy, not an allowlist. This section
is kept as a record and for the day someone changes the policy.



Full transitive closure resolved against crates.io (normal, non-optional,
Windows target):

| Set | Crates |
| --- | --- |
| Direct | 11 |
| **Transitive** | **76** |
| Without ratatui | 53 |

ratatui costs 23 and **stays** — §0.4 makes it the compositor and diff engine,
and it is what lets us drop a 1,030-line blitter. Do not trade it away.

Caching remote → count irrelevant. Curated allowlist → `cargo vendor
--versioned-dirs`, one artifact instead of 76 tickets. herdr vendors its own
deps the same way.

Expect questions about: `winapi` 0.3.9 + its `-gnu` stubs (pulled by
`portable-pty`, predates `windows-sys`), `nix`/`redox_syscall` (pulled
untargeted, compiled out on Windows), `shared_library`.

### 6.2 Other

| Constraint | Handling |
| --- | --- |
| WMI flagged by EDR | Not used — §0.2 |
| Unsigned binary / SmartScreen | Build locally, run from `%USERPROFILE%`. No download, no installer. herdr lists signing as unsupported too. |
| AppLocker / WDAC | Verify in Phase 0 — a user-dir `.exe` that won't launch is a hard stop |
| Bundled ConPTY | Not shipped. Accept system ConPTY (§2.3) |
| Named pipe security | SDDL `D:P(A;;GA;;;SY)(A;;GA;;;OW)` — document it (§3) |
| No network egress | Local named pipe only. No telemetry, no update check |

## 7. Build setup — source in WSL, compiler on Windows

Use `./x` at the repo root; it exists so nobody has to remember the following.

The source lives in WSL so git lives with it. The compiler must be the Windows
MSVC toolchain. Three facts make that work, each found by hitting it:

1. `cargo.exe` **can** read the source over `\\wsl.localhost\...` (pass
   `--manifest-path`; `cmd.exe` cannot `cd` to a UNC path, but cargo does not
   need it to).
2. `cargo.exe` **cannot** put `target/` there — 9p has no lock-file support, so
   the incremental-compilation session lock fails with
   `os error -2147024895`. `CARGO_TARGET_DIR` must be a Windows-local path.
3. WSL does **not** forward environment variables to Windows processes without
   `WSLENV`. Setting `CARGO_TARGET_DIR` alone silently does nothing — the same
   trap that produced a false Probe D result (§0.1).

```bash
./x test              # cargo test
./x release           # optimised build
./x install [dir]     # release build, then both binaries into dir (~/bin)
./x clippy            # anything else is passed through to cargo
```

`./x` also runs under Git Bash, where `uname` says `MINGW`. On the work
laptop, with the repo on a Windows disk, none of the three facts above apply
and it uses plain `cargo` with plain paths. On *this* machine you can also
open the WSL repo from Git Bash, over `//wsl.localhost/...`, and fact 2 comes
back with a second edge to it:

4. Those `//` paths reach `link.exe`, which reads a leading `/` as the start
   of an option: every object file is discarded as *LNK4044: unrecognized
   option* and the link dies with *LNK1561: entry point must be defined*. So
   `x` converts the manifest with `cygpath -w` (giving `\\wsl.localhost\...`,
   which cannot be mistaken for a switch) and, seeing a `//` repo path, sends
   `target/` to the same Windows-local directory the WSL branch uses. The two
   hosts then share one build cache.

The Windows home comes from `%USERPROFILE%` rather than a hardcoded name, so
nothing in the script assumes this machine; `ZMUX_WINHOME` overrides it.

`install` copies **both** binaries, because the client starts the daemon by
looking next to itself, and refuses to run while a `zmux` process is alive —
Windows will not replace a running `.exe`, and the failure otherwise arrives
as a bare *Access is denied* from the linker.

### Original notes

Windows toolchain: `/mnt/c/Users/eusth/.cargo/bin/cargo.exe`,
`1.96.1-x86_64-pc-windows-msvc`, VS 2019.

Keep the repo on the Windows side (`C:\Users\eusth\src\zmux`), edit via
`/mnt/c/...` from WSL. `cargo.exe` cannot see WSL paths, and building on
`/mnt/c` with Linux cargo is slow over 9p. WSL `cargo` is for `cargo check`
on platform-independent modules only. Do not cross-compile to `-msvc`.

---

## 8. Handoff to the work laptop

Development is on a personal Windows 11 PC. The deliverable moves to a managed
corporate laptop later. Design for that now — retrofitting portability is the
expensive way to do it.

### 8.1 Three rules, applied from the first commit

1. **Commit `Cargo.lock`.** Pins all 76 crates to exact versions, so the work
   build resolves identically and cannot drift.
2. **`cargo vendor --versioned-dirs`, committed — now insurance, not the
   critical path.** Artifactory resolves all 76 crates (§6.1), so this is no
   longer load-bearing. Still do it: it makes the build reproducible offline,
   survives a policy change or a yanked crate, and costs one command. herdr
   vendors its own deps the same way.

   **Independently of availability, vendor `portable-pty` from herdr's tree**
   (`herdr/vendor/portable-pty/`). That drops the unmaintained `shared_library`
   (last published 2018) and closes the relative-`conpty.dll` DLL-hijack path
   in stock 0.9.0 — a security decision, not an availability one (§2.3).
3. **CRT linkage: dynamic (Rust's default) — static is a fallback, not the plan.**
   Corporate IT already installs the VC++ Redistributable via their own
   tooling, so `vcruntime140.dll` is present. That removes the reason to
   static-link, and there is a real argument *against* it on a managed
   laptop: a dynamic CRT gets patched by Windows Update, while a
   statically-linked one freezes whatever CRT the binary was built with, so a
   CRT CVE would require us to rebuild and redistribute.

   The `vcruntime140.dll` ABI has been stable since VS2015, so any 2015–2022
   redist satisfies a Rust MSVC build. Version mismatch risk is low.

   Keep static linking documented as a one-line fallback for the case where a
   machine turns out to lack the redist:

   ```toml
   # .cargo/config.toml — only if the redist is genuinely missing
   [target.x86_64-pc-windows-msvc]
   rustflags = ["-C", "target-feature=+crt-static"]
   ```

   Measured on the dev PC: it removes `vcruntime140.dll` and every
   `api-ms-win-crt-*` import, leaving only `kernel32`/`ntdll`/`dbghelp` plus one
   api-set — all shipped with Windows — at a cost of ~95 KB. Caveat: some crates
   assume a dynamic CRT, so re-verify against the full 76-crate set before
   relying on it.

   **The probe binaries are the exception and stay static** (§8.4): they must
   run on an unverified machine with nothing installed, before any of this is
   confirmed.

### 8.2 Ship both delivery modes — you don't know which will be allowed

| Mode | Needs on the work laptop | Blocked by |
| --- | --- | --- |
| **A. Source + `vendor/`** | Rust toolchain installed | No toolchain, no admin |
| **B. Prebuilt `.exe`** | VC++ redist (confirmed present) | AppLocker / WDAC / SmartScreen on unsigned binaries |

Prepare both. Mode B is a single self-contained file; mode A is the fallback
when policy forbids unsigned executables but permits building from source.

### 8.3 What will differ on the work laptop, in likely order

| Difference | Consequence |
| --- | --- |
| **Older Windows build** | ConPTY teardown deadlock (§2.5); degraded sequence fidelity. The dev PC is 25H2 and will hide both. |
| **EDR (endpoint security)** | Watches the detached daemon. §0.2's no-WMI policy is the main mitigation. |
| **AppLocker / WDAC** | May block an unsigned `.exe` from a user directory. Test with a trivial binary **early** — before writing 3 weeks of code. |
| **No admin rights** | Cannot install a missing redist or toolchain. Confirmed non-issue for the redist — corporate IT ships it. |
| **Group-policy job objects** | Probe C's answer on this PC may not hold there. **Re-run Probe C there** — it is zero-dependency precisely so it can run anywhere, first, with nothing installed. |
| **Different Git for Windows version** | Changes the Probe D (mintty/ConPTY) answer. Re-run it. |

### 8.4 Minimum viable validation on the work laptop

When you get access, before trusting anything: re-run **Probe C** (zero deps,
needs no toolchain — just copy the `.exe`), then **Probe D**, then record the
Windows build. Those three answers determine whether the design still holds.


---

## 9. Windows

Several independent screenfuls of panes in one session: `C-b c` creates one,
`C-b n` / `C-b p` cycle, `C-b <digit>` jumps. tmux's model, reduced to the
smallest thing that works.

### A window IS a layout

`src/windows.rs` holds `Vec<Layout>` plus a current index. There is no window
struct, no `WindowId` and no second id allocator to keep in step with
`PaneId` — a window carries no state beyond its layout, so inventing a type
for it would have bought nothing. Windows are addressed by position, which is
also what the status bar shows.

The cost is that closing a window renumbers the ones after it, where tmux
leaves a gap — so `C-b <digit>` addresses a position, not an identity, and a
window closing elsewhere shifts what a digit means. Accepted: indices stay
contiguous, and the status bar always shows the current numbering, so the
digit to press is on screen rather than remembered.

An out-of-range digit is IGNORED rather than clamped. `C-b 7` with three
windows means "go to seven"; landing on the third instead would move you off
the window you were working in, which is worse than doing nothing.

### Panes never move

Panes stay in the session's one `HashMap<PaneId, PaneState>` whichever window
displays them. Switching windows changes only which layout gets composited —
nothing is suspended, restarted or re-parented, and a background window's
shell keeps running with its output parsed into its own `vt100` grid. That is
the whole feature; everything else is bookkeeping.

Two consequences worth stating:

- **`MAX_PANES` counts across every window.** A hidden pane costs a ConPTY and
  a shell exactly like a visible one.
- **Every window reflows, not just the visible one.** A hidden pane left at an
  old size keeps producing output wrapped for that size, and switching to it
  later cannot un-wrap text that was already laid out wrongly.

### A window switch cannot be a delta

`reflow_locked()` clears `last_published`, so the next publish is a full
frame. A window switch replaces every cell on screen; a row diff against the
screen the client was showing would be meaningless. This falls out of reusing
reflow rather than being special-cased, which is why `switch_window` calls it
even though no pane geometry changed.

### Closing

A window's last pane taking the window with it is load-bearing, not a nicety:
without it a window created by mistake could only be dismissed by exiting its
shell. Both `C-b x` and a shell exiting route through one `remove_pane`, which
returns `Closed::{Pane, Window, LastPane, NotFound}` — a bool cannot say
whether a closed window means a closed session.

`remove_pane` does nothing once `running` is false. `shutdown()` saves the
arrangement the user actually had and then kills every pane, each of which
duly reports its exit; letting those edit and re-save the layout replaced that
snapshot with whatever panes happened to still be standing. Observed as a
restarted session coming back with one window instead of two.

### Persistence

`FORMAT_VERSION` 2 stores a window list and the current index. v1 files are
**migrated**, not discarded — a v1 session is exactly a v2 session with one
window, so this is a mechanical translation rather than the guessing §4
otherwise forbids.

`Windows::repaired()` exists because `serde`'s derive bypasses
`Layout::from_saved`, which is the only thing that advances the `PaneId`
allocator past restored ids (§2.2) and repairs focus naming a missing pane.
Deserialising straight into the runtime type would have reintroduced the
id-collision bug this design exists to avoid.

It also refuses a file that puts one pane id in two windows. That is not
repairable: restore spawns a shell per leaf and keys the pane map by id, so
the second spawn would overwrite the first and leave a live shell with
nothing owning it. Unreachable through the API, reachable by editing the
file.

### Wire

`PROTOCOL_VERSION` 3 appends `Cmd::{NewWindow, NextWindow, PrevWindow}`, and
4 appends `Cmd::SelectWindow(u8)`. The handshake rejects a mismatch, so a v4
client meeting a v3 daemon is refused cleanly rather than failing to decode
mid-stream.

Appending an enum VARIANT is cheap; adding a struct FIELD is not. `PaneInfo`
and `Frame` are bincode structs, and bincode does not tag fields, so a new
field changes the encoding for every existing reader rather than only for the
ones that use it. 6 (`PaneInfo::command`, `busy`, `last_exit`) and 7
(`Frame::mouse`, `FrameDelta::mouse`) each cost a version for that reason.

Two things cross the wire only because the two ends know different halves of
the answer. `Frame::app_cursor` and `Frame::mouse` go daemon → client: only
the daemon's vt100 state knows what the focused pane's program has asked for,
and only the client can act on it, by encoding a key or by handing over the
host terminal's mouse. `ClientMsg::Mouse` goes the other way for the mirror
reason: only the client knows where the pointer is, and only the daemon knows
which pane that lands in and what the program there wants.

### The status bar and the dividers are ours to colour

`render.rs` owns every cell we draw ourselves. The session says what a run of
status text MEANS — `Role::{Name, Sep, Info, Window, ActiveWindow, Alert}` —
and `render.rs` alone decides what a role looks like, so the palette lives in
one place and the width ladder stays a pure function of the text.

Two choices worth recording:

- **Fixed 256-colour indices, not 0–15.** A colour scheme remaps the first
  sixteen, so chrome built from them looks different in every Git Bash theme
  and can come out invisible — "white" painted onto a light background. The
  greys from 236 up and the accents (24 deep blue for the session name, 214
  amber for the window you are on) are stable whatever the scheme.
- **The dividers recede.** Drawn in the default foreground they were the
  brightest thing on screen, competing with the shell output they exist to
  separate. Grey 240 puts them behind it.

The bar names the focused pane by its POSITION in the current window, not by
`PaneId`. Ids come from an allocator that never reuses a number, so after a
few splits and closes the bar said "2 panes | pane 7" - a number with nothing
on screen to match it. It is left out entirely when the window holds one
pane, where it could only ever say "pane 1" next to "1 pane".

The active window is marked twice, by colour and by the `*` the bar already
carried. Cheap redundancy: the colour is what you actually see, and the
marker keeps the bar legible if the colour does not survive.


---

## 10. What the third review changed

An adversarial review (Codex `gpt-5.6-sol`) of the windows work found eight
correctness issues. Six were real and are fixed; the reasoning for the other
two is worth keeping.

**Fixed:**

- `publish()` composed its snapshot OUTSIDE `last_published`, so two
  publishers could commit in the opposite order and the loser reset the base
  to an older revision. Every later delta was then refused against a client
  that had moved past it. The client's `revision < base_revision` guard
  rejected the bad delta but nothing repaired the base, so the display froze
  until an unrelated full frame arrived. The lock is now held across
  compose-derive-send-update.
- `is_running()` then `listen()` is a TOCTOU race; Windows allows several
  instances of one pipe name, so two daemons could serve one session. A named
  mutex (`ipc::claim_session`) makes the claim atomic. It retries for five
  seconds, because a daemon releases its claim marginally after its listener
  closes and a legitimate restart lands in that window.
- `shutdown()` drains the pane map under `geometry`, but `split`,
  `new_window` and `switch_window` did not re-check `running` once they held
  it - so a command arriving after the drain could create a ConPTY nothing
  owned and nothing tore down.
- Write-then-rename stops a torn state file, not a stale one: two savers
  could capture in one order and rename in the other. A `save` mutex now
  covers capture, write and rename together.
- `adjust_ratio` adjusted the first matching ANCESTOR, so in `1 | (2 | 3)`
  the divider between 2 and 3 could not be moved by any pane. It descends
  first and takes the deepest split on the axis. It is deliberately NOT
  restricted to the side the focused pane sits on: that reads better in
  isolation but breaks the symmetry this module documents and tests, leaving
  an edge pane unable to point outward.
- `write_focused` returned an error when the focused pane had already exited,
  and the daemon ends a connection on any input error - so typing as a shell
  exited threw the user out of a live session. A vanished pane is a stale
  keystroke, not a protocol violation.

**Not taken:**

- Deleting `Layout::is_empty` / `Windows::is_empty`. They are unused and
  always return `false`, but clippy's `len_without_is_empty` requires them
  alongside `len()`. Removing them trades a documented invariant for an
  `#[allow]`.
- Rewriting the full-frame window-switch test to identify which message the
  switch produced. The concern - that it could pass or fail on unrelated
  traffic - is answered by draining both shells to silence first, and the
  test was verified non-vacuous by breaking `reflow_locked`'s invalidation
  and watching it fail. What the user actually cares about, that the display
  keeps updating after a switch, is asserted in
  `a_background_window_keeps_its_output`, which uses only the harness's
  reassembling reader.

---

## 11. The control connection

A session that only a human can drive is half a session. herdr's answer is a
JSON CLI per primitive; zmux takes the same shape at a fraction of the size,
because the daemon already holds everything worth exposing.

**A separate handshake, not a quiet attach.** `ClientMsg::Control` replaces
`Hello`. The peer gets no frames and is never registered as a client, and that
is the whole reason it exists: the composited size is the SMALLEST attached
viewport, so a control caller registering as a client would shrink the user's
screen to a size it invented - and would hold the idle timer open for as long
as it stayed connected. `a_control_connection_is_not_an_attached_client` pins
this.

**Every mutation answers with the state it produced.** Not an ack:
`ServerMsg::Info`. A caller that splits a pane learns the new pane's position
from the same round trip, so there is no follow-up query to race against the
user doing something else. It is also the only thing that makes the CLI
scriptable in one line.

It is a snapshot, though, not a transaction: `info()` is read straight after
the command and before `publish()` composites a frame, but no lock spans the
two, so an attached human moving focus in that window is reported as if the
command had done it. Closing that would mean holding a session-wide lock
across a command and a full read-back, which is a worse trade than the
window it removes. `PaneInfo::id` is the identifier that survives a focus
change, and the skill says so.

**Position, not `PaneId`.** Every control command takes the 1-based position
in the current window - what the status bar shows, and what `C-b <digit>`
means. Ids come from an allocator that never reuses a number (§9), so a
user-facing surface built on them would be unreadable within an hour's work.
`PaneInfo` carries both, and says which is which.

**A wrong pane is refused, never redirected.** `InputTo { pane: 7 }` with two
panes is an error. Clamping would type a command into a shell the caller did
not mean, which is how a background helper destroys somebody's afternoon.

So is a command that could not be carried out. A split with no room, or
`close` on the last pane, answers `Refused` rather than an `Info` that happens
to look the same as before - someone attached can SEE that nothing happened,
and a script cannot. Only the creating and destroying commands report this: a
focus command that does nothing at the edge of the screen is not a failure,
it is the edge.

**Writing does not move focus.** `InputTo` deliberately skips the
`follow_focused` that `write_focused` does: a caller typing into a background
pane has no business yanking the user's scrollback view or their cursor. This
is the difference between "an agent can use zmux" and "an agent and a human
can use zmux at the same time".

**Capture is visible rows only.** Reading scrollback means moving the pane's
scroll position, which is user-visible state; there is no way to borrow it
invisibly while the same pane is on somebody's screen. A limitation stated in
the skill beats a capture that makes the user's terminal jump.

Wire: v5, appending `Control`, `Query`, `InputTo` to `ClientMsg` and
`Info`/`Capture` to `ServerMsg`. A pre-v5 daemon cannot decode `Control` at
all and closes without a word, so the client turns that silence into the
instruction that fixes it - kill the session and start it again.

The surface an agent gets is `skills/zmux/SKILL.md`, `include_str!`d into
the client and printed by `zmux --skill`, plus `.claude-plugin/` so Claude
Code can install it as a plugin. `ZMUX=1` and `ZMUX_SESSION` are set on
the daemon before the first pane spawns, so every pane shell inherits them and
"am I inside, and inside what?" has an answer that does not involve walking
process ancestry.

## 12. Agent coordination

§11 gave a script a way to drive a session. This gives one *agent* a way to
drive another: make a pane, start something in it, give it work, know when it
has stopped, and read the answer — without touching the screen of the person
whose session it is.

### The decision the rest follows from: reported, never inferred

herdr learns an agent's state by matching the bottom of the pane against
versioned regex manifests — 22 of them in `src/detect/manifests/`. They are
real engineering: priorities, regions, arbitration against hook-reported
state. They are also pinned to other people's user interfaces. `pi.toml`
matches the literal border

```
── ⠋ Working ─
```

The day pi changes that glyph, every herdr user gets a wrong answer. Silently,
because there is no test in either repo that can notice. The manifests carry
`version` and `updated_at` fields precisely because they rot.

zmux does not ship patterns. A pane's state is what the pane **said** it
was, and a pane that has said nothing is `unknown` — which does not mean
finished, and nothing here treats it as if it did.

This is cheaper *and* better here, for a reason specific to the target
machine: the agent that will run in these panes is built in-house. When you
own the reporter, a reporting contract beats reverse-engineering its output.

### The contract

```
ESC ] 1341 ; zmux ; state=working ; agent=ampere ; name=reviewer ; seq=12 ESC \
```

Written by the program to its own stdout. No process spawned, no `zmux` on
`PATH`, no session name to look up, and a terminal that does not understand it
swallows it — so the same binary still works outside zmux.

- **The literal `zmux` is a guard.** 1341 is unregistered and arbitrary.
  The guard makes a collision with somebody else's 1341 a no-op rather than a
  mis-parse, which means the number is cheap to change if it ever conflicts.
- **`agent` is free text.** An enum of known agents is a list to maintain, and
  zmux has no reason to hold an opinion about which agents exist. `ampere`,
  `pi`, anything.
- **A report says only what changed.** A turn boundary sets `state`; a startup
  report sets `agent` and `name` once.
- **Out of order is dropped.** Two hooks firing close together can arrive
  backwards, and taking the later-arriving one leaves a pane reading `working`
  after it went idle — the one lie a caller waiting on that pane cannot
  survive. `seq` is monotonic per pane; at or below the last accepted value,
  the report is discarded whole.
- **Nonsense drops the key, not the report.** A misspelt state or a name with
  a capital in it is ignored; the rest is taken. A typo in somebody's hook must
  not erase a state that was true.

`ClientMsg::Report` carries the same thing over the pipe, for a reporter whose
stdout is captured and cannot reach the tty. `zmux report --check` sends
both ways and reads the result back, so on a machine where you cannot attach a
debugger, "the escape is not reaching the terminal" is a sentence rather than
an afternoon.

### Staleness is exact, not a timeout

A reported state becomes a lie the moment the process that reported it exits.
The usual fix is a TTL, which is a guess with a number attached.

OSC 133 already brackets the foreground command, and every report an agent
makes necessarily arrives between the two. So **`133;D` clears everything the
pane claimed**. When `D` lands, the agent is gone, and so is its state, its
name and its agent label. No timer, no guess.

`133;C` used to clear as well, on the reasoning that a new command starting
means the last one's claims are about a dead process. Two real agents broke
that in the first minute: pi and ampere stamp `133;A` / `133;B` / `133;C`
around **every assistant and user message**, as zone markers so a terminal can
jump between them (`modes/interactive/components/assistant-message.ts`). Read
as "a command started", that wiped the agent's own report of itself once per
message — exactly when a caller is reading it.

`D` alone was always sufficient, which is the part worth remembering. Every
`C` from a shell follows the `D` of the command before it, so the claim is
already gone; the only `C` with no `D` in front of it is the first command in
the pane, when there is nothing to forget. The lesson generalises: **OSC 133
is not only a shell's vocabulary.** A TUI may use the same markers to mean
something about its own layout, so only the marker that carries an exit status
can be read as "a process ended".

A consequence worth stating: a **name lives exactly as long as the process
that claimed it**. That is herdr's rule too, and for the same reason — a name
that outlives its owner points at whatever moved in afterwards.

### Four things zmux measured, for agents that report nothing

Refusing to guess is not the same as having nothing to say. These are
observations, handed over verbatim, for a caller to draw its own conclusions
from:

| | |
| --- | --- |
| `command` | OSC 633;E — is an agent running here at all |
| `quiet_ms` | how long since the pane produced a byte |
| `bells` | how many times it asked for attention |
| `title` | what it says it is doing |

Three of these were already arriving and being dropped. `Signals::bell` had
been set by the vt100 callback since the beginning and read by nothing.

`bells` is a **count**, not a flag, because a flag has to be cleared to be
useful twice and the status bar and a control caller would race to clear it,
each swallowing the other's notification. `quiet_ms` is stamped by the reader
thread into an atomic on the pane itself — not a map the pump would lock
hundreds of times a second — and measured against a monotonic process clock,
so an NTP correction cannot make a pane look like it spoke in the future.

`title` is now reported whenever there is one. The status bar still suppresses
it when shell integration is live, where it is only the directory restated;
that is a *presentation* rule, and `Activity::raw_title` is what crosses the
wire.

### Waiting

A wait belongs in the daemon, not in every caller's loop. Three things it gets
right that a hand-rolled poll does not:

**The pane is pinned.** The target resolves once, to an id, and the wait
follows that id. Re-resolving each tick would let a replacement answer: wait
on position 2, have that pane exit, and whatever slides into position 2
satisfies the wait. A pinned pane that vanishes is `gone`. herdr pins for
exactly this reason.

**`--settle` closes a race that is otherwise invisible.** Run a command and
immediately wait for quiet, and the shell has not read the line yet — so the
wait is satisfied by the quiet *before* the command, instantly and wrongly.
The gate ignores the condition until the pane is seen doing something, or the
settle time elapses. herdr solves the same problem by requiring observed
activity after a prompt.
`without_a_settle_gate_a_wait_can_answer_about_the_wrong_moment` pins both
halves: that the race reproduces, and that the gate closes it.

Every signal the gate watches is compared **against the reading taken when
the wait began**, never tested as a level. That distinction is the whole gate
for an agent pane, and getting it wrong cost the feature its main use case:
an agent is one long foreground command, so `133;C` set `busy` when it
started and it stays set until it exits. A level test therefore opened the
gate on the *first poll* of every agent wait — measured at 96µs — and handed
back the state from before the prompt. `bells` had always been compared
against `start`, which is why it was the one signal that behaved; `busy` and
`working` now match it. `a_settle_gate_holds_for_a_pane_that_was_already_busy`
pins it, and the shell-case test above pins that the original purpose still
works.

**The daemon caps the timeout, not the caller.** A wait holds a connection
thread; a script that asks to wait forever and then dies leaks one until the
daemon exits. At the ceiling the caller is told `timeout` — which it has to
handle anyway — and can ask again. `timeout` is a *result*, not an error: it
rides in the JSON like everything else, and only the exit status says `met`.

It polls at 50ms against **one** pane, via `pane_status` rather than `info`,
which walks every pane in every window. A subscriber registry would need a new
lock, a new ordering to get wrong, and a way to unregister a caller that died
mid-wait. herdr is event-driven because it serves many long-lived clients;
this serves one-shot CLI calls.

`Until::Settled` counts `blocked`. A caller told only about a clean finish
waits out its entire timeout on the one case that actually needed a person.

`Until::Output` takes a literal substring. A regex engine would be the tenth
direct dependency (§6.1) on a binary this project has reasons to keep small,
and `capture | grep` already covers what a substring cannot express.

It searches the last `RECENT_LINES_DEFAULT` lines, not the whole scrollback,
and that bound is documented rather than raised. Reading further is linear -
measured at 80x24: 200 lines cost 68us per poll, 1000 cost 314us, 5000 cost
1.6ms - so a larger window moves the threshold at a cost in Term-lock time
twenty times a second, and any fast enough burst still beats it. Closing it
properly would mean matching incrementally as output arrives, which is the
per-pane subscriber registry this section already declined. A caller waits
for something printed at the END of a command, which is what "it finished"
looks like anyway.

### Driving another agent: the six things a caller cannot do itself

§12 gave a caller the pieces. Reading herdr's `agent.*` surface against them
showed six places where the pieces do not compose, and every one of them is a
thing the DAEMON has to do because a caller physically cannot.

**Write and wait as one request.** Between an `InputAt` and a `Wait` a caller
has no way to say "compare against the reading from before my write" - it can
only read the pane afterwards, and a pane that reacted and settled again in
between is indistinguishable from one that never heard. `Submit` holds both
halves. The guarantee is structural and **is not pinned by a test**: inside
the daemon the before- and after-readings are microseconds apart and no pane
can react between them, so moving the reading later leaves the suite green.
The window it closes is two client round trips wide and nothing can make a
reaction land inside it on purpose. What IS tested is the pair of things a
caller genuinely cannot do for itself, below.

**Refuse a pane that reported `blocked`.** `blocked` means the agent has
stopped to ask something. A prompt sent now answers ITS question, and there
is no undo. Refused before a byte is written, as herdr does.

**Say `stalled` rather than `timeout`.** A prompt that provoked nothing is a
different failure from one still being worked on, with a different fix.
Neither means nothing was written - the text went out either way, so a blind
retry sends it twice.

**Say `replaced` when the agent has gone but the pane has not.** Counted from
claims that DIED, not from `133;D`, and the difference is the whole point:
the rcfile emits `D` from PROMPT_COMMAND on **every prompt** - deliberately,
because being at a prompt is being idle - so an idle shell would look like it
was losing a process at every keystroke. This is the same trap as reading
`133;C` as "a command started", which used to wipe an agent's own report once
per message. **OSC 133 says where the shell IS, not only what just happened
to it.** Checked only AFTER the condition, so a wait for `quiet` or for
`unknown` - both of which a command ending satisfies - still answers `Met`.

**`Until::Reported`.** "Did it hear me", which no state can express: an agent
that says `working` twice changes nothing and has plainly reacted. It is also
how `start` knows an agent is up without holding an opinion about which state
it comes up in - `idle` for one, `working` for another that greets you. An
enum of those would be a list to maintain, which is the thing §12 exists to
avoid.

**`start` refuses a pane that is busy.** The command line would be typed into
whatever holds the foreground. herdr asks the OS for the foreground process;
this asks the pane, which already said so through shell integration - the
same answer without the process lookup that §0.2's neighbour rules out.

### Enter, and when it is its own write

A pasted prompt's Enter now goes in a **separate write, 300ms later**. Plain
text keeps it in the same write.

The two cases differ. Into a shell, one write is atomic and right: the line is
read whole and nothing can interleave. Into a program that asked for bracketed
paste, the paste is an **event** - a text box collects it, places the cursor,
usually redraws - and a `\r` arriving in the same read is consumed as part of
that event, so the prompt lands in the box and is never sent.

This reverses the earlier reasoning here, which was that a redraw between the
two writes could LOSE the Enter. A redraw is what the beat is for. herdr
reached the same split against twenty real agents - its agent prompt waits
300ms, its `pane run` concatenates - and that is evidence of a kind this
project cannot generate for itself, so it wins over the argument.

**Not demonstrated by a test, and the test says so.** `bash` is not a text
box: it reads the line whichever way the bytes arrive, so the one-write
version passes too. Proving the benefit needs a real TUI agent.

The Enter is a literal `\r` rather than a mode-dependent encoding, because
vt100 0.16.2 does not track the kitty keyboard protocol and there is nothing
better to know here. herdr can encode through the pane's own key mode because
its engine reports it.

### Reading a full-screen program's own history

The thing §12 could not do, and the reason the rest of it fell short of
being usable: an agent draws on the **alternate screen**, which has no
scrollback. Everything it has scrolled past is gone as far as the terminal
is concerned. Measured on a real agent: 100 lines printed, **19 reachable**.
`capture --source recent` cannot see an answer that is more than a screen
long, which is most answers.

The application still HAS those rows - it redraws them when scrolled. So the
only way to read them is to be a user: send the wheel, photograph each
screen, put the viewport back. `--source history` does that. herdr's version
is `src/server/alt_screen_read.rs`, 996 lines, which is the honest measure of
the care it takes.

**This writes into somebody's live pane**, and everything in `harvest.rs` is
shaped by that. It refuses rather than guesses; it bails to the ordinary read
on anything it does not recognise; it spends its last budget putting the
viewport back. The worst outcome it permits is a plain screen read. The
outcome it must never produce is a pane left scrolled somewhere its owner did
not put it - and when it cannot restore, it says so on the daemon's stderr,
because nothing else could explain what happened to that screen.

It declines, falling back to the ordinary read, for: a pane not on the
alternate screen (it has real scrollback and needs none of this), one that
never asked for the mouse (nothing sent can scroll it, and the bytes would
arrive as TEXT), one **somebody is already scrolled back in** - probed for by
scrolling DOWN first and checking nothing moved - and one already being
harvested.

**The step size is measured, not assumed.** How far one wheel event scrolls
is the application's decision: three lines is common, one is usual, five
exists. The first step sends ONE event, `merge_up` measures what it was
worth, and the rest are sized to stay just inside a screen so consecutive
screens always overlap. Without the overlap there is nothing to align on and
no way to know what was skipped. An application that moves more than a whole
screen on a single event cannot be read this way at all, and says so rather
than inventing the gap.

**Two bugs found building it, both the same bug.**

`settle` waited for the pane to be QUIET after a wheel event. A pane that has
been idle is still idle a millisecond after you poke it, so it returned
immediately and photographed the screen from before its own event - every
step read `Unchanged` and the harvest concluded it was already at the top.
Quiet is not evidence of having reacted. It now waits for the redraw to
**start** (a change in `Pane::last_output_ms`) and then to finish. This is
the same mistake as the wait gate reading `busy` as a level, in the same
session, in different code.

And `-n` kept the wrong end. Collection runs backwards from the bottom and
overshoots, so trimming the front threw away the newest output - the part
somebody reading an answer came for.

### Reading history without the screen jumping

`capture` could only ever return the visible rows, and the skill said so as a
limitation. The reason was real: reading scrollback means moving vt100's
scrollback offset, and that offset is the pane's own user-visible state.
Borrow it while somebody is reading their output and their terminal jumps.

One lock closes that. Every reader of a pane's screen takes the same
`Mutex<Term>`, so `read_recent` moves the offset, collects, and puts it back
with nobody able to observe the middle. The restore goes through the same path
scrolling uses, so `Term::scroll` and vt100 cannot end up disagreeing about
where the user is.

Collection walks a screenful at a time, because vt100 shows `rows` at once,
and each window contributes only the rows the step revealed —
`set_scrollback` clamps, so consecutive windows otherwise overlap by however
far the clamp moved. `RecentUnwrapped` joins rows that only ended because they
ran out of width: a pane is as wide as its share of the window, so those breaks
are the layout talking rather than the text. It is the source to read a
transcript with.

### Addressing

`ZMUX_PANE` is set per pane, to the **id**. Positions renumber when a pane
closes, so a shell that read its own position at startup would be telling a
caller to type into somebody else's pane an hour later.

There is deliberately no `ZMUX_WINDOW`: closing a window shifts every index
after it. A pane's window is a question for `info`, which answers it fresh.

`info` now spans every window. It used to report the visible one only, which
made a background window's panes unreachable *and* undiscoverable — finding
out what was in a window meant switching to it, moving the user's screen.
`position` stays within its window, so it still means what the status bar
shows, and `focus_id` is the handle that needs no pairing with `window`.

A name **two panes claim addresses neither**. Nothing can prevent the
duplicate at the point it is made: `record` is per-pane, and a report arrives
on the pty reader path already holding that pane's `Term` lock, so it cannot
go looking at its neighbours without inverting the lock order. So the check
lives in `resolve`, where the locks are taken in the right order anyway — and
that is the better place regardless, because an ambiguous name is
**temporary**: when one of the two claimants exits, `133;D` clears its claim
and the name starts working again. Refusing beats picking the lower id, which
is how a prompt ends up in the wrong agent's pane; the refusal says how many
panes answer to the name, because "no pane named 'reviewer'" sends somebody
looking for a pane that is right in front of them.

`-p` takes a position (`3`), an id (`@3`), a name (`reviewer`) or `current`.
Bare digits are positions and a name may not begin with one, so the four
cannot collide; `current` is a reserved name.

### Trust

A pane's reports are exactly as trustworthy as whatever runs in that pane.
`cat` a hostile file and it can claim any state and any name — just as it can
already lie about the working directory through OSC 7, which zmux has
believed since Phase 4.

That is why state is advisory and **nothing destructive keys off it**. Nothing
in zmux closes a pane, kills a shell or writes a file because of a reported
state. The worst a forged report can do is make a waiting caller stop waiting,
or make a name resolve to the wrong pane — and a caller that cares holds an
`@id`, which no report can influence.

There is one place where a caller's *text* is not merely advisory, and it is
worth naming because the defence had the bug. A prompt submitted with
`--paste` is wrapped in `ESC[200~ … ESC[201~`, so text containing an end
marker would close the paste early and the remainder would be typed as live
keystrokes — the tail of somebody's prompt becoming commands in their shell.
`paste::strip_end_marker` removes the marker for that reason.

It removed it by **scanning the input**, which a payload could exploit to
rebuild one out of the repair: `ESC[2` + `ESC[201~` + `01~` drops the middle
marker and joins the surviving halves into a live one. The attack was handed
its weapon by the defence. Checking the **output** after each byte fixes it
and stays linear, because removing a suffix cannot expose a marker the same
rule did not already remove. `no_payload_at_all_can_put_an_end_marker_in_the_body`
pins the invariant rather than an example of it, assembling payloads out of
marker fragments — uniform random bytes would never build one.

Text with no marker in it is still handled by a plain scan, because
reassembly needs a removal and there is nothing to remove. That is worth
keeping rather than tidying away: at 64KB, which a prompt can be, the
byte-wise walk costs 238µs against the scan's 22µs, and the marker-free
case is almost always the case.

The pipe's ACL is unchanged. `InputAt` reaches any pane in the session, but
anyone who can open the pipe could already type into the focused one, so the
trust boundary is where it was.

### Wire

v8. `PaneInfo` gained `window`, `state`, `agent`, `name`, `quiet_ms`, `bells`
and `title`; `SessionInfo` gained `focus_id` and `created`. Struct fields, so
they cost a version exactly as `PaneInfo::command` did in v6 — bincode does
not tag fields, and a new one changes the encoding for every existing reader.
`Target`, `InputAt`, `Report`, `Wait`, `Query::CaptureAt` and the background
splits are appended variants, which are free.

One trap worth recording, because it fails silently: **do not put
`skip_serializing_if` on a field that crosses this wire.** bincode 2 goes
through the `Serialize` impl, so a skipped field is skipped *on the wire*, and
the decoder then reads the next field's bytes as this one. `SessionInfo::created`
is a plain `Option` and shows as `null` in JSON for that reason.

### Does the lock discipline survive a flooding pane?

The §12 workload is one agent polling `info`, or sitting in a `wait`, while
the pane beside it floods. Both sides want the same `Term` locks: the pump
takes them to compose, `info` and `capture` take them to read. If the order
in §2 were wrong, or a lock were held across a pty write, this is where it
would show - as a control caller starved behind the frame loop.

Measured on the target machine against a flat-out producer, microseconds:

| | quiet p50/p90/max | flooding p50/p90/max |
| --- | --- | --- |
| `info` | 26 / 34 / 114 | 33 / 44 / 97 |
| `capture` | 51 / 63 / 81 | 92 / 102 / 589 |

A flood costs a control caller about a third more on `info` and under double
on `capture`. The worst single call in the run was 589µs. **No convoy, no
starvation**: an agent can poll at 20Hz next to a pane producing as fast as
Windows will carry it and never wait a millisecond.

Session start to first control reply is **56ms**, and teardown **51ms** —
both dominated by spawning and tearing down the first ConPTY, and both far
below anything a person notices.

`a_control_caller_is_not_starved_by_a_flooding_pane` keeps this true. It
asserts on p50 at 300x the measured value, because it is not trying to pin
33µs - that would fail on a busy machine for no reason. It is trying to
catch the SHAPE: a lock held across a compose or a write puts a control
caller behind the 16ms frame interval. Verified by holding the pane map
across the composite, which takes `info` to 20.9ms at p50 and fails it.

### A known gap: the backwards-delta race has no regression test

`publish()` holds `last_published` across compose-derive-send because two
publishers composing concurrently could commit in the opposite order: the
loser derives a delta from a base the winner has already moved past, the
client refuses it, nothing repairs the base, and the screen freezes on the
last frame that applied. It was found live, by switching windows while a
pane was producing output.

**Nothing in the suite would catch it coming back.** That is worth writing
down rather than leaving for someone to discover by assuming otherwise.
Moving the compose back outside the lock leaves every test green, including
`switching_windows_under_load_does_not_freeze_the_display`, which is named
for what it does check rather than for this.

Three shapes were tried. A **flood masks it**: the client's slot stays
occupied, so `coalescing` forces a full frame, and a full frame repairs the
very chain the race breaks — the bug needs a client that is keeping up and
genuinely taking deltas. **Paced output** is the right shape, but `sleep` on
MSYS is a process at ~7ms a call, so a loop long enough to race across runs
for minutes, and a short one never hit the window. The window is the few tens
of microseconds between `snapshot()` returning and the lock being taken, and
no arrangement of shell commands can aim at it.

Closing it properly would mean a seam in `publish()` that a test can block
on — injecting a pause between compose and commit — which is production code
existing for a test. Worth doing if this area is ever touched again; not
worth doing speculatively. Until then the guard is the comment, and the
comment is why it is three paragraphs long.

### What a frame costs, and what was not done about it

Measured on the target machine, release build, dense screens, four panes —
`examples/perf.rs` reproduces all of it:

| | 80x24 | 160x44 | 240x60 |
| --- | --- | --- | --- |
| `frame_from_screen`, one pane | 30µs | 111µs | **228µs** |
| `composite`, frames already built | 9.6µs | 9.6µs | 14µs |
| `delta_from`, nothing moved | 1.6µs | 5.8µs | 12µs |
| `delta_from`, full redraw | 1.4µs | 3.3µs | 6.2µs |

Two things in that table are worth reading twice.

**The composite is one function.** Building the per-pane frames is 94% of it;
the blit and the layout arithmetic together are 14µs. Any work spent on
compositing that is not spent on `frame_from_screen` is spent on the wrong
thing.

**A delta costs more when nothing moved than when everything did.** An
unchanged row must be compared to the end before it can be called unchanged,
while a changed one differs at the first cell and short-circuits — so the
copy is cheaper than the proof. It is not a bug and it does not need fixing;
it is worth knowing because it means the steady state is the expensive one,
and it inverts the intuition that would otherwise guide an optimization here.

`Cell` now derives `Copy`. It is fourteen bytes of plain data and always was,
and the derive lets `to_vec` reach its memcpy specialisation and removes a
per-cell `clone` from the blit: 8% off the settled delta, 18% off a full
redraw, one word of source.

**Per-pane frame caching was considered and rejected.** `snapshot()` rebuilds
every pane's frame on every publish, so a session with one busy pane and
three idle ones pays for all four. Keying a cache on the pty reader's
`last_output` stamp would skip the idle ones — but the key would also have to
carry the scroll offset and the size, because `set_scrollback` and a resize
both change what `screen()` shows without a byte arriving. Miss one of those
and a pane freezes on screen, which is the failure this file spends three
paragraphs of `publish()` guarding against elsewhere. The whole path is
230µs at 60fps — **1.4% of one core** — and `publish()` already returns
immediately when no client is attached, which is the entire headless-agent
case §12 exists for. A cache whose failure mode is a frozen pane is not worth
1.4% of a core.

Going below that would mean going around vt100: `contents()` validates UTF-8
per cell and there is no slice accessor. The one optimisation its public API
does allow — guarding on `has_contents()` — was measured earlier at 5% slower
on dense screens and 6% faster on sparse ones, and rejected for depending on
the content rather than the code.

### What this deliberately does not do

- **No screen-pattern manifests**, per the argument above.
- **No process detection.** herdr's `CreateToolhelp32Snapshot` route is MITRE
  T1057 Process Discovery (§0.2's neighbour), and it was not adopted for the
  same reason WMI was not.
- **No event subscription stream.** `Wait` covers the agent case.
- **No regex matching**, **no remote**, **no automatic editing of anyone's
  agent configuration**, and **no pane creation inside a background window**.
