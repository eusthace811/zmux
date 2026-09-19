//! zmuxd - the daemon. Owns the ConPTY and the terminal state; survives the
//! client detaching. DESIGN.md §3.

use std::io::Write;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use interprocess::local_socket::prelude::*;
use interprocess::TryClone as _;
use zmux::ipc;
use zmux::protocol::{ClientMsg, ServerMsg, PROTOCOL_VERSION};
use zmux::pty::{Pane, PtyEvent};
use zmux::render::frame_from_screen;
use zmux::term::Term;

const SCROLLBACK: usize = 10_000;

fn main() -> Result<()> {
    let session = std::env::args().nth(1).unwrap_or_else(|| "default".into());
    eprintln!("[zmuxd] session={session}");

    let listener = ipc::listen(&session)?;
    eprintln!("[zmuxd] listening on {}", ipc::pipe_name(&session));

    loop {
        let mut stream = match listener.accept() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[zmuxd] accept: {e}");
                continue;
            }
        };
        if let Err(e) = serve(&mut stream, &session) {
            eprintln!("[zmuxd] client ended: {e}");
        }
    }
}

fn serve(stream: &mut interprocess::local_socket::Stream, session: &str) -> Result<()> {
    // --- handshake, with version check (DESIGN.md §3) ---
    let (cols, rows) = match ipc::read_msg::<_, ClientMsg>(stream)? {
        Some(ClientMsg::Hello { version, cols, rows }) => {
            if version != PROTOCOL_VERSION {
                ipc::write_msg(stream, &ServerMsg::Refused {
                    reason: format!("protocol {version} != daemon {PROTOCOL_VERSION}"),
                })?;
                return Ok(());
            }
            (cols, rows)
        }
        other => {
            ipc::write_msg(stream, &ServerMsg::Refused { reason: format!("expected Hello, got {other:?}") })?;
            return Ok(());
        }
    };
    ipc::write_msg(stream, &ServerMsg::Hello { version: PROTOCOL_VERSION, session: session.into() })?;

    // --- terminal state, shared with the pty reader thread ---
    let term = Arc::new(Mutex::new(Term::new(cols, rows, SCROLLBACK)));
    let feed = Arc::clone(&term);

    // Every chunk: feed vt100, then answer any device queries. Answering is
    // MANDATORY - see Pane::spawn and DESIGN.md §2.5.
    let pane = Pane::spawn(cols, rows, move |chunk| {
        let mut t = match feed.lock() { Ok(t) => t, Err(p) => p.into_inner() };
        t.parser.process(chunk);
        let cur = t.cursor();
        t.responder.scan(chunk, cur)
    })?;

    // Reading from the socket blocks, so input gets its own thread.
    let (in_tx, in_rx) = std::sync::mpsc::channel::<ClientMsg>();
    let mut rstream = stream.try_clone()?;
    std::thread::spawn(move || {
        while let Ok(Some(msg)) = ipc::read_msg::<_, ClientMsg>(&mut rstream) {
            if in_tx.send(msg).is_err() {
                break;
            }
        }
    });

    let mut dirty = true;
    loop {
        // Drain client input.
        while let Ok(msg) = in_rx.try_recv() {
            match msg {
                ClientMsg::Input { data } => pane.write_input(&data)?,
                ClientMsg::Resize { cols, rows } => {
                    pane.resize(cols, rows)?;
                    if let Ok(mut t) = term.lock() {
                        t.resize(cols, rows);
                    }
                    dirty = true;
                }
                ClientMsg::Detach => {
                    ipc::write_msg(stream, &ServerMsg::Detached)?;
                    return Ok(());
                }
                ClientMsg::Hello { .. } => {}
            }
        }

        // Drain pty output.
        match pane.events.recv_timeout(Duration::from_millis(16)) {
            Ok(PtyEvent::Output(_)) => dirty = true,
            Ok(PtyEvent::Exited(code)) => {
                let _ = ipc::write_msg(stream, &ServerMsg::PaneExited { code });
                return Ok(());
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }

        if dirty {
            let frame = {
                let t = match term.lock() { Ok(t) => t, Err(p) => p.into_inner() };
                frame_from_screen(t.parser.screen())
            };
            ipc::write_msg(stream, &ServerMsg::Frame(frame))?;
            let _ = std::io::stderr().flush();
            dirty = false;
        }
    }
}
