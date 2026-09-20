//! zmux - the client. Draws frames and forwards keystrokes. Owns no state,
//! so detaching loses nothing. DESIGN.md §3.

use std::io::{stdout, Write};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::{execute, terminal};
use interprocess::TryClone as _;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use zmux::ipc;
use zmux::protocol::{Cmd, ClientMsg, Frame, Query, ServerMsg, PROTOCOL_VERSION};
use zmux::render::{paint, paint_cursor, paint_overlay};

/// Every host mouse-reporting mode we know how to turn off.
///
/// Wider than crossterm's `DisableMouseCapture`, which clears 1006, 1015,
/// 1003, 1002 and 1000 and leaves `?9` (X10 compatibility) and `?1005`/`?1016`
/// (alternative encodings) set. A program that enabled one of those and then
/// died leaves the host terminal reporting mouse events at a zmux that has
/// stopped listening for them, and the only cure is closing the window.
/// herdr clears the same list on entry and exit (`terminal_modes.rs:3`).
const CLEAR_HOST_MOUSE: &[u8] =
    b"\x1b[?1006l\x1b[?1016l\x1b[?1015l\x1b[?1005l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?9l";

/// Hand the mouse to zmux, or give it back to the terminal.
///
/// Only one of them can have it: while these modes are set the terminal
/// forwards clicks and drags to us instead of acting on them itself, which is
/// why selecting text does nothing until they are cleared.
fn set_mouse_capture(on: bool) -> Result<()> {
    let mut out = stdout();
    if on {
        execute!(out, crossterm::event::EnableMouseCapture)?;
    } else {
        out.write_all(CLEAR_HOST_MOUSE)?;
        out.flush()?;
    }
    Ok(())
}

/// Restores terminal state even on panic or early return.
struct TermGuard;

