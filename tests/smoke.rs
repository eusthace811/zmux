//! End-to-end: daemon + ConPTY + real Git Bash + vt100 + the wire protocol.
//! Needs no console, so this runs from WSL against the Windows binaries.

mod common;
use common::Daemon;

use zmux::ipc;
use zmux::protocol::{ClientMsg, ServerMsg, PROTOCOL_VERSION};

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
        if let Some(ServerMsg::Frame(f)) = c.next_msg(500) {
            seen.push(f.revision);
        }
    }
    assert!(seen.len() >= 2, "expected several frames, saw {}", seen.len());
    assert!(seen.windows(2).all(|w| w[1] >= w[0]), "revisions went backwards: {seen:?}");
}
