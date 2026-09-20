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
use zmux::protocol::{Cmd, ClientMsg, Query, ServerMsg, Target, PROTOCOL_VERSION};
use zmux::pty::PtyEvent;
use zmux::session::{Outbound, Session};

fn main() -> Result<()> {
    // Before anything that could trigger a LoadLibrary (see harden.rs).
    zmux::harden::restrict_dll_search_path();
    let session_name = std::env::args().nth(1).unwrap_or_else(|| "default".into());
    eprintln!("[zmuxd] session={session_name}");

    // Every pane shell inherits the daemon's environment, so setting these
    // two here is all it takes for anything running in a pane - a script, a
    // prompt, a coding agent - to know where it is. Set before Session::new,
    // which spawns the first pane. herdr does the same with HERDR_ENV; the
    // point of a marker like this is that "am I inside?" has an answer that
    // does not involve guessing from process ancestry.
    std::env::set_var("ZMUX", "1");
    std::env::set_var("ZMUX_SESSION", &session_name);

    // Atomically claim the session name. This must outlive the listener, so
    // it is bound to a name rather than dropped at the end of the statement -
    // `let _ = ...` would release it immediately and defeat the whole point.
    //
    // Retry briefly. A daemon releases the claim at process exit, which is
    // marginally AFTER its listener closes, so a client that saw the old one
    // stop and started a replacement can arrive inside that window. Failing
    // outright there would turn an ordinary restart into "already served".
    let _claim = {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(c) = ipc::claim_session(&session_name) {
                break c;
            }
            if std::time::Instant::now() >= deadline {
                eprintln!("[zmuxd] session '{session_name}' is already served by another daemon");
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    };
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
        let idle_timeout = Duration::from_secs(session.cfg.idle_timeout_hours * 3600);
        let wake_name = session_name.clone();
        std::thread::spawn(move || {
            let mut idle_since: Option<std::time::Instant> = None;
            while s.running.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(5));
                if s.client_count() == 0 {
                    let since = idle_since.get_or_insert_with(std::time::Instant::now);
                    if since.elapsed() > idle_timeout {
                        eprintln!("[zmuxd] idle timeout, exiting");
                        s.shutdown();
                    }
                } else {
                    idle_since = None;
                }
            }
            // The main thread is blocked in accept(); shutting the session
            // down does not wake it. Without this the "idle exit" leaves a
            // listening orphan forever.
            let _ = ipc::connect(&wake_name);
        });
    }

    while session.running.load(Ordering::Relaxed) {
        let stream = match listener.accept() {
            Ok(s) => s,
            Err(e) => {
                if !session.running.load(Ordering::Relaxed) {
                    break;
                }
                eprintln!("[zmuxd] accept: {e}");
                continue;
            }
        };
        if !session.running.load(Ordering::Relaxed) {
            break;
        }
        let s = Arc::clone(&session);
        std::thread::spawn(move || {
            if let Err(e) = serve(s, stream) {
                eprintln!("[zmuxd] connection ended: {e}");
            }
        });
    }
    // Tear every ConPTY down explicitly rather than exiting under them.
    session.shutdown();
    eprintln!("[zmuxd] stopped");
    Ok(())
}

/// Frame pacing.
///
/// An earlier version used `recv_timeout(16ms)` and published at the bottom of
/// every successful iteration - but recv_timeout returns IMMEDIATELY when an
/// event is waiting, so continuous output produced one full composite per pty
/// read chunk, hundreds per second. This uses a real deadline: drain whatever
/// has arrived, then publish at most once per FRAME_INTERVAL.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