impl TermGuard {
    fn enter(mouse: bool) -> Result<Self> {
        terminal::enable_raw_mode()?;
        // DisableLineWrap matters: herdr notes clients must disable host
        // autowrap or writing the last column wraps and corrupts the next row
        // (render_ansi.rs:986, terminal_setup.rs:114). DESIGN.md §0.4.
        execute!(stdout(), terminal::EnterAlternateScreen, terminal::DisableLineWrap,
                 crossterm::cursor::Hide)?;
        // Clear before setting, even when we are about to capture: whatever
        // the last program in this window left behind is not our state.
        set_mouse_capture(false)?;
        if mouse {
            set_mouse_capture(true)?;
        }
        Ok(Self)
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = set_mouse_capture(false);
        let _ = execute!(stdout(), crossterm::cursor::Show, terminal::EnableLineWrap,
                         terminal::LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

fn spawn_daemon(session: &str) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let exe = std::env::current_exe()?.with_file_name("zmuxd.exe");
    if !exe.is_file() {
        bail!("daemon not found next to the client: {}", exe.display());
    }
    // Say so BEFORE spawning, because after this the answer is invisible: the
    // daemon comes up, sessions work, and the loss only shows when the
    // terminal closes and everything the user thought was persistent is gone.
    // `DETACHED_PROCESS` leaves the console but NOT the job (probe-c's header),
    // so the daemon inherits this limit from us.
    if zmux::harden::job_kills_members_on_close() == Some(true) {
        eprintln!(
            "[zmux] warning: this process is in a job object marked \
             KILL_ON_JOB_CLOSE, and the daemon inherits it.\n\
             [zmux] it will be killed when the last handle to that job closes. \
             For a console host that is when you close the window, so this \
             session may not outlive it.\n\
             [zmux] run probes/probe-c to find out for certain on this machine."
        );
    }
    // DESIGN.md §0.2: DETACHED_PROCESS only. No WMI - that is MITRE T1047 and
    // a high-signal EDR detection on a managed laptop. Probe C confirmed this
    // is sufficient ON THIS PC; the check above is what answers the question
    // anywhere else. CREATE_BREAKAWAY_FROM_JOB is the documented fallback rung.
    std::process::Command::new(exe)
        .arg(session)
        .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
        .spawn()?;
    Ok(())
}

const USAGE: &str = "\
zmux - tmux-style panes for Windows

  zmux [session]          attach to a session, creating it if needed
  zmux attach <session>   attach to an existing session (does not create)
  zmux ls                 list live sessions
  zmux kill <session>     terminate a session and its daemon
  zmux config             write a commented default config, print its path
  zmux --skill            print the agent skill file (see skills/zmux)
  zmux --help

Driving a session without attaching (for scripts and agents):
  zmux info <session>                    layout, windows, focus, as JSON
  zmux send <session> <command> [n]      one command; `send` alone lists them
  zmux keys <session> [-p N] <text>      type text, no Enter
  zmux run  <session> [-p N] <command>   type it and press Enter
  zmux capture <session> [-p N] [-n N]   a pane's visible screen, as text
  -p N chooses a pane by its position in the current window (default: focused)
";

/// The names `zmux send` accepts, printed when it is given none.
const CMD_NAMES: &str = "\
commands for `zmux send <session> <command>`:
  split-h  split-v  close
  focus-left  focus-right  focus-up  focus-down
  resize-left  resize-right  resize-up  resize-down
  new-window  next-window  prev-window  select-window <index>
  scroll-up  scroll-down  page-up  page-down  scroll-bottom";

fn parse_cmd(name: &str, arg: Option<&str>) -> Option<Cmd> {
    Some(match name {
        "split-h" => Cmd::SplitHorizontal,
        "split-v" => Cmd::SplitVertical,
        "close" => Cmd::ClosePane,
        "focus-left" => Cmd::FocusLeft,
        "focus-right" => Cmd::FocusRight,
        "focus-up" => Cmd::FocusUp,
        "focus-down" => Cmd::FocusDown,
        "resize-left" => Cmd::ResizeLeft,
        "resize-right" => Cmd::ResizeRight,
        "resize-up" => Cmd::ResizeUp,
        "resize-down" => Cmd::ResizeDown,
        "new-window" => Cmd::NewWindow,
        "next-window" => Cmd::NextWindow,
        "prev-window" => Cmd::PrevWindow,
        "select-window" => Cmd::SelectWindow(arg?.parse().ok()?),
        "scroll-up" => Cmd::ScrollUp,
        "scroll-down" => Cmd::ScrollDown,
        "page-up" => Cmd::ScrollPageUp,
        "page-down" => Cmd::ScrollPageDown,
        "scroll-bottom" => Cmd::ScrollToBottom,
        _ => return None,
    })
}

fn session_arg(args: &[String], sub: &str) -> Result<String> {
    args.get(1).cloned().with_context(|| format!("usage: zmux {sub} <session> ..."))
}

/// Pull an optional leading `-p N` / `--pane N` off the argument list.
fn take_pane(args: &[String]) -> Result<(Option<u8>, &[String])> {
    match args.first().map(String::as_str) {
        Some("-p" | "--pane") => {
            let n = args.get(1).and_then(|v| v.parse().ok()).context("-p needs a pane number")?;
            Ok((Some(n), &args[2..]))
        }
        _ => Ok((None, args)),
    }
}

/// `capture`'s flags: `-p N` and `-n LINES`, in either order.
///
/// An argument that is neither is an ERROR rather than something to skip.
/// Reading only a LEADING `-p` meant `capture s -n 20 -p 2` quietly dropped
/// the `-p` and answered with the focused pane - the same silent wrong answer
/// that an out-of-range position is refused to avoid.
fn capture_args(args: &[String]) -> Result<(Option<u8>, u16)> {
    const USAGE: &str = "usage: zmux capture <session> [-p N] [-n LINES]";
    let (mut pane, mut lines) = (None, 0u16);
    let mut rest = args;
    while let Some(flag) = rest.first().map(String::as_str) {
        // Recognise the flag BEFORE asking for its value, or an argument that
        // is not a flag at all comes back as "nonsense needs a number".
        if !matches!(flag, "-p" | "--pane" | "-n" | "--lines") {
            bail!("unexpected argument '{flag}'\n{USAGE}");
        }
        let value = rest.get(1).with_context(|| format!("{flag} needs a number\n{USAGE}"))?;
        match flag {
            "-p" | "--pane" => pane = Some(value.parse().context(USAGE)?),
            _ => lines = value.parse().context(USAGE)?,
        }
        rest = &rest[2..];
    }
    Ok((pane, lines))
}

/// Backslash escapes, so control keys are reachable without a key-name
/// vocabulary: `\x03` is Ctrl-C, `\e` Escape, `\r` Enter. An unknown escape
/// is left alone rather than swallowed - a Windows path typed as an argument
/// is full of backslashes and none of them mean anything here.
fn unescape(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut it = s.bytes();
    while let Some(b) = it.next() {
        if b != b'\\' {
            out.push(b);
            continue;
        }
        match it.next() {
            Some(b'r') => out.push(b'\r'),
            Some(b'n') => out.push(b'\n'),
            Some(b't') => out.push(b'\t'),
            Some(b'e') => out.push(0x1b),
            Some(b'\\') => out.push(b'\\'),
            Some(b'x') => {
                // Look ahead by CLONING the iterator. A `Peekable` cannot see
                // the second digit - peeking twice returned the first one
                // twice, so `\x03` sent NUL, `\x1b` sent 0x11, and Ctrl-C
                // never reached a pane. Consume only once both digits match.
                let hex: String =
                    it.clone().take(2).map(char::from).filter(char::is_ascii_hexdigit).collect();
                match u8::from_str_radix(&hex, 16) {
                    Ok(v) if hex.len() == 2 => {
                        it.nth(1);
                        out.push(v);
                    }
                    _ => out.extend_from_slice(b"\\x"),
                }
            }
            Some(other) => {
                out.push(b'\\');
                out.push(other);
            }
            None => out.push(b'\\'),
        }
    }
    out
}

/// Connect, turning "the pipe is not there" into "the session is not there".
///
/// Only that one error means a missing session. A busy pipe or a refused ACL
/// is a different problem, and flattening it to "no such session" would send
/// someone looking in the wrong place.
fn connect_to(session: &str) -> Result<interprocess::local_socket::Stream> {
    ipc::connect(session).map_err(|e| {
        match e.downcast_ref::<std::io::Error>().map(std::io::Error::kind) {
            Some(std::io::ErrorKind::NotFound) => anyhow::anyhow!("no such session: {session}"),
            _ => e.context(format!("connecting to session '{session}'")),
        }
    })
}

/// One control round trip: connect, handshake, ask, read the single reply.
///
/// A fresh connection per command on purpose. These are one-shot CLI calls,
/// and a control connection holds no session state - nothing is gained by
/// keeping it open, and a dropped one costs nothing.
fn request(session: &str, msg: &ClientMsg) -> Result<ServerMsg> {
    // One connection, not two. `is_running` IS a connection - it opens the
    // pipe and drops it - so probing first cost a second daemon handler
    // thread per command and still could not stop the daemon disappearing
    // between the probe and the real connect. The connect error carries the
    // same diagnosis.
    let mut s = connect_to(session)?;
    ipc::write_msg(&mut s, &ClientMsg::Control { version: PROTOCOL_VERSION })?;
    match ipc::read_msg::<_, ServerMsg>(&mut s) {
        // Verify the name we were ACTUALLY given, exactly as `attach` does.
        // Pipe names are sanitised lossily, so `a/b` and `a_b` reach one
        // daemon; a control caller that skipped this check would run its
        // command in somebody else's session, and neither side would know.
        Ok(Some(ServerMsg::Hello { session: served, .. })) if served != session => bail!(
            "asked for session '{session}' but the daemon serves '{served}' - \
             the two names collide once sanitised for a pipe name"
        ),
        Ok(Some(ServerMsg::Hello { .. })) => {}
        Ok(Some(ServerMsg::Refused { reason })) => bail!("daemon refused: {reason}"),
        Ok(Some(other)) => bail!("unexpected handshake reply: {other:?}"),
        // A daemon from before v5 does not know `Control`, so it cannot even
        // decode the request: it closes without a word. Say what to do.
        Ok(None) | Err(_) => bail!(
            "the daemon serving '{session}' did not answer. It is probably an older \
             build: `zmux kill {session}`, then start the session again."
        ),
    }
    ipc::write_msg(&mut s, msg)?;
    match ipc::read_msg::<_, ServerMsg>(&mut s) {
        Ok(Some(ServerMsg::Refused { reason })) => bail!("{reason}"),
        Ok(Some(reply)) => Ok(reply),
        Ok(None) => bail!("the daemon closed the connection without replying"),
        Err(e) => bail!("no reply from the daemon: {e}"),
    }
}

/// Every mutation answers with the session state it produced, so a caller
/// learns where its new pane landed without a second, racy query.
fn print_info(reply: ServerMsg) -> Result<()> {
    let ServerMsg::Info(info) = reply else { bail!("unexpected reply: {reply:?}") };
    println!("{}", serde_json::to_string_pretty(&info)?);
    Ok(())
}

/// Bundled at build time; `zmux --skill` prints it.
const SKILL: &str = include_str!("../../skills/zmux/SKILL.md");

fn main() -> Result<()> {
    // Before anything that could trigger a LoadLibrary (see harden.rs).
    zmux::harden::restrict_dll_search_path();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--help" | "-h") => {
            // The shortcuts are generated from one list, so this
            // and `C-b ?` cannot drift apart again.
            let cfg = zmux::config::Config::load();
            println!("{USAGE}\n{}", zmux::help::usage_block(cfg.prefix));
            return Ok(());
        }
        Some("--skill") => {
            // Bundled at build time so the printed skill always matches this
            // binary, the way herdr does it: a skill that describes a CLI it
            // was not built alongside is worse than none.
            print!("{SKILL}");
            return Ok(());
        }
        Some("ls") => {
            let sessions = ipc::list_sessions();
            if sessions.is_empty() {
                println!("no live sessions");
            } else {
                for s in sessions {
                    println!("{s}");
                }
            }
            return Ok(());
        }
        Some("config") => {
            // Config::write_default keeps an existing file; this only ever
            // creates the template, never overwrites what someone has edited.
            println!("{}", zmux::config::Config::write_default()?.display());
            return Ok(());
        }
        Some("kill") => {
            let Some(name) = args.get(1) else { bail!("usage: zmux kill <session>") };
            // Ask WHICH session this pipe serves before ending it. `Shutdown`
            // is acted on before any handshake, so sent on its own it cannot
            // tell `a/b` from the `a_b` it collides with - and this is the one
            // command where being wrong ends every shell in somebody's
            // session. A daemon too old for the control protocol closes
            // without answering; that is exactly the daemon a version
            // mismatch tells you to kill, so it is ended unverified.
            let mut s = connect_to(name)?;
            ipc::write_msg(&mut s, &ClientMsg::Control { version: PROTOCOL_VERSION })?;
            let verified = match ipc::read_msg::<_, ServerMsg>(&mut s) {
                Ok(Some(ServerMsg::Hello { session: served, .. })) => {
                    if served != *name {
                        bail!(
                            "asked to kill '{name}' but the daemon serves '{served}' - \
                             the two names collide once sanitised for a pipe name"
                        );
                    }
                    true
                }
                _ => false,
            };
            // Reuse the control connection when it lives; it takes Shutdown
            // too. Otherwise the old daemon has already dropped this one.
            let mut s = if verified { s } else { connect_to(name)? };
            ipc::write_msg(&mut s, &ClientMsg::Shutdown)?;
            let _ = ipc::read_msg::<_, ServerMsg>(&mut s);
            println!("killed {name}");
            return Ok(());
        }
        Some("info") => {
            print_info(request(&session_arg(&args, "info")?, &ClientMsg::Query(Query::Info))?)?;
            return Ok(());
        }
        Some("send") => {
            let Some(name) = args.get(2) else {
                println!("{CMD_NAMES}");
                return Ok(());
            };
            let cmd = parse_cmd(name, args.get(3).map(String::as_str))
                .with_context(|| format!("unknown command '{name}'\n\n{CMD_NAMES}"))?;
            print_info(request(&session_arg(&args, "send")?, &ClientMsg::Command(cmd))?)?;
            return Ok(());
        }
        Some("keys" | "run") => {
            let enter = args[0] == "run";
            let session = session_arg(&args, &args[0])?;
            let (pane, rest) = take_pane(&args[2..])?;
            if rest.is_empty() {
                bail!("usage: zmux {} <session> [-p N] <text>", args[0]);
            }
            let mut data = unescape(&rest.join(" "));
            if enter {
                data.push(b'\r');
            }
            let msg = match pane {
                Some(p) => ClientMsg::InputTo { pane: p, data },
                None => ClientMsg::Input { data },
            };
            print_info(request(&session, &msg)?)?;
            return Ok(());
        }
        Some("capture") => {
            let session = session_arg(&args, "capture")?;
            let (pane, lines) = capture_args(&args[2..])?;
            let reply = request(&session, &ClientMsg::Query(Query::Capture { pane, lines }))?;
            let ServerMsg::Capture { text } = reply else { bail!("unexpected reply: {reply:?}") };
            println!("{text}");
            return Ok(());
        }
        Some("attach") => {
            let Some(name) = args.get(1) else { bail!("usage: zmux attach <session>") };
            if !ipc::is_running(name) {
                bail!("no such session: {name}. `zmux {name}` would create it.");
            }
            return attach(name.clone());
        }
        _ => {}
    }
    let session = args.first().cloned().unwrap_or_else(|| "default".into());
    attach(session)
}

