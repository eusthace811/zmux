//! End-to-end: daemon + ConPTY + real Git Bash + vt100 + the wire protocol.
//! Needs no console, so this runs from WSL against the Windows binaries.

mod common;
use common::Daemon;

use zmux::ipc;
use zmux::protocol::{ClientMsg, ServerMsg};

#[test]
fn daemon_runs_git_bash_and_renders_output() {
    let d = Daemon::start("zmux-test-echo");
    // A prompt proves ConPTY started bash AND that the startup DSR-CPR
    // handshake was answered - unanswered, the shell never initialises and
    // this times out (DESIGN.md §2.5).
    let mut c = d.attach_ready(80, 24);

    c.input("echo ZMUX_MARKER_OK\r");
    let screen = c
        .wait_for(20, |t| t.matches("ZMUX_MARKER_OK").count() >= 2)
        .expect("command output never appeared");
    // Twice: once echoed by the shell, once as the command's output.
    assert!(screen.matches("ZMUX_MARKER_OK").count() >= 2, "screen was:\n{screen}");
}

#[test]
fn rejects_a_mismatched_protocol_version() {
    let d = Daemon::start("zmux-test-version");
    let mut s = ipc::connect(&d.session).expect("connect");
    ipc::write_msg(&mut s, &ClientMsg::Hello { version: 9999, cols: 80, rows: 24 }).expect("hello");
    match ipc::read_msg::<_, ServerMsg>(&mut s).expect("reply") {
        Some(ServerMsg::Refused { reason }) => assert!(reason.contains("9999"), "{reason}"),
        other => panic!("expected Refused, got {other:?}"),
    }
}

/// Absurd geometry must be clamped, not allocated.
///
/// `65535 x 65535` reaching `vt100::Screen::set_size` asks for ~4 billion
/// cells. A peer on a local named pipe is not automatically trusted.
#[test]
fn absurd_geometry_is_clamped_not_allocated() {
    let d = Daemon::start("zmux-test-geometry");
    let mut c = d.attach(u16::MAX, u16::MAX);
    let msg = c.next_msg(20_000).expect("no frame after an absurd Hello");
    match msg {
        ServerMsg::Frame(f) => {
            assert!(f.cols <= zmux::protocol::MAX_COLS, "cols not clamped: {}", f.cols);
            assert!(f.rows <= zmux::protocol::MAX_ROWS, "rows not clamped: {}", f.rows);
        }
        other => panic!("expected a Frame, got {other:?}"),
    }
    // And the daemon is still healthy.
    assert!(ipc::is_running(&d.session));
}

/// Frames must carry a non-decreasing revision, so a client can drop a stale
/// broadcast that arrives after a newer snapshot.
#[test]
fn frames_carry_monotonic_revisions() {
    let d = Daemon::start("zmux-test-revisions");
    let mut c = d.attach_ready(80, 24);
    c.input("echo REV_A\r");
    assert!(c.wait_for(20, |t| t.contains("REV_A")).is_some());

    let mut seen = Vec::new();
    c.input("echo REV_B\r");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline && seen.len() < 3 {
        match c.next_msg(500) {
            // Most updates are deltas once a client is in sync, so counting
            // only full frames would miss almost everything.
            Some(ServerMsg::Frame(f)) => seen.push(f.revision),
            Some(ServerMsg::Delta(d)) => seen.push(d.revision),
            _ => {}
        }
    }
    assert!(seen.len() >= 2, "expected several updates, saw {}", seen.len());
    assert!(seen.windows(2).all(|w| w[1] >= w[0]), "revisions went backwards: {seen:?}");
}

/// After the first full frame, ordinary output should arrive as deltas
/// carrying only the rows that changed - not the whole grid every tick.
#[test]
fn steady_state_updates_are_deltas_not_full_frames() {
    let d = Daemon::start("zmux-test-deltas");
    let mut c = d.attach_ready(100, 30);

    c.input("echo DELTA_PROBE\r");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let (mut fulls, mut deltas, mut changed_rows) = (0usize, 0usize, Vec::new());
    while std::time::Instant::now() < deadline && (deltas < 2) {
        match c.next_msg(500) {
            Some(ServerMsg::Frame(_)) => fulls += 1,
            Some(ServerMsg::Delta(dl)) => {
                deltas += 1;
                changed_rows.push(dl.rows_changed.len());
            }
            _ => {}
        }
    }
    assert!(deltas >= 1, "no deltas sent; every tick was a full frame ({fulls} of them)");
    // Echoing one line should not repaint thirty rows.
    let smallest = changed_rows.iter().copied().min().unwrap_or(usize::MAX);
    assert!(
        smallest < 30,
        "smallest delta touched {smallest} rows - that is a full repaint, not a delta"
    );
}

