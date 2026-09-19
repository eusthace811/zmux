//! zmuxd - the daemon.
//!
//! Owns the session for its whole lifetime. Clients attach and detach; the
//! ConPTY and terminal state do not care (DESIGN.md §3).

use std::sync::atomic::Ordering;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use interprocess::local_socket::prelude::*;
use interprocess::TryClone as _;

use zmux::ipc;
use zmux::layout::{Dir, Nav, PaneId};
use zmux::protocol::{Cmd, ClientMsg, ServerMsg, PROTOCOL_VERSION};
use zmux::pty::PtyEvent;
use zmux::session::Session;

/// How long an idle daemon lingers with no clients before exiting.
/// `None` would mean "forever"; a bounded default avoids orphaned daemons.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60 * 60 * 8);

fn main() -> Result<()> {
    let session_name = std::env::args().nth(1).unwrap_or_else(|| "default".into());
    eprintln!("[zmuxd] session={session_name}");

    let listener = ipc::listen(&session_name)?;
    eprintln!("[zmuxd] listening on {}", ipc::pipe_name(&session_name));

    // The session is created ONCE and outlives every connection.
    let (session, events) = Session::new(&session_name, 80, 24)?;

    // Pump: pty output -> vt100 (already fed on the reader thread) -> broadcast.
    {
        let s = Arc::clone(&session);
        std::thread::spawn(move || pump(s, events));
    }

    // Exit once the pane is gone, or after a long idle with nobody attached.
    {
        let s = Arc::clone(&session);
        std::thread::spawn(move || {
            let mut idle_since: Option<std::time::Instant> = None;
            while s.running.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(5));
                if s.client_count() == 0 {
                    let since = idle_since.get_or_insert_with(std::time::Instant::now);
                    if since.elapsed() > IDLE_TIMEOUT {
                        eprintln!("[zmuxd] idle timeout, exiting");
                        s.shutdown();
                    }
                } else {
                    idle_since = None;
                }
            }
            std::process::exit(0);
        });
    }

    while session.running.load(Ordering::Relaxed) {
        let stream = match listener.accept() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[zmuxd] accept: {e}");
                continue;
            }
        };
        let s = Arc::clone(&session);
        std::thread::spawn(move || {
            if let Err(e) = serve(s, stream) {
                eprintln!("[zmuxd] connection ended: {e}");
            }
        });
    }
    Ok(())
}

/// Coalesce pty output into frames. Output arrives in bursts, so redrawing per
/// chunk would be wasteful; we mark dirty and emit at most one frame per tick.
fn pump(session: Arc<Session>, events: std::sync::mpsc::Receiver<(PaneId, PtyEvent)>) {
    let mut dirty = false;
    loop {
        match events.recv_timeout(Duration::from_millis(16)) {
            Ok((_, PtyEvent::Output(_))) => dirty = true,
            Ok((id, PtyEvent::Exited(code))) => {
                // Only the LAST pane exiting ends the session; otherwise the
                // pane is removed and the survivors reclaim its space.
                if session.pane_exited(id) {
                    session.broadcast(&ServerMsg::PaneExited { code });
                    session.shutdown();
                    return;
                }
                dirty = true;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                session.shutdown();
                return;
            }
        }
        if dirty && session.client_count() > 0 {
            session.revision.fetch_add(1, Ordering::Relaxed);
            session.broadcast(&ServerMsg::Frame(session.snapshot()));
            dirty = false;
        } else if dirty && session.client_count() == 0 {
            // Nobody attached: keep parsing (vt100 is fed on the reader thread)
            // but do not render. Reattach sends a full frame anyway.
            dirty = false;
        }
    }
}