fn attach(session: String) -> Result<()> {
    let cfg = zmux::config::Config::load();

    if !ipc::is_running(&session) {
        eprintln!("starting daemon...");
        spawn_daemon(&session)?;
        // Poll for readiness; herdr waits up to 15s at 50ms (autodetect.rs).
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while std::time::Instant::now() < deadline && !ipc::is_running(&session) {
            std::thread::sleep(Duration::from_millis(50));
        }
        if !ipc::is_running(&session) {
            bail!("daemon did not come up within 15s");
        }
    }

    let mut stream = ipc::connect(&session)?;
    let (cols, rows) = terminal::size()?;
    ipc::write_msg(&mut stream, &ClientMsg::Hello { version: PROTOCOL_VERSION, cols, rows })?;

    match ipc::read_msg::<_, ServerMsg>(&mut stream)? {
        // Verify the name we were ACTUALLY given. Pipe names are sanitised
        // lossily and truncated, so two different session names can map to
        // one pipe - `a/b` and `a?b` both become `a_b`. Without this you
        // silently join somebody else's session; with it you get told.
        Some(ServerMsg::Hello { session: served, .. }) if served != session => {
            bail!(
                "asked for session '{session}' but the daemon serves '{served}' - \
                 the two names collide once sanitised for a pipe name"
            );
        }
        Some(ServerMsg::Hello { session, .. }) => eprintln!("attached to {session}"),
        Some(ServerMsg::Refused { reason }) => bail!("daemon refused: {reason}"),
        other => bail!("unexpected handshake reply: {other:?}"),
    }

    let mut mouse = cfg.mouse;
    // Nothing has drawn yet, so nothing can be asking for the mouse. `Auto`
    // starts off and turns itself on with the first frame that wants it.
    let mut capturing = mouse.captures(false);
    let _guard = TermGuard::enter(capturing)?;
    let mut term = Terminal::new(CrosstermBackend::new(stdout()))?;

    // Frames arrive on their own thread; the main loop owns input and drawing.
    // Bounded so a stalled UI cannot grow this without limit. Note this
    // BLOCKS the reader when full rather than dropping - which is what we
    // want, because the daemon then sees us as slow and can act, whereas
    // silently discarding frames would desynchronise the delta chain.
    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<ServerMsg>(128);
    let mut rstream = stream.try_clone()?;
    std::thread::spawn(move || {
        while let Ok(Some(msg)) = ipc::read_msg::<_, ServerMsg>(&mut rstream) {
            if frame_tx.send(msg).is_err() {
                return;
            }
        }
        // Dropping frame_tx here disconnects frame_rx, which is how the main
        // loop learns the daemon is gone. Without this the client sits in raw
        // alternate-screen mode redrawing the last frame forever.
    });

    let mut latest: Option<Frame> = None;
    let mut seen_revision = 0u64;
    let mut help = false;
    loop {
        // Input. Ctrl-b is the prefix, as in tmux.
        if event::poll(Duration::from_millis(8))? {
            match event::read()? {
                // Drop releases FIRST. Windows reports press and release, and
                // prefix handling used to run before the encoder's release
                // filter - so the Ctrl-b RELEASE was read as a doubled prefix
                // and sent a literal Ctrl-b to the pane.
                Event::Key(k) if matches!(k.kind, event::KeyEventKind::Release) => {}
                // Any key closes the shortcut list, and is SWALLOWED closing
                // it - otherwise whatever you pressed to get rid of it turns
                // up in your shell.
                Event::Key(_) if help => help = false,
                Event::Key(k) if is_prefix(&k, &cfg) => match read_prefix_command(&cfg)? {
                    Prefix::Detach => {
                        ipc::write_msg(&mut stream, &ClientMsg::Detach)?;
                        break;
                    }
                    Prefix::Scroll => {
                        if !scroll_mode(&mut stream, capturing)? {
                            drop(_guard);
                            eprintln!("lost connection to the daemon");
                            return Ok(());
                        }
                    }
                    Prefix::MouseToggle => {
                        mouse = mouse.toggled();
                    }
                    Prefix::Help => help = true,
                    Prefix::Command(cmd) => {
                        ipc::write_msg(&mut stream, &ClientMsg::Command(cmd))?;
                    }
                    // A doubled prefix sends a literal Ctrl-b to the pane, so
                    // the shortcut stays reachable inside programs that use it.
                    Prefix::Literal => {
                        ipc::write_msg(&mut stream,
                            &ClientMsg::Input { data: vec![cfg.prefix_byte()] })?;
                    }
                    Prefix::Ignore => {}
                },
                Event::Key(k) => {
                    let app_cursor = latest.as_ref().is_some_and(|f| f.app_cursor);
                    if let Some(bytes) = zmux::keys::encode(k, app_cursor) {
                        ipc::write_msg(&mut stream, &ClientMsg::Input { data: bytes })?;
                    }
                }
                // Every mouse event goes to the daemon as it happened. Only
                // the daemon knows which pane the pointer is over and what
                // the program there asked for, so it - not the client -
                // decides between feeding that program and focusing a pane.
                Event::Mouse(m) => {
                    if let Some(ev) = mouse_event(m) {
                        ipc::write_msg(&mut stream, &ClientMsg::Mouse(ev))?;
                    }
                }
                Event::Resize(c, r) => {
                    ipc::write_msg(&mut stream, &ClientMsg::Resize { cols: c, rows: r })?;
                }
                _ => {}
            }
        }

        // Arrival order is not trustworthy: a queued broadcast can land after
        // a newer snapshot written directly on attach. Revisions decide.
        loop {
            match frame_rx.try_recv() {
                Ok(ServerMsg::Frame(f)) => {
                    // Strictly newer, or the first frame we have seen. An
                    // EQUAL revision means a queued frame from before an
                    // attach snapshot, which would move the screen backwards.
                    if !f.is_well_formed() {
                        eprintln!("discarding a malformed frame from the daemon");
                    } else if latest.is_none() || f.revision > seen_revision {
                        seen_revision = f.revision;
                        latest = Some(f);
                    }
                }
                Ok(ServerMsg::Delta(d)) => {
                    // A delta that does not apply is discarded rather than
                    // rendered half-updated; the daemon sends a full frame to
                    // any client it knows is out of sync.
                    if let Some(f) = latest.as_mut() {
                        if f.apply_delta(&d) {
                            seen_revision = f.revision;
                        }
                    }
                }
                Ok(ServerMsg::PaneExited { code }) => {
                    drop(_guard);
                    println!("session ended (exit code {code})");
                    return Ok(());
                }
                Ok(ServerMsg::Detached) => return Ok(()),
                Ok(ServerMsg::ShuttingDown) => {
                    drop(_guard);
                    println!("session terminated");
                    return Ok(());
                }
                Ok(_) => {}
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    drop(_guard);
                    eprintln!("lost connection to the daemon");
                    return Ok(());
                }
            }
        }

        // Who should have the mouse now. Checked once a tick rather than on
        // every frame: the answer only changes when a program starts or stops
        // asking, and telling the terminal the same thing repeatedly costs a
        // write to stdout each time.
        let want = mouse.captures(latest.as_ref().is_some_and(|f| f.mouse));
        if want != capturing {
            set_mouse_capture(want)?;
            capturing = want;
        }

        if let Some(f) = &latest {
            // ratatui diffs against its own previous buffer and emits only the
            // changed cells - no hand-written blitter (DESIGN.md §0.4).
            let size = term.size()?;
            term.draw(|frame| {
                let buf = frame.buffer_mut();
                paint(buf, f, 0, 0);
                if help {
                    // Over the cursor too: it belongs to a pane you are not
                    // looking at while this is up.
                    let text = (size.width as usize).saturating_sub(8);
                    paint_overlay(buf, &zmux::help::lines(cfg.prefix, text, size.height as usize));
                } else {
                    paint_cursor(buf, f, 0, 0);
                }
            })?;
        }
    }

    stdout().flush()?;
    Ok(())
}

