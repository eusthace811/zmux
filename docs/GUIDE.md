# zmux — user guide

tmux-style panes for Windows, running Git Bash. One background daemon per
session owns the shells; the client you type in owns nothing, so closing it
loses nothing.

Everything below assumes a **Git Bash window** (MinTTY). No winpty, no
PowerShell, no WSL — see [Troubleshooting](#troubleshooting).

---

## Install for global use

Everything runs from the repo folder — the one holding `Cargo.toml` and the
`x` script. The script works out which host it is on; you only have to be in
the right directory.

**On this machine** the repo lives under your WSL home, so open a **WSL**
shell:

```bash
# /home/<you>/projects/Experiments/zmux/zmux
cd ~/projects/Experiments/zmux/zmux

# build --release, then copy both binaries to ~/bin
./x install
```

then, in a **Git Bash** window:

```bash
hash -r
zmux --help
```

Git Bash can reach that same repo folder over the WSL share, which is handy
for reading the source or running `git` from the Windows side:

```bash
cd //wsl.localhost/Ubuntu/home/<you>/projects/Experiments/zmux/zmux
```

Reach it, yes — but do not build it from there; see the warning below.

Run from WSL, `./x` compiles with the *Windows* cargo and installs into the
**Windows** home (`C:\Users\<you>\bin`), which is the `~/bin` Git Bash sees —
the shells run on Windows, so that is the only home that matters.

**On the work laptop** there is no WSL: the repo sits on the Windows disk and
Git Bash does the whole job, from the folder you cloned into —

```bash
cd ~/projects/zmux       # i.e. C:\Users\<you>\projects\zmux
./x install
hash -r
```

Do **not** run `./x install` from Git Bash against the WSL copy of the repo
(the `//wsl.localhost/...` path above): cargo would put `target/` on the 9p
share, which has no file locks, and the build fails part-way. From Git Bash,
build a repo that lives on the Windows disk; from WSL, `./x` already takes
care of it — it compiles with the Windows cargo and keeps `target/` on `C:`
for exactly this reason.

`~/bin` is on Git Bash's `PATH` already (it exports `$HOME/bin:$PATH`
unconditionally, in `/etc/profile.d/env.sh`), so `hash -r` is only there to
clear bash's memory of a command it could not find before. To install
somewhere else — a shared tools folder, say — name it:

```bash
./x install /mnt/c/Tools/zmux     # from WSL
./x install /c/Tools/zmux         # the same folder, named from Git Bash
```

Name it the way the shell you are in names it, or you will get a directory
somewhere you did not mean. Then add that folder to `PATH` yourself; the
script prints the line to add. To
reach `zmux` from `cmd` or PowerShell as well, add the folder to the Windows
`PATH` (System Properties → Environment Variables → `Path`).

Two rules the script follows, and you should too if you ever copy the
binaries by hand:

- **Both, always.** `zmux.exe` starts `zmuxd.exe` by looking *next to
  itself*, so a half-installed pair fails at the first attach.
- **Do not rename them.** The client looks for that exact file name.

**Upgrading:** Windows will not let a running `.exe` be replaced. `./x install`
checks first and tells you which sessions to `zmux kill`, rather than
letting the build fail with *Access is denied*.

---

## Everyday use

Once installed these need no particular directory — run them from any Git
Bash window, wherever you happen to be:

```bash
zmux                    # attach to "default", creating it if needed
zmux work               # a session named "work"
zmux ls                 # which sessions are alive
zmux attach work        # attach, but refuse to create
zmux kill work          # end the session and its daemon
zmux config             # write a commented config file, print its path
zmux --help
```

Inside a session, `C-b d` detaches: the shells keep running, and
`zmux work` puts you back where you were. You can attach from a second Git
Bash window at the same time — both see the same screen, sized to the
**smaller** of the two windows.

A session ends when you `zmux kill` it, when every shell in it has exited,
or after **8 idle hours** with nobody attached.

---

## Reading the status bar

```
 work │ 0 1* 2 │ 2 panes │ pane 1
   ▲      ▲         ▲         ▲
   │      │         │         └─ which pane has focus, counting from 1
   │      │         └─ panes in THIS window
   │      └─ windows; the highlighted one is where you are, and its
   │         number is the digit that jumps back to it
   └─ session name
```

The window list appears only once you have more than one window, and the
focused pane only once the window holds more than one — neither can tell you
anything otherwise. `SCROLL` appears in amber while you are looking at
history rather than the live screen.

On a narrow terminal the line drops its least useful parts first, ending at
` 1/3 | 2p ` — which window of how many, and how many panes.