fn apply(session: &Arc<Session>, cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::SplitHorizontal => {
            session.split(Dir::Horizontal)?;
        }
        Cmd::SplitVertical => {
            session.split(Dir::Vertical)?;
        }
        Cmd::ClosePane => {
            session.close_focused();
        }
        Cmd::FocusLeft => {
            session.focus_dir(Nav::Left);
        }
        Cmd::FocusRight => {
            session.focus_dir(Nav::Right);
        }
        Cmd::FocusUp => {
            session.focus_dir(Nav::Up);
        }
        Cmd::FocusDown => {
            session.focus_dir(Nav::Down);
        }
        Cmd::ResizeLeft => {
            session.resize_focused(Nav::Left, 2);
        }
        Cmd::ResizeRight => {
            session.resize_focused(Nav::Right, 2);
        }
        Cmd::ResizeUp => {
            session.resize_focused(Nav::Up, 1);
        }
        Cmd::ResizeDown => {
            session.resize_focused(Nav::Down, 1);
        }
    }
    Ok(())
}

fn serve(session: Arc<Session>, mut stream: interprocess::local_socket::Stream) -> Result<()> {
    let (cols, rows) = match ipc::read_msg::<_, ClientMsg>(&mut stream)? {
        Some(ClientMsg::Hello { version, cols, rows }) => {
            if version != PROTOCOL_VERSION {
                ipc::write_msg(&mut stream, &ServerMsg::Refused {
                    reason: format!("protocol {version} != daemon {PROTOCOL_VERSION}"),
                })?;
                return Ok(());
            }
            (cols, rows)
        }
        Some(ClientMsg::Shutdown) => {
            ipc::write_msg(&mut stream, &ServerMsg::ShuttingDown)?;
            session.shutdown();
            std::process::exit(0);
        }
        other => {
            ipc::write_msg(&mut stream, &ServerMsg::Refused {
                reason: format!("expected Hello, got {other:?}"),
            })?;
            return Ok(());
        }
    };

    ipc::write_msg(&mut stream, &ServerMsg::Hello {
        version: PROTOCOL_VERSION,
        session: session.name.clone(),
    })?;

    let (id, outbox) = session.attach(cols, rows);

    // Full frame immediately on attach: a reattaching client has no history,
    // and deltas are meaningless without a base (DESIGN.md §4 Phase 2).
    ipc::write_msg(&mut stream, &ServerMsg::Frame(session.snapshot()))?;

    // Inbound on its own thread; socket reads block.
    let (in_tx, in_rx) = std::sync::mpsc::channel::<ClientMsg>();
    let mut rstream = stream.try_clone()?;
    std::thread::spawn(move || {
        while let Ok(Some(msg)) = ipc::read_msg::<_, ClientMsg>(&mut rstream) {
            if in_tx.send(msg).is_err() {
                break;
            }
        }
        let _ = in_tx.send(ClientMsg::Detach);
    });

    let result = (|| -> Result<()> {
        loop {
            while let Ok(msg) = in_rx.try_recv() {
                match msg {
                    ClientMsg::Input { data } => session.write_focused(&data)?,
                    ClientMsg::Command(cmd) => {
                        apply(&session, cmd)?;
                        ipc::write_msg(&mut stream, &ServerMsg::Frame(session.snapshot()))?;
                    }
                    ClientMsg::Click { x, y } => {
                        if session.focus_at(x, y) {
                            ipc::write_msg(&mut stream, &ServerMsg::Frame(session.snapshot()))?;
                        }
                    }
                    ClientMsg::Resize { cols, rows } => session.set_client_size(id, cols, rows),
                    ClientMsg::Detach => {
                        let _ = ipc::write_msg(&mut stream, &ServerMsg::Detached);
                        return Ok(());
                    }
                    ClientMsg::Shutdown => {
                        session.shutdown();
                        let _ = ipc::write_msg(&mut stream, &ServerMsg::ShuttingDown);
                        return Ok(());
                    }
                    ClientMsg::Hello { .. } => {}
                }
            }
            match outbox.recv_timeout(Duration::from_millis(16)) {
                Ok(msg) => {
                    let stop = matches!(msg, ServerMsg::PaneExited { .. } | ServerMsg::ShuttingDown);
                    ipc::write_msg(&mut stream, &msg)?;
                    if stop {
                        return Ok(());
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
            if !session.running.load(Ordering::Relaxed) {
                return Ok(());
            }
        }
    })();

    // Always deregister, however we left: a stale client would keep the pty
    // sized to a viewport nobody is looking at.
    session.detach(id);
    result
}
