//! End-to-end: spawn the daemon, attach as a client, drive a real Git Bash,
//! and read the rendered grid back.
//!
//! Needs no console, so it runs anywhere - including from WSL against the
//! Windows binaries.

mod common;
use common::{wait_for, Daemon};

use zmux::ipc;
use zmux::protocol::{ClientMsg, ServerMsg, PROTOCOL_VERSION};

#[test]
fn daemon_runs_git_bash_and_renders_output() {
    let d = Daemon::start("zmux-test-echo");
    let mut s = ipc::connect(&d.session).expect("connect");

    ipc::write_msg(&mut s, &ClientMsg::Hello { version: PROTOCOL_VERSION, cols: 80, rows: 24 })
        .expect("hello");
    match ipc::read_msg::<_, ServerMsg>(&mut s).expect("reply") {
        Some(ServerMsg::Hello { version, .. }) => assert_eq!(version, PROTOCOL_VERSION),
        other => panic!("expected Hello, got {other:?}"),
    }

    // A prompt proves ConPTY started bash AND that the startup DSR-CPR
    // handshake was answered - without the reply the shell never initialises
    // and this hangs (DESIGN.md §2.5).
    assert!(
        wait_for(&mut s, 20, |t| t.contains('$')).is_some(),
        "no shell prompt appeared - CPR handshake likely unanswered"
    );

    ipc::write_msg(&mut s, &ClientMsg::Input { data: b"echo ZMUX_MARKER_OK\r".to_vec() })
        .expect("send input");

    let screen = wait_for(&mut s, 20, |t| t.matches("ZMUX_MARKER_OK").count() >= 2)
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