fn is_prefix(k: &KeyEvent, cfg: &zmux::config::Config) -> bool {
    k.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(k.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&cfg.prefix))
}

/// Turn a crossterm mouse event into the wire form.
///
/// `None` for the ones nothing downstream can use - horizontal scrolling has
/// no xterm button number, so there is nothing to forward and nothing zmux
/// does with it either.
fn mouse_event(m: event::MouseEvent) -> Option<zmux::protocol::MouseIn> {
    use zmux::protocol::{MouseKind, MOUSE_ALT, MOUSE_CTRL, MOUSE_SHIFT};
    // xterm's numbering, which is what these end up as on the way to a
    // program: left 0, middle 1, right 2.
    let button = |b| match b {
        event::MouseButton::Left => 0,
        event::MouseButton::Middle => 1,
        event::MouseButton::Right => 2,
    };
    let kind = match m.kind {
        event::MouseEventKind::Down(b) => MouseKind::Press(button(b)),
        event::MouseEventKind::Up(b) => MouseKind::Release(button(b)),
        event::MouseEventKind::Drag(b) => MouseKind::Drag(button(b)),
        event::MouseEventKind::Moved => MouseKind::Move,
        event::MouseEventKind::ScrollUp => MouseKind::WheelUp,
        event::MouseEventKind::ScrollDown => MouseKind::WheelDown,
        event::MouseEventKind::ScrollLeft | event::MouseEventKind::ScrollRight => return None,
    };
    let mut mods = 0;
    if m.modifiers.contains(KeyModifiers::SHIFT) { mods |= MOUSE_SHIFT }
    if m.modifiers.contains(KeyModifiers::ALT) { mods |= MOUSE_ALT }
    if m.modifiers.contains(KeyModifiers::CONTROL) { mods |= MOUSE_CTRL }
    Some(zmux::protocol::MouseIn { x: m.column, y: m.row, kind, mods })
}