fn pump(session: Arc<Session>, events: std::sync::mpsc::Receiver<(PaneId, PtyEvent)>) {
    let mut dirty = false;
    let mut next_frame = std::time::Instant::now();
    loop {
        // The session holds its own events_tx, so Disconnected is unreachable
        // while it lives; `running` is the actual stop signal.
        if !session.running.load(Ordering::Relaxed) {
            return;
        }
        session.settle_size();
        match events.recv_timeout(FRAME_INTERVAL) {
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
        if !dirty {
            continue;
        }
        // Nobody attached: keep parsing (vt100 is fed on the reader thread)
        // but do not spend CPU compositing. Reattach sends a full frame.
        if session.client_count() == 0 {
            dirty = false;
            continue;
        }
        let now = std::time::Instant::now();
        if now < next_frame {
            continue;
        }
        next_frame = now + FRAME_INTERVAL;
        session.revision.fetch_add(1, Ordering::Relaxed);
        session.publish();
        dirty = false;
    }
}

/// Whether a command that CREATES something managed to, and what it made.
///
/// `Ok(None)` is the session declining - no room, or already at `MAX_PANES` -
/// and `Err` is the shell refusing to start. Neither may end the connection
/// (see `apply`), but both are things a caller has to be told.
fn created<T>(outcome: Result<Option<T>>, what: &str, id: impl Fn(&T) -> PaneId) -> Applied {
    match outcome {
        Ok(Some(v)) => Ok(Some(id(&v).0)),
        Ok(None) => Err(format!(
            "{what} refused: too small to divide, or the session is at its pane limit"
        )),
        Err(e) => Err(format!("{what} failed: {e}")),
    }
}

/// What a command did: the pane it created, or why it could not run.
type Applied = std::result::Result<Option<u32>, String>;

/// Run one client command. `Err(reason)` when it could not be carried out,
/// `Ok(Some(id))` when it created a pane.
///
/// Never an `Err`, on purpose. Creating a pane can fail - the shell may not
/// start under resource pressure - and propagating that ended the CONNECTION,
/// so a transient failure threw the user out of a session that was otherwise
/// perfectly healthy.
///
/// The reason exists for CONTROL callers. Someone attached can SEE that
/// nothing happened; a script cannot, and a script told its split succeeded
/// goes on to type into a pane that was never created - which is precisely
/// the wrong-shell accident an out-of-range `-p` is refused to avoid. Only
/// the commands that create or destroy report one: a focus command that does
/// nothing at the edge of the screen is not a failure, it is the edge.
fn apply(session: &Arc<Session>, cmd: Cmd) -> Applied {
    match cmd {
        Cmd::SplitHorizontal => created(session.split(Dir::Horizontal), "split", |id| *id),
        Cmd::SplitVertical => created(session.split(Dir::Vertical), "split", |id| *id),
        Cmd::SplitHorizontalBackground => {
            created(session.split_background(Dir::Horizontal), "split", |id| *id)
        }
        Cmd::SplitVerticalBackground => {
            created(session.split_background(Dir::Vertical), "split", |id| *id)
        }
        Cmd::NewWindow => created(session.new_window(), "new window", |(_, id)| *id),
        Cmd::ClosePane => {
            if session.close_focused() {
                Ok(None)
            } else {
                Err("cannot close the session's last pane".to_string())
            }
        }
        Cmd::FocusLeft => {
            session.focus_dir(Nav::Left);
            Ok(None)
        }
        Cmd::FocusRight => {
            session.focus_dir(Nav::Right);
            Ok(None)
        }
        Cmd::FocusUp => {
            session.focus_dir(Nav::Up);
            Ok(None)
        }
        Cmd::FocusDown => {
            session.focus_dir(Nav::Down);
            Ok(None)
        }
        Cmd::ResizeLeft => {
            session.resize_focused(Nav::Left, 2);
            Ok(None)
        }
        Cmd::ResizeRight => {
            session.resize_focused(Nav::Right, 2);
            Ok(None)
        }
        Cmd::ResizeUp => {
            session.resize_focused(Nav::Up, 1);
            Ok(None)
        }
        Cmd::ResizeDown => {
            session.resize_focused(Nav::Down, 1);
            Ok(None)
        }
        Cmd::ScrollUp => {
            session.scroll_focused(Some(1));
            Ok(None)
        }
        Cmd::ScrollDown => {
            session.scroll_focused(Some(-1));
            Ok(None)
        }
        Cmd::ScrollPageUp => {
            session.scroll_focused(Some(10));
            Ok(None)
        }
        Cmd::ScrollPageDown => {
            session.scroll_focused(Some(-10));
            Ok(None)
        }
        Cmd::ScrollToBottom => {
            session.scroll_focused(None);
            Ok(None)
        }
        Cmd::NextWindow => {
            session.next_window();
            Ok(None)
        }
        Cmd::PrevWindow => {
            session.prev_window();
            Ok(None)
        }
        Cmd::SelectWindow(i) => {
            session.select_window(i as usize);
            Ok(None)
        }
    }
}

/// A control connection: one reply per request, no frames, no client record.
///
/// This is the whole agent-facing surface. It deliberately does NOT call
/// `session.attach`: a caller with no terminal must not constrain the
/// composited size (the session is the SMALLEST attached viewport) and must
/// not keep the idle timer from ever firing.
///
/// Mutations answer with the resulting `Info` rather than a bare ack, so a
/// caller that split a pane learns the new pane's position from the same
/// round trip - there is no second query to race against the user.
fn serve_control(
    session: &Arc<Session>,
    mut stream: interprocess::local_socket::Stream,
) -> Result<()> {
    ipc::write_msg(&mut stream, &ServerMsg::Hello {
        version: PROTOCOL_VERSION,
        session: session.name.clone(),
    })?;

    while let Ok(Some(msg)) = ipc::read_msg::<_, ClientMsg>(&mut stream) {
        let reply = match msg {
            // Same path as a keystroke from an attached client: apply, then
            // publish so everyone watching sees it immediately.
            ClientMsg::Command(cmd) => match apply(session, cmd) {
                // A caller told "ok" here would act on a pane that was never
                // created. Still publish: a split that reached the tree and
                // only then failed to start a shell was rolled back, and the
                // pump may already have sent attached clients the
                // intermediate layout. Skipping the repaint would leave a
                // pane on their screen that no longer exists.
                Err(reason) => {
                    session.publish();
                    ServerMsg::Refused { reason }
                }
                // Read the state back BEFORE publishing. `publish` composites
                // a whole screen, and every millisecond of it is a millisecond
                // in which an attached human can move the focus - so the reply
                // would describe where THEY went, not what this command did.
                // The gap cannot be closed entirely without a lock spanning
                // both, so `panes[].id` is the identifier that survives it.
                Ok(made) => {
                    let mut info = session.info();
                    // The one fact the snapshot cannot carry on its own: a
                    // background split leaves `focus` on the OLD pane, so
                    // without this the caller has no way to name what it just
                    // made except by diffing, which races the user.
                    info.created = made;
                    session.publish();
                    ServerMsg::Info(info)
                }
            },
            ClientMsg::Input { data } => match session.write_focused(&data) {
                Ok(true) => {
                    // Publish, unlike the attached path. Writing snaps the
                    // pane back to live output, and an input that produces
                    // nothing - which `zmux keys` can send - has no echo to
                    // wake the pump and repaint it. Costs nothing when nobody
                    // is attached, which is the usual case here.
                    session.publish();
                    ServerMsg::Info(session.info())
                }
                // The focused pane exited between resolving it and writing.
                // An attached client shrugs that off; a caller scripting the
                // session must not be told its command ran.
                Ok(false) => ServerMsg::Refused { reason: "no focused pane".into() },
                Err(e) => ServerMsg::Refused { reason: e.to_string() },
            },
            ClientMsg::InputTo { pane, data } => match session.write_pane(pane as usize, &data) {
                Ok(true) => ServerMsg::Info(session.info()),
                Ok(false) => ServerMsg::Refused { reason: format!("no pane {pane}") },
                Err(e) => ServerMsg::Refused { reason: e.to_string() },
            },
            // Resolve, then act on the ID. A target that does not resolve is
            // refused by name, so a caller can tell "no such pane" from "that
            // pane went away" - and never has its input redirected.
            ClientMsg::InputAt { target, data, paste, enter } => {
                match session.resolve(&target) {
                    Some(id) => match session.write_to(id, &data, paste, enter) {
                        Ok(true) => ServerMsg::Info(session.info()),
                        Ok(false) => ServerMsg::Refused { reason: gone(&target) },
                        Err(e) => ServerMsg::Refused { reason: e.to_string() },
                    },
                    None => ServerMsg::Refused { reason: unknown(&target) },
                }
            }
            // A pane saying what it is doing. Refused for a target that
            // does not resolve, like every other one: a report landing on
            // whichever pane happened to be nearby is worse than none.
            ClientMsg::Report { target, report } => match session.resolve(&target) {
                Some(id) => {
                    session.report(id, &report);
                    ServerMsg::Info(session.info())
                }
                None => ServerMsg::Refused { reason: unknown(&target) },
            },
            ClientMsg::Query(Query::Info) => ServerMsg::Info(session.info()),
            ClientMsg::Query(Query::CaptureAt { target, lines }) => {
                match session.resolve(&target).and_then(|id| session.capture_from(id, lines)) {
                    Some(text) => ServerMsg::Capture { text },
                    None => ServerMsg::Refused { reason: unknown(&target) },
                }
            }
            ClientMsg::Query(Query::Capture { pane, lines }) => {
                match session.capture(pane.map(usize::from), lines) {
                    Some(text) => ServerMsg::Capture { text },
                    None => ServerMsg::Refused {
                        reason: match pane {
                            Some(p) => format!("no pane {p}"),
                            None => "no focused pane".into(),
                        },
                    },
                }
            }
            ClientMsg::Shutdown => {
                let _ = ipc::write_msg(&mut stream, &ServerMsg::ShuttingDown);
                session.shutdown();
                let _ = ipc::connect(&session.name);
                return Ok(());
            }
            ClientMsg::Detach => return Ok(()),
            other => ServerMsg::Refused {
                reason: format!("not valid on a control connection: {other:?}"),
            },
        };
        ipc::write_msg(&mut stream, &reply)?;
    }
    Ok(())
}

/// How a target that resolves to nothing is reported.
///
/// Named back at the caller in their own words - `@7`, `reviewer`, `3` - so
/// the message says which of their arguments was wrong, rather than making
/// them guess which pane the daemon thought they meant.
fn unknown(t: &Target) -> String {
    match t {
        Target::Focused => "no focused pane".into(),
        Target::Position(p) => format!("no pane at position {p} in the current window"),
        Target::Id(id) => format!("no pane with id @{id}"),
        Target::Name(n) => format!("no pane named '{n}'"),
    }
}

/// The pane resolved and then exited before the write landed. A different
/// answer from `unknown`: the caller's target was right, it is just too late.
fn gone(t: &Target) -> String {
    format!("{} - it exited before the write landed", unknown(t))
}

fn serve(session: Arc<Session>, mut stream: interprocess::local_socket::Stream) -> Result<()> {
    // A probe connects, learns the session is alive and closes without saying
    // anything - `zmux ls` and every `is_running` check does exactly that.
    // Propagating those filled the daemon's log with "connection ended: the
    // pipe is being closed", dozens of lines per run, which buries anything
    // that actually went wrong. A peer that says nothing is not an error.
    let first = match ipc::read_msg::<_, ClientMsg>(&mut stream) {
        Ok(Some(msg)) => msg,
        Ok(None) | Err(_) => return Ok(()),
    };
    let (cols, rows) = match first {
        ClientMsg::Hello { version, cols, rows } => {
            if version != PROTOCOL_VERSION {
                // Say what to DO about it. The common cause by far is an
                // upgraded binary meeting a session still served by the daemon
                // the previous build left running, and "protocol 3 != 2" does
                // not tell anyone that killing the session fixes it.
                ipc::write_msg(&mut stream, &ServerMsg::Refused {
                    reason: format!(
                        "client speaks protocol {version}, this daemon speaks \
                         {PROTOCOL_VERSION}. The session is served by an older build; \
                         run `zmux kill {}` and attach again to restart it.",
                        session.name
                    ),
                })?;
                return Ok(());
            }
            (cols, rows)
        }
        ClientMsg::Shutdown => {
            let _ = ipc::write_msg(&mut stream, &ServerMsg::ShuttingDown);
            session.shutdown();
            // Unblock the accept loop so main() can run its clean teardown.
            let _ = ipc::connect(&session.name);
            return Ok(());
        }
        ClientMsg::Control { version } => {
            if version != PROTOCOL_VERSION {
                ipc::write_msg(&mut stream, &ServerMsg::Refused {
                    reason: format!(
                        "control client speaks protocol {version}, this daemon speaks \
                         {PROTOCOL_VERSION}. Run `zmux kill {}` and start it again.",
                        session.name
                    ),
                })?;
                return Ok(());
            }
            return serve_control(&session, stream);
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

    let (id, outbox, latest) = session.attach(cols, rows);

    // Full frame immediately on attach: a reattaching client has no history,
    // and deltas are meaningless without a base (DESIGN.md §4 Phase 2).
    //
    // A broadcast queued between attach() and here would be OLDER than this
    // snapshot but arrive after it. Frames carry their revision and the client
    // drops anything it has already surpassed, so the screen cannot regress.
    ipc::write_msg(&mut stream, &ServerMsg::Frame(session.snapshot()))?;

    // Inbound on its own thread; socket reads block.
    // Bounded: a peer that floods input must not grow the daemon's memory.
    // Backpressure lands on the socket read, which is where it belongs.
    let (in_tx, in_rx) = std::sync::mpsc::sync_channel::<ClientMsg>(256);
    let mut rstream = stream.try_clone()?;
    std::thread::spawn(move || {
        while let Ok(Some(msg)) = ipc::read_msg::<_, ClientMsg>(&mut rstream) {
            if in_tx.send(msg).is_err() {
                break;
            }
        }
        let _ = in_tx.send(ClientMsg::Detach);
    });

    // OUTBOUND on its own thread too.
    //
    // Writing frames from the serve loop deadlocks against a client that stops
    // reading: the daemon blocks filling the pipe with frames while that
    // client blocks trying to send input, and neither side drains. Verified -
    // it wedged a stress test. The read path must keep running however slow
    // the peer is, so writes get their own thread. If that thread wedges only
    // this connection suffers; the session and other clients stay responsive.
    let mut wstream = stream.try_clone()?;
    let writer = std::thread::spawn(move || {
        while let Ok(item) = outbox.recv() {
            let msg = match item {
                // Take whatever is newest; intermediate frames are skipped
                // rather than replayed, which is the point of coalescing.
                Outbound::FrameReady => match latest.lock() {
                    Ok(mut g) => match g.take() {
                        Some(m) => m,
                        None => continue,
                    },
                    Err(p) => match p.into_inner().take() {
                        Some(m) => m,
                        None => continue,
                    },
                },
                Outbound::Control(m) => m,
            };
            let stop = matches!(msg, ServerMsg::PaneExited { .. } | ServerMsg::ShuttingDown);
            if ipc::write_msg(&mut wstream, &msg).is_err() || stop {
                return;
            }
        }
    });

    let result = (|| -> Result<()> {
        loop {
            // Blocking recv with a timeout: the writer thread owns the socket
            // now, so this loop only has to service inbound messages.
            let msg = match in_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(m) => m,
                Err(RecvTimeoutError::Timeout) => {
                    if !session.running.load(Ordering::Relaxed) || writer.is_finished() {
                        return Ok(());
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            };
            {
                match msg {
                    // A rejected oversized input closes this connection rather
                    // than taking the session down.
                    ClientMsg::Input { data } => {
                        if let Err(e) = session.write_focused(&data) {
                            eprintln!("[zmuxd] rejecting input from client {id}: {e}");
                            return Ok(());
                        }
                    }
                    // Commands publish to EVERY client rather than writing a
                    // frame to this one stream. Two reasons: a split or a
                    // focus change is session state that other attached
                    // clients must see, and a direct write would bypass the
                    // delta base, leaving the next delta unapplicable.
                    ClientMsg::Command(cmd) => {
                        // Someone attached can see for themselves that nothing
                        // happened, so the reason only goes to the log.
                        if let Err(reason) = apply(&session, cmd) {
                            eprintln!("[zmuxd] {reason}");
                        }
                        session.publish();
                    }
                    ClientMsg::Mouse(ev) => {
                        if session.mouse(ev) {
                            session.publish();
                        }
                    }
                    ClientMsg::Click { x, y } => {
                        if session.focus_at(x, y) {
                            session.publish();
                        }
                    }
                    ClientMsg::Resize { cols, rows } => session.set_client_size(id, cols, rows),
                    ClientMsg::Detach => {
                        session.detach(id);
                        return Ok(());
                    }
                    ClientMsg::Shutdown => {
                        session.shutdown();
                        return Ok(());
                    }
                    // Control-connection messages. An attached client has no
                    // business sending these, and there is nowhere to put a
                    // reply on a connection that is streaming frames.
                    ClientMsg::Hello { .. }
                    | ClientMsg::Control { .. }
                    | ClientMsg::Query(_)
                    | ClientMsg::InputTo { .. }
                    | ClientMsg::InputAt { .. }
                    | ClientMsg::Report { .. } => {}
                }
            }
        }
    })();

    // Always deregister, however we left: a stale client would keep the pty
    // sized to a viewport nobody is looking at.
    session.detach(id);
    result
}