---

## Shortcuts

`C-b` means: hold Ctrl, press `b`, release both, **then** press the next key.
It is a prefix, not a chord.

### Panes

| Key | |
| --- | --- |
| `C-b %` | split left / right |
| `C-b "` | split top / bottom |
| `C-b x` | close the focused pane |
| `C-b ←` `→` `↑` `↓` | move focus |
| `C-b Ctrl-←` `→` `↑` `↓` | resize the focused pane (2 columns / 1 row a step) |
| *click* | focus the pane you clicked |

A new pane starts in the focused pane's current directory. Closing the last
pane of a window closes the window; `C-b x` on the last pane of the last
window is refused, since there would be nothing left to show.

Typing `exit` in a shell closes its pane the same way — except in that final
pane, where it does what you asked and ends the session.

### Windows

| Key | |
| --- | --- |
| `C-b c` | new window (one pane, inheriting the current directory) |
| `C-b n` / `C-b p` | next / previous window, wrapping |
| `C-b 0` … `C-b 9` | jump to that window by the number on the status bar |

A window is a whole screenful of panes. Background windows keep running:
their shells produce output, and it is there when you switch back.

Windows are numbered by position, so closing one renumbers those after it —
which is why the bar always shows the current numbering. A digit with no
window behind it does nothing, rather than landing you somewhere unintended.

### Scrollback

| Key | |
| --- | --- |
| `C-b [` | enter scroll mode |
| `C-b PageUp` | scroll up a page without entering scroll mode |
| *wheel* | scroll the focused pane, any time |

In scroll mode: `↑`/`k` and `↓`/`j` move a line, `PageUp`, `Space` and `Home`
move ten up, `PageDown` moves ten down, and `q`, `Esc`, `Enter` or `End`
return to the live screen. (`Home` scrolls a page up; it does not jump to the
oldest line.) Each pane keeps **10,000 lines** by default.

### Other

| Key | |
| --- | --- |
| `C-b d` | detach; the session keeps running |
| `C-b C-b` | send a literal `Ctrl-b` to the shell |

`C-b C-b` is how you reach Ctrl-b inside programs that want it themselves —
readline's "move back one character", for instance.

**Selecting text:** zmux captures the mouse, so hold **Shift** to use
MinTTY's own selection and copy.

---

## Configuration

```bash
zmux config      # writes %APPDATA%\zmux\config.toml if it is not there,
                   # and prints the path either way
```

Every setting is optional and every default is the behaviour you already
have. A malformed file is reported and ignored rather than being fatal.

| Setting | Default | |
| --- | --- | --- |
| `prefix` | `"b"` | the prefix key, combined with Ctrl |
| `shell` | `""` | full path to the shell; empty means find Git Bash |
| `shell_args` | `[]` | extra arguments for the shell |
| `scrollback` | `10000` | lines kept per pane (capped at 1,000,000) |
| `shell_integration` | `true` | OSC 7 directory tracking, so new panes inherit the current directory |
| `idle_timeout_hours` | `8` | how long an unattended daemon lingers |
| `status_bar` | `true` | turning it off gives panes one more row |

`prefix` is read by the client, so a new `zmux` picks it up. Everything
else is read by the daemon when the session is created — `zmux kill` the
session and start it again.

---

## Where things live

| | |
| --- | --- |
| `%APPDATA%\zmux\config.toml` | your settings |
| `%APPDATA%\zmux\session-<name>.json` | saved layout and per-pane directories |
| `\\.\pipe\zmux-<sid>-<session>` | the live session; it exists exactly while the daemon does |

Layouts survive a reboot; processes do not. Reattaching after the daemon is
gone rebuilds the panes and starts a fresh shell in each one's last
directory.

---

## Troubleshooting

**"protocol mismatch"** — a daemon from an older build is still serving that
session. `zmux kill <session>` and attach again; the message names the
session for you.

**Blank panes** — you started it from WSL or another non-console host. zmux
needs a real Windows console, which the Git Bash window provides.

**"Access is denied" when copying a new build** — a client or daemon is still
running and holding the `.exe`. `zmux ls`, kill what it lists, then copy.

**"no such session: x"** — `zmux attach` never creates. Use `zmux x`.

**Two names, one session** — session names are sanitised for the pipe
namespace, so `a/b` and `a_b` collide. The client compares the name it asked
for against the one the daemon reports and refuses the mismatch rather than
silently joining the wrong session.

**Arrow keys or function keys misbehaving in an app** — worth reporting; keys
are encoded xterm-style, including application cursor mode, and anything that
still goes wrong is a bug rather than a limitation.