/// A delta whose base the client does not have must be refused, not applied on
/// top of the wrong screen.
#[test]
fn a_delta_against_an_unknown_base_is_refused() {
    use zmux::protocol::{Frame, FrameDelta};
    let mut f = Frame::blank(10, 3);
    f.revision = 7;
    let bogus = FrameDelta {
        base_revision: 999,
        revision: 1000,
        cols: 10,
        rows: 3,
        rows_changed: vec![(0, vec![Default::default(); 10])],
        cursor: None,
        app_cursor: false,
    };
    assert!(!f.apply_delta(&bogus), "applied a delta with the wrong base");
    assert_eq!(f.revision, 7, "a refused delta must not mutate the frame");
}

/// A delta whose geometry disagrees must be refused too - applying it would
/// index out of bounds or silently smear rows.
#[test]
fn a_delta_with_mismatched_geometry_is_refused() {
    use zmux::protocol::{Frame, FrameDelta};
    let mut f = Frame::blank(10, 3);
    let wrong = FrameDelta {
        base_revision: 0,
        revision: 1,
        cols: 20,
        rows: 3,
        rows_changed: vec![(0, vec![Default::default(); 20])],
        cursor: None,
        app_cursor: false,
    };
    assert!(!f.apply_delta(&wrong));
}

/// Keys that the Phase 1 encoder could not represent must actually reach the
/// shell. Ctrl-C is the one users notice first.
#[test]
fn control_c_interrupts_a_running_command() {
    let d = Daemon::start("zmux-test-ctrlc");
    let mut c = d.attach_ready(100, 24);

    // The marker is split by shell quoting so the ECHOED COMMAND LINE does not
    // contain it - only the command's OUTPUT would. A naive marker matches the
    // line the user typed and reports failure even when Ctrl-C worked
    // perfectly, which is exactly what happened the first time this was
    // written.
    c.input("sleep 60; echo SHOULD_NOT''_PRINT\r");
    std::thread::sleep(std::time::Duration::from_millis(1200));
    c.send(&ClientMsg::Input { data: vec![0x03] }); // Ctrl-C

    // The prompt must come back well before `sleep 60` could have ended.
    let screen = c
        .wait_for(20, |t| t.matches('$').count() >= 2)
        .expect("Ctrl-C did not interrupt the command");
    assert!(
        !screen.contains("SHOULD_NOT_PRINT"),
        "the command ran to completion instead of being interrupted:\n{screen}"
    );
}

/// Alt-based word motion is readline's, and the old encoder dropped it
/// entirely.
#[test]
fn alt_keys_reach_the_shell_as_escape_prefixed_bytes() {
    let d = Daemon::start("zmux-test-altkeys");
    let mut c = d.attach_ready(100, 24);

    // Type a line, then Alt-b (back one word) and insert a marker mid-line.
    c.input("echo alpha bravo");
    std::thread::sleep(std::time::Duration::from_millis(400));
    c.send(&ClientMsg::Input { data: b"\x1bb".to_vec() }); // Alt-b
    std::thread::sleep(std::time::Duration::from_millis(300));
    c.input("X");
    std::thread::sleep(std::time::Duration::from_millis(300));
    c.input("\r");

    // If Alt-b worked the X lands before "bravo"; otherwise at the end.
    let screen = c
        .wait_for(20, |t| t.contains("alpha Xbravo") || t.contains("alpha bravoX"))
        .expect("neither outcome appeared");
    assert!(
        screen.contains("alpha Xbravo"),
        "Alt-b did not reach readline as a word motion:\n{screen}"
    );
}

/// A shell exiting must actually be noticed.
///
/// This is the regression test for a bug that made every `PtyEvent::Exited`
/// unreachable: the constructor took the child handle OUT of the shared slot
/// the waiter thread polls, so the waiter found `None` on its first tick and
/// returned. Typing `exit` left a dead pane on screen and a session whose last
/// shell had gone lingered until its idle timeout. Nothing in the suite typed
/// `exit`, so it went unnoticed.
#[test]
fn typing_exit_in_the_only_pane_ends_the_session() {
    let name = "zmux-test-exit";
    let d = Daemon::start(name);
    let mut c = d.attach_ready(100, 24);
    c.input("exit\r");

    // The client is told, rather than just being cut off.
    let told = (0..40).any(|_| {
        matches!(
            c.next_msg(500),
            Some(ServerMsg::PaneExited { .. } | ServerMsg::ShuttingDown)
        )
    });
    assert!(told, "the client was never told its session had ended");

    let gone = (0..40).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(250));
        !ipc::is_running(name)
    });
    assert!(gone, "the daemon outlived its only shell");
}