enum Prefix {
    Detach,
    Command(Cmd),
    Literal,
    /// Enter scroll mode: keys drive history until q/Esc/Enter leaves it.
    Scroll,
    /// Hand the mouse to the other side - see `config::Mouse`.
    MouseToggle,
    /// Put the shortcut list on screen.
    Help,
    Ignore,
}

/// Read the key following the prefix and map it to an action.
///
/// Bindings follow tmux so muscle memory transfers.
fn read_prefix_command(cfg: &zmux::config::Config) -> Result<Prefix> {
    // Bounded wait: a prefix followed by nothing must not wedge the client.
    if !event::poll(Duration::from_millis(2000))? {
        return Ok(Prefix::Ignore);
    }
    let Event::Key(k) = event::read()? else { return Ok(Prefix::Ignore) };
    if matches!(k.kind, event::KeyEventKind::Release) {
        // The release of the prefix key itself; wait for a real press.
        return read_prefix_command(cfg);
    }
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    Ok(match k.code {
        KeyCode::Char('d') => Prefix::Detach,
        KeyCode::Char(c) if ctrl && c.eq_ignore_ascii_case(&cfg.prefix) => Prefix::Literal,
        KeyCode::Char('%') => Prefix::Command(Cmd::SplitHorizontal),
        KeyCode::Char('"') => Prefix::Command(Cmd::SplitVertical),
        KeyCode::Char('x') => Prefix::Command(Cmd::ClosePane),
        KeyCode::Char('c') => Prefix::Command(Cmd::NewWindow),
        KeyCode::Char('n') => Prefix::Command(Cmd::NextWindow),
        KeyCode::Char('p') => Prefix::Command(Cmd::PrevWindow),
        // Digits jump to a window by the index the status bar shows. Only one
        // digit, as in tmux: beyond nine windows, `C-b n` is the way.
        KeyCode::Char(c @ '0'..='9') => {
            Prefix::Command(Cmd::SelectWindow(c as u8 - b'0'))
        }
        KeyCode::Char('[') => Prefix::Scroll,
        KeyCode::Char('m') => Prefix::MouseToggle,
        KeyCode::Char('?') => Prefix::Help,
        KeyCode::PageUp => Prefix::Command(Cmd::ScrollPageUp),
        KeyCode::Left if ctrl => Prefix::Command(Cmd::ResizeLeft),
        KeyCode::Right if ctrl => Prefix::Command(Cmd::ResizeRight),
        KeyCode::Up if ctrl => Prefix::Command(Cmd::ResizeUp),
        KeyCode::Down if ctrl => Prefix::Command(Cmd::ResizeDown),
        KeyCode::Left => Prefix::Command(Cmd::FocusLeft),
        KeyCode::Right => Prefix::Command(Cmd::FocusRight),
        KeyCode::Up => Prefix::Command(Cmd::FocusUp),
        KeyCode::Down => Prefix::Command(Cmd::FocusDown),
        _ => Prefix::Ignore,
    })
}

