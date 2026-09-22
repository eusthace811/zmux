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

**On this machine** the repo lives inside WSL, not on `C:`. That makes `~`
ambiguous — it is a different folder in each shell, and so is the repo:

| In | `~` is | the repo folder is |
| --- | --- | --- |
| WSL | `/home/<you>` | `/home/<you>/projects/Experiments/zmux/zmux` |
| Git Bash | `/c/Users/<you>` | `//wsl.localhost/Ubuntu/home/<you>/projects/Experiments/zmux/zmux` |

`cd ~/projects/Experiments/zmux/zmux` therefore works in WSL and **not**
in Git Bash, where it looks under `C:\Users\<you>` and finds something else,
or nothing. Every path below is spelled out in full for that reason.

Build and install from a **WSL** shell:

```bash
cd /home/<you>/projects/Experiments/zmux/zmux
./x install                 # --release, then both binaries into place
```

`./x` compiles with the *Windows* cargo and installs into the **Windows**
home, `C:\Users\<you>\bin` — the shells run on Windows, so that is the only
home that matters. Pick it up in a **Git Bash** window:

```bash
hash -r
zmux --help
```

Git Bash can build the very same repo over the WSL share, if you would
rather stay in one window — `./x` notices and does the right thing:

```bash
cd //wsl.localhost/Ubuntu/home/<you>/projects/Experiments/zmux/zmux
./x install
hash -r
```

Either shell produces the same binaries, and both keep `target/` in
`C:\Users\<you>\.zmux-target`, so they share one build cache: whichever you
run second only rebuilds what changed.

**On the work laptop** there is no WSL: the repo sits on the Windows disk and
Git Bash does the whole job, from the folder you cloned into —

```bash
cd /c/Users/<you>/projects/zmux      # wherever you cloned it
./x install
hash -r
```

What `./x` is protecting you from, either way, is `target/` landing on the
WSL share. Plain `cargo build` there fails twice over: the 9p share has no
file locks, and the `//wsl.localhost/...` object paths reach `link.exe`,
which reads a leading `/` as the start of an option — *LNK4044: unrecognized
option*, then *LNK1561: entry point must be defined*. If you see that, you
ran `cargo` directly; run `./x` instead.

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
zmux --skill            # what a coding agent should know about zmux
zmux --help
```

A session can also be driven from outside, without attaching — for scripts,
and for coding agents:

```bash
zmux info work                        # layout, windows, focus, as JSON
zmux send work split-v                # any keybinding, by name
zmux run  work -p 2 "cargo test"      # type it into pane 2 and press Enter
zmux keys work -p 2 "text"            # type it, no Enter
zmux capture work -p 2 -n 20          # that pane's last 20 lines
zmux wait work -p 2 --until quiet     # block until it finishes
```

`-p` names the pane — `2` its position in the current window, `@2` its id,
`current` the pane you are in — and targeting one this way does **not** move
your focus, so someone else can keep working while a script types in another
pane. `send` with no command prints the list. Every command that changes
something answers with the new `info`, so a script learns where its new pane
landed without asking twice.

**Two agents, one session** below has the rest: making a pane without
stealing the focus, reading past the visible rows, and knowing when the thing
in a pane has stopped.

Everything else — `zmux`, `zmux <name>`, `zmux attach` — opens the
full-screen client and does not return until you detach.

Inside a session, `C-b d` detaches: the shells keep running, and
`zmux work` puts you back where you were. You can attach from a second Git
Bash window at the same time — both see the same screen, sized to the
**smaller** of the two windows.

A session ends when you `zmux kill` it, when every shell in it has exited,
or after **8 idle hours** with nobody attached.

---

## Inside a pane

Each pane shell inherits two variables from its daemon:

```bash
echo "$ZMUX"          # 1
echo "$ZMUX_SESSION"  # the session that owns this shell
```

Useful in a prompt, in a script that should behave differently under zmux,
or for a coding agent working out where it is. `ZMUX` is unset in an
ordinary Git Bash window.

### Pasting

Paste as you normally would (right-click in mintty, `Ctrl+V` or right-click in
Windows Terminal). The pane receives it the way it would outside zmux:

- A program that asked for **bracketed paste** gets the text as one block.
  An agent's prompt box shows `[Pasted N lines]`, and none of the newlines
  in the text submits anything. Most shells, editors and agents ask.
- A program that did not ask gets the text typed, exactly as a terminal would
  type it. In a plain shell, each line runs as you paste it.

zmux decides this per pane, from what the program in *that* pane asked
for. The focused pane gets the paste, and it snaps back to live output if you
were scrolled up.

A paste over 1 MiB is dropped rather than split: split into pieces, it would
reach an agent as several separate pastes.

---

## Reading the status bar

```
 work │ 0:bash  1:cargo*  2:vim │ 1• 2*
   ▲       ▲                       ▲
   │       │                       └─ panes in THIS window: 1 is running
   │       │                          something, 2 is the one you are typing in
   │       └─ windows, each named after what is running in it; the
   │          highlighted one is where you are, and its number is the
   │          digit that jumps back to it
   └─ session name
