//! zmux - the client. Draws frames and forwards keystrokes. Owns no state,
//! so detaching loses nothing. DESIGN.md §3.

use std::io::{stdout, Write};
use std::time::Duration;

use anyhow::{bail, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::{execute, terminal};
use interprocess::TryClone as _;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use zmux::ipc;
use zmux::protocol::{Cmd, ClientMsg, Frame, ServerMsg, PROTOCOL_VERSION};
use zmux::render::{paint, paint_cursor};

/// Restores terminal state even on panic or early return.
struct TermGuard;

impl TermGuard {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        // DisableLineWrap matters: herdr notes clients must disable host
        // autowrap or writing the last column wraps and corrupts the next row
        // (render_ansi.rs:986, terminal_setup.rs:114). DESIGN.md §0.4.
        execute!(stdout(), terminal::EnterAlternateScreen, terminal::DisableLineWrap,
                 crossterm::event::EnableMouseCapture, crossterm::cursor::Hide)?;
        Ok(Self)
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = execute!(stdout(), crossterm::cursor::Show,
                         crossterm::event::DisableMouseCapture, terminal::EnableLineWrap,
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
    // DESIGN.md §0.2: DETACHED_PROCESS only. No WMI - that is MITRE T1047 and
    // a high-signal EDR detection on a managed laptop. Probe C confirmed this
    // is sufficient; CREATE_BREAKAWAY_FROM_JOB is the documented fallback rung.
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
  zmux --help

In a session (Ctrl-b is the prefix):
  C-b d          detach              C-b %          split left/right
  C-b x          close pane          C-b \"          split top/bottom
  C-b <arrow>    move focus          C-b C-<arrow>  resize
  C-b C-b        send a literal Ctrl-b";

fn main() -> Result<()> {
    // Before anything that could trigger a LoadLibrary (see harden.rs).
    zmux::harden::restrict_dll_search_path();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--help" | "-h") => {
            println!("{USAGE}");
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
        Some("kill") => {
            let Some(name) = args.get(1) else { bail!("usage: zmux kill <session>") };
            if !ipc::is_running(name) {
                bail!("no such session: {name}");
            }
            let mut s = ipc::connect(name)?;
            ipc::write_msg(&mut s, &ClientMsg::Shutdown)?;
            let _ = ipc::read_msg::<_, ServerMsg>(&mut s);
            println!("killed {name}");
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
        Some(ServerMsg::Hello { session, .. }) => eprintln!("attached to {session}"),
        Some(ServerMsg::Refused { reason }) => bail!("daemon refused: {reason}"),
        other => bail!("unexpected handshake reply: {other:?}"),
    }

    let _guard = TermGuard::enter()?;
    let mut term = Terminal::new(CrosstermBackend::new(stdout()))?;

    // Frames arrive on their own thread; the main loop owns input and drawing.
    let (frame_tx, frame_rx) = std::sync::mpsc::channel::<ServerMsg>();
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
    loop {
        // Input. Ctrl-b is the prefix, as in tmux.
        if event::poll(Duration::from_millis(8))? {
            match event::read()? {
                Event::Key(k) if is_prefix(&k, &cfg) => match read_prefix_command(&cfg)? {
                    Prefix::Detach => {
                        ipc::write_msg(&mut stream, &ClientMsg::Detach)?;
                        break;
                    }
                    Prefix::Scroll => {
                        if !scroll_mode(&mut stream)? {
                            drop(_guard);
                            eprintln!("lost connection to the daemon");
                            return Ok(());
                        }
                    }
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
                Event::Mouse(m) => match m.kind {
                    event::MouseEventKind::Down(event::MouseButton::Left) => {
                        ipc::write_msg(&mut stream, &ClientMsg::Click { x: m.column, y: m.row })?;
                    }
                    // Wheel scrolls history directly - no mode to enter.
                    event::MouseEventKind::ScrollUp => {
                        ipc::write_msg(&mut stream, &ClientMsg::Command(Cmd::ScrollUp))?;
                    }
                    event::MouseEventKind::ScrollDown => {
                        ipc::write_msg(&mut stream, &ClientMsg::Command(Cmd::ScrollDown))?;
                    }
                    _ => {}
                },
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

        if let Some(f) = &latest {
            // ratatui diffs against its own previous buffer and emits only the
            // changed cells - no hand-written blitter (DESIGN.md §0.4).
            term.draw(|frame| {
                let buf = frame.buffer_mut();
                paint(buf, f, 0, 0);
                paint_cursor(buf, f, 0, 0);
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

enum Prefix {
    Detach,
    Command(Cmd),
    Literal,
    /// Enter scroll mode: keys drive history until q/Esc/Enter leaves it.
    Scroll,
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
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    Ok(match k.code {
        KeyCode::Char('d') => Prefix::Detach,
        KeyCode::Char(c) if ctrl && c.eq_ignore_ascii_case(&cfg.prefix) => Prefix::Literal,
        KeyCode::Char('%') => Prefix::Command(Cmd::SplitHorizontal),
        KeyCode::Char('"') => Prefix::Command(Cmd::SplitVertical),
        KeyCode::Char('x') => Prefix::Command(Cmd::ClosePane),
        KeyCode::Char('[') => Prefix::Scroll,
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
fn scroll_mode(stream: &mut interprocess::local_socket::Stream) -> Result<bool> {
    loop {
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let cmd = match event::read()? {
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