/// Scroll mode: arrows and page keys move through history; q, Esc or Enter
/// leave it and snap back to live output.
///
/// Blocking here keeps the mode explicit and avoids threading a mode flag
/// through the main loop, at the cost of not repainting while scrolling - the
/// daemon still pushes frames, which the next loop iteration drains.
/// Returns false if the connection died while we were in scroll mode - the
/// main loop watches the frame channel, but this one has its own input loop
/// and would otherwise spin forever in raw alternate-screen mode.
fn scroll_mode(stream: &mut interprocess::local_socket::Stream, had_mouse: bool) -> Result<bool> {
    // Scroll mode is zmux's own, so zmux takes the mouse for the duration
    // whatever `mouse` says: a wheel is the obvious way to move through
    // history, and under `auto` there is no program asking for it here.
    if !had_mouse {
        set_mouse_capture(true)?;
    }
    let out = scroll_keys(stream);
    if !had_mouse {
        set_mouse_capture(false)?;
    }
    out
}

fn scroll_keys(stream: &mut interprocess::local_socket::Stream) -> Result<bool> {
    loop {
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let cmd = match event::read()? {
            // Releases would scroll twice per keypress.
            Event::Key(k) if matches!(k.kind, event::KeyEventKind::Release) => continue,
            Event::Key(k) => match k.code {
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => {
                    return Ok(ipc::write_msg(stream,
                        &ClientMsg::Command(Cmd::ScrollToBottom)).is_ok());
                }
                KeyCode::Up | KeyCode::Char('k') => Cmd::ScrollUp,
                KeyCode::Down | KeyCode::Char('j') => Cmd::ScrollDown,
                KeyCode::PageUp | KeyCode::Char(' ') => Cmd::ScrollPageUp,
                KeyCode::PageDown => Cmd::ScrollPageDown,
                KeyCode::Home => Cmd::ScrollPageUp,
                KeyCode::End => {
                    return Ok(ipc::write_msg(stream,
                        &ClientMsg::Command(Cmd::ScrollToBottom)).is_ok());
                }
                _ => continue,
            },
            Event::Mouse(m) => match m.kind {
                event::MouseEventKind::ScrollUp => Cmd::ScrollUp,
                event::MouseEventKind::ScrollDown => Cmd::ScrollDown,
                _ => continue,
            },
            Event::Resize(c, r) => {
                if ipc::write_msg(stream, &ClientMsg::Resize { cols: c, rows: r }).is_err() {
                    return Ok(false);
                }
                continue;
            }
            _ => continue,
        };
        // A failed write is how we notice the daemon has gone.
        if ipc::write_msg(stream, &ClientMsg::Command(cmd)).is_err() {
            return Ok(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The escapes the skill file promises. `\x03` is the one that matters:
    /// it is how a caller interrupts whatever is running in a pane, and it
    /// used to arrive as NUL, which a shell ignores - so "send Ctrl-C" did
    /// nothing at all and said it had worked.
    #[test]
    fn hex_escapes_decode_both_digits() {
        assert_eq!(unescape(r"\x03"), vec![0x03]);
        assert_eq!(unescape(r"\x1b"), vec![0x1b]);
        assert_eq!(unescape(r"\x41"), b"A".to_vec());
        assert_eq!(unescape(r"a\x03b"), vec![b'a', 0x03, b'b']);
        assert_eq!(unescape(r"\xff"), vec![0xff]);
    }

    #[test]
    fn the_named_escapes_are_the_control_bytes_they_name() {
        assert_eq!(unescape(r"\r\n\t\e"), vec![b'\r', b'\n', b'\t', 0x1b]);
        assert_eq!(unescape(r"\\"), vec![b'\\']);
    }

    /// A Windows path is mostly backslashes and none of them are escapes.
    /// Swallowing them would silently retype the argument as something else.
    #[test]
    fn an_unknown_escape_is_left_alone() {
        assert_eq!(unescape(r"C:\Users\dev"), b"C:\\Users\\dev".to_vec());
        // Including `\x` without two hex digits after it.
        assert_eq!(unescape(r"\xzz"), b"\\xzz".to_vec());
        assert_eq!(unescape(r"\x4z"), b"\\x4z".to_vec());
        assert_eq!(unescape(r"\x"), b"\\x".to_vec());
        assert_eq!(unescape(r"ends\"), b"ends\\".to_vec());
    }

    /// `+3` parses as 3 for `from_str_radix`, so the digits are checked
    /// rather than trusted: `\x+3` is not a byte, it is text.
    #[test]
    fn a_sign_is_not_a_hex_digit() {
        assert_eq!(unescape(r"\x+3"), b"\\x+3".to_vec());
    }

    #[test]
    fn a_pane_flag_is_taken_only_from_the_front() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        let args = a(&["-p", "2", "cargo", "test"]);
        let (pane, rest) = take_pane(&args).unwrap();
        assert_eq!(pane, Some(2));
        assert_eq!(rest, &args[2..]);

        // No flag: every argument is text, and nothing is consumed.
        let args = a(&["cargo", "test"]);
        let (pane, rest) = take_pane(&args).unwrap();
        assert_eq!(pane, None);
        assert_eq!(rest, &args[..]);

        // A flag with no number is an error, not a silently ignored flag -
        // otherwise `-p` alone would type "-p" into the focused pane.
        assert!(take_pane(&a(&["-p"])).is_err());
        assert!(take_pane(&a(&["-p", "x"])).is_err());
        // Beyond a u8 there is no such pane anyway (MAX_PANES is 64).
        assert!(take_pane(&a(&["-p", "300"])).is_err());
    }

    #[test]
    fn capture_takes_its_flags_in_either_order() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        assert_eq!(capture_args(&a(&[])).unwrap(), (None, 0));
        assert_eq!(capture_args(&a(&["-p", "2"])).unwrap(), (Some(2), 0));
        assert_eq!(capture_args(&a(&["-n", "20"])).unwrap(), (None, 20));
        assert_eq!(capture_args(&a(&["-p", "2", "-n", "20"])).unwrap(), (Some(2), 20));
        // The order that used to lose the pane silently.
        assert_eq!(capture_args(&a(&["-n", "20", "-p", "2"])).unwrap(), (Some(2), 20));
        assert_eq!(capture_args(&a(&["--pane", "3", "--lines", "5"])).unwrap(), (Some(3), 5));
    }

    /// Answering with the WRONG pane is worse than answering with an error,
    /// so anything not understood stops the command.
    #[test]
    fn capture_refuses_an_argument_it_would_otherwise_ignore() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        let e = capture_args(&a(&["-p", "2", "extra"])).unwrap_err().to_string();
        assert!(e.contains("unexpected argument 'extra'"), "misleading error: {e}");
        // Not "extra needs a number": it is not a flag, it is a stray word.
        assert!(!e.contains("needs a number"), "misleading error: {e}");
        assert!(capture_args(&a(&["-p"])).is_err(), "a flag with no value must not pass");
        assert!(capture_args(&a(&["-n", "lots"])).is_err());
        assert!(capture_args(&a(&["-p", "300"])).is_err(), "no pane is beyond a u8");
    }

    /// Every name the CLI prints in its own help must resolve to a command.
    /// The list and the parser used to be able to drift apart.
    #[test]
    fn every_advertised_command_name_parses() {
        let names: Vec<&str> = CMD_NAMES
            .lines()
            .skip(1)
            .flat_map(str::split_whitespace)
            .filter(|w| !w.starts_with('<'))
            .collect();
        assert!(names.len() >= 19, "the list looks empty: {names:?}");
        for n in names {
            assert!(parse_cmd(n, Some("0")).is_some(), "`send` advertises '{n}' but cannot parse it");
        }
        assert!(parse_cmd("no-such-command", None).is_none());
        // `select-window` needs its index; without one it is not a command.
        assert!(parse_cmd("select-window", None).is_none());
    }
}