```

A window is named after the command running in its focused pane — `cargo`,
`vim`, `ssh` — and after the shell itself when nothing is. `•` marks a pane
that is running something, so you can see a build still going in a pane you
are not looking at. Both need zmux's shell integration, which is on unless
you set `shell_integration = false`.

Whatever is highlighted — the session name, the window you are on — keeps a
column of its own colour inside its edges and a column of the bar's outside
them, so the text is centred in its block and the block is not touching the
window next to it. That is why the spacing around the highlighted window
looks wider than the rest: one of those two columns is coloured.

The window list appears only once you have more than one window, and a pane's
number only once the window holds more than one — neither can tell you
anything otherwise, though a single pane still gets its `•`. `SCROLL` appears
in amber while you are looking at history rather than the live screen.

On a narrow terminal the line drops its least useful parts first: the window
names go before the window list does, and the last rung is ` 1/3 | 2p • ` —
which window of how many, how many panes, and whether anything is running.

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
| *click* | focus the pane you clicked, when zmux has the mouse |

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
| *wheel* | scroll the focused pane, when zmux has the mouse |

In scroll mode: `↑`/`k` and `↓`/`j` move a line, `PageUp`, `Space` and `Home`
move ten up, `PageDown` moves ten down, and `q`, `Esc`, `Enter` or `End`
return to the live screen. (`Home` scrolls a page up; it does not jump to the
oldest line.) Each pane keeps **10,000 lines** by default.

### Other

| Key | |
| --- | --- |
| `C-b ?` | the shortcut list, on screen; any key closes it |
| `C-b d` | detach; the session keeps running |
| `C-b m` | cycle who owns the mouse: auto → zmux → the terminal |
| `C-b C-b` | send a literal `Ctrl-b` to the shell |

`C-b C-b` is how you reach Ctrl-b inside programs that want it themselves —
readline's "move back one character", for instance.

**Selecting text.** Only one of zmux and your terminal can have the mouse.
While zmux has it, the terminal never sees your drag, and dragging out a
selection does nothing at all.

By default zmux holds it only while the program in the focused pane has
asked for one. At a shell prompt nobody has, so the mouse is yours and
selection works; start `vim` or `lazygit` and it moves across without your
doing anything; quit and it comes back. That is the `mouse = "auto"` setting.

`C-b m` cycles through the three settings for the session:

| | |
| --- | --- |
| `"auto"` | whoever is using it — the default |
| `"on"` | zmux always: click focuses a pane, the wheel scrolls history |
| `"off"` | the terminal always; nothing inside zmux sees a mouse, programs included |

Holding **Shift** while dragging also works whatever the setting: Windows
Terminal and MinTTY both read Shift as "ignore the application and let me
select".

Under `"auto"` the wheel does not scroll history at a prompt, because zmux
is not holding the mouse there — `C-b [` and `C-b PageUp` are. Scroll mode
itself takes the mouse for as long as you are in it, so the wheel works once
you are there.

Two things to expect even once selection works. zmux runs on the alternate
screen, so the terminal's own scrollbar is empty — you can only select what
is on screen. And with a left/right split every line crosses both panes, so a
selection takes text from both; tmux has the same problem for the same reason.

---

## Two agents, one session

A pane can be driven from outside without anyone attaching, which is how a
coding agent gets a second pane to work in — and how it finds out what
happened there. None of this moves the cursor of whoever is using the session.

Inside a pane, the shell already knows where it is:

```bash
echo "$ZMUX"          # 1
echo "$ZMUX_SESSION"  # the session that owns this pane
echo "$ZMUX_PANE"     # this pane's id
```

`ZMUX_PANE` is an **id**, not a position. Positions renumber when a pane
closes; ids never do, and never get reused. Anything that outlives a single
command should hold the id.

### Make a pane and put something in it

```bash
S=$ZMUX_SESSION
NEW=$(zmux send "$S" split-v-bg | python -c 'import json,sys;print(json.load(sys.stdin)["created"])')
zmux run "$S" -p "@$NEW" "ampere"
```

`split-v-bg` splits **without taking the focus**, so the user keeps typing
where they were. Because the focus did not move, the reply's `created` field
is what names the new pane.

`-p` accepts four things:

| | |
| --- | --- |
| `3` | position in the current window, as the status bar counts |
| `@3` | pane id — works in a window that is not on screen |
| `reviewer` | a name the pane claimed for itself |
| `current` | the pane you are running in (`$ZMUX_PANE`) |

A target that matches no pane is **refused**, never redirected to something
nearby.

### Give it a prompt

```bash
zmux run "$S" -p "@$NEW" --paste "Review the diff and report only what is actionable."
```

`--paste` matters for anything with a text box. Without it every newline in a
multi-line prompt submits a fragment, and you get three turns instead of one.
zmux only wraps the text when the program in that pane has actually asked
for bracketed paste, so `--paste` at a plain shell prompt is harmless.

### Find out when it is done

```bash
zmux wait "$S" -p "@$NEW" --until settled --timeout 120000
```

| `--until` | |
| --- | --- |
| `settled` | it reported idle, done **or** blocked — the usual one |
| `idle` `working` `blocked` `done` | one specific reported state |
| `quiet` | no shell command running. Right for `cargo test`; useless for an agent, which *is* one long command |
| `quiet-for MS` | the pane has produced no output for that long |
| `output TEXT` | that text is in the pane's last 200 lines. Wait for something printed at the *end* of a command: a pane can out-produce that window between two polls |
| `bell` | the pane rang the bell |

It prints the session state the wait ended with, plus `outcome`: `met`,
`timeout` or `gone`. Only `met` exits 0, so `zmux wait … && zmux capture …`
does what it looks like. A `timeout` is a result, not a failure — the pane is
simply still going.

`--settle` (default 1.5s) fixes a race you would otherwise hit immediately:
run a command and wait for `quiet` in the next breath, and the shell has not
read the line yet, so the wait is satisfied by the quiet *before* your
command. The gate ignores the condition until the pane is seen doing
something *since the wait began* - a pane that was already busy, as every
agent is for its whole life, does not count as having started. Pass
`--settle 0` when the pane is already working on the thing you are
waiting for.

### Read the answer

```bash
zmux capture "$S" -p "@$NEW" --source unwrapped -n 120
```

| `--source` | |
| --- | --- |
| `screen` | the rows on screen (the default) |
| `recent` | recent output, reaching back into scrollback |
| `unwrapped` | the same, with lines joined where the pane was too narrow to hold them |

Use `unwrapped` for a transcript. A pane is only as wide as its share of the
window, so a long answer comes back broken at that width — and those breaks
are your layout, not the text. Reading history does **not** move the pane's
scroll position, so it is safe to do while somebody is reading their own
output.

### Teaching your agent to say what it is doing

zmux never guesses at a pane's state by reading its screen. A pane's state
is what the pane **said**, and a pane that said nothing is `unknown` — which
does not mean finished.

An agent reports by writing one escape to its own terminal:

```
ESC ] 1341 ; zmux ; state=working ; agent=ampere ; seq=12 ESC ```

No process, no `zmux` on `PATH`, no session name, and a terminal that does
not understand it ignores it — so the same build still works outside zmux.
`examples/report-state.sh` has a shell helper and the three lines to add to an
agent you own. From a script whose output is captured and cannot reach the
terminal:

```bash
zmux report --state working --agent ampere    # defaults to your own pane
zmux report --check                           # does the channel work here?
```

A claim is cleared when the command that made it ends, so nothing goes stale
and a name lives exactly as long as the process that claimed it.

**For an agent that reports nothing**, `zmux info` still gives you four
facts it actually measured — `command` (is something running), `quiet_ms` (how
long it has been silent), `bells` (did it ask for attention) and `title` (what
it says it is doing). `--until quiet-for 3000` is the honest "it has probably
finished" for those.

A pane's reports are only as trustworthy as what runs in that pane, which is
why nothing destructive keys off them.

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
| `mouse` | `"auto"` | who owns the mouse: `"auto"`, `"on"` or `"off"` (see **Selecting text**) |
| `wait_timeout_max_ms` | `600000` | ceiling on one `zmux wait` (see **Two agents, one session**) |

`prefix` and `mouse` are read by the client, so a new `zmux` picks them up. Everything
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

**A pane disappears the moment it is made, or an existing one vanishes** —
the machine may be out of consoles. Every pane is one, and Git Bash refuses to
start once the *whole machine* has 128, counting every terminal, editor and
`git.exe` that is holding one. The shell starts and immediately dies, so zmux
sees an ordinary exit and closes the pane; the reason was on the pane's screen
for the moment it existed.

The daemon now keeps that reason. Start it with its stderr somewhere you can
read — from Git Bash, `zmuxd.exe <session> 2> zmux.log` — and look for

```
[session] pane 3 never started: ... console device allocation failure - too many consoles in use
```

Closing some terminal windows is the cure. There is nothing zmux can do
about the ceiling itself: it is shared with everything else running.

**Arrow keys or function keys misbehaving in an app** — worth reporting. Keys
reach the pane exactly as your terminal encodes them, except the arrows and
Home/End, which zmux rewrites for a program in application cursor mode.
Anything that still goes wrong is a bug rather than a limitation.

**A paste arrives letter by letter, or an agent submits the first line of
it** — the program in that pane did not ask for bracketed paste, so it was
typed at, as any terminal would. If the program *does* ask outside zmux,
this is a bug: check you are running a matching client and daemon (`zmux
kill <session>` after upgrading).

**"this console does not support VT input"** — the client reads the keyboard
in VT input mode, which needs Windows 10 1809 or later. mintty (Git Bash) and
Windows Terminal both provide it; a very old `conhost` does not.
