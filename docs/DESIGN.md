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
