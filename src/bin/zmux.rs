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
use zmux::protocol::{ClientMsg, Frame, ServerMsg, PROTOCOL_VERSION};
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
                 crossterm::cursor::Hide)?;
        Ok(Self)
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
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

In a session:  Ctrl-b d  detach";

fn main() -> Result<()> {
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
                break;
            }
        }
    });

    let mut latest: Option<Frame> = None;
    loop {
        // Input. Ctrl-b d detaches (tmux muscle memory).
        if event::poll(Duration::from_millis(8))? {
            match event::read()? {
                Event::Key(k) => {
                    if k.code == KeyCode::Char('b') && k.modifiers.contains(KeyModifiers::CONTROL) {
                        if let Event::Key(next) = event::read()? {
                            if next.code == KeyCode::Char('d') {
                                ipc::write_msg(&mut stream, &ClientMsg::Detach)?;
                                break;
                            }
                        }
                        continue;
                    }
                    if let Some(bytes) = encode_key(k) {
                        ipc::write_msg(&mut stream, &ClientMsg::Input { data: bytes })?;
                    }
                }
                Event::Resize(c, r) => {
                    ipc::write_msg(&mut stream, &ClientMsg::Resize { cols: c, rows: r })?;
                }
                _ => {}
            }
        }

        // Newest frame wins; older ones are stale by definition.
        while let Ok(msg) = frame_rx.try_recv() {
            match msg {
                ServerMsg::Frame(f) => latest = Some(f),
                ServerMsg::PaneExited { code } => {
                    drop(_guard);
                    println!("pane exited with code {code}");
                    return Ok(());
                }
                ServerMsg::Detached => return Ok(()),
                _ => {}
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

/// Minimal key encoder for Phase 1.
///
/// Phase 2 replaces this with herdr's `input/encode.rs`, which handles the
/// Kitty keyboard protocol, application cursor mode and mouse encoding.
fn encode_key(k: KeyEvent) -> Option<Vec<u8>> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    Some(match k.code {
        KeyCode::Char(c) if ctrl => {
            let b = (c.to_ascii_uppercase() as u8).wrapping_sub(b'@');
            vec![b]
        }
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        _ => return None,
    })
}
