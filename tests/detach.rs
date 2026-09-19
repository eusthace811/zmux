//! Phase 2: detaching must NOT kill the session.
//!
//! These exist because an earlier daemon owned the pane inside the connection
//! handler, so detaching dropped the ConPTY and killed the shell. It looked
//! like it worked - the client exited cleanly - and persistence was a fiction.

mod common;
use common::{text_of, wait_for, Daemon};

use zmux::ipc;
use zmux::protocol::{ClientMsg, ServerMsg, PROTOCOL_VERSION};

fn hello(s: &mut interprocess::local_socket::Stream, cols: u16, rows: u16) {
    ipc::write_msg(s, &ClientMsg::Hello { version: PROTOCOL_VERSION, cols, rows }).expect("hello");
    match ipc::read_msg::<_, ServerMsg>(s).expect("reply") {
        Some(ServerMsg::Hello { .. }) => {}
        other => panic!("expected Hello, got {other:?}"),
    }
}

#[test]
fn shell_survives_detach_and_state_is_restored_on_reattach() {
    let d = Daemon::start("zmux-test-detach");

    // Attach, leave a marker in the scrollback, detach.
    {
        let mut s = ipc::connect(&d.session).expect("connect");
        hello(&mut s, 80, 24);
        assert!(wait_for(&mut s, 20, |t| t.contains('$')).is_some(), "no prompt");
        ipc::write_msg(&mut s, &ClientMsg::Input { data: b"echo PERSIST_MARKER\r".to_vec() })
            .expect("input");
        assert!(
            wait_for(&mut s, 20, |t| t.matches("PERSIST_MARKER").count() >= 2).is_some(),
            "marker never rendered"
        );
        ipc::write_msg(&mut s, &ClientMsg::Detach).expect("detach");
    }

    std::thread::sleep(std::time::Duration::from_millis(500));

    // The daemon must still be listening - this is the whole feature.
    assert!(ipc::is_running(&d.session), "daemon died on detach; session was not persistent");

    // Reattach: the first frame must already carry the pre-detach screen.
    let mut s2 = ipc::connect(&d.session).expect("reconnect");
    hello(&mut s2, 80, 24);
    let first = match ipc::read_msg::<_, ServerMsg>(&mut s2).expect("frame") {
        Some(ServerMsg::Frame(f)) => text_of(&f),
        other => panic!("expected an immediate full Frame on attach, got {other:?}"),
    };
    assert!(
        first.contains("PERSIST_MARKER"),
        "reattach did not resync prior screen state; got:\n{first}"
    );

    // And the same shell is still alive, not a fresh one.
    ipc::write_msg(&mut s2, &ClientMsg::Input { data: b"echo SECOND_MARKER\r".to_vec() })
        .expect("input");
    assert!(
        wait_for(&mut s2, 20, |t| t.contains("SECOND_MARKER") && t.contains("PERSIST_MARKER"))
            .is_some(),
        "shell did not respond after reattach"
    );
}

#[test]
fn reattaching_at_a_different_size_resizes_the_pty() {
    let d = Daemon::start("zmux-test-resize");
    {
        let mut s = ipc::connect(&d.session).expect("connect");
        hello(&mut s, 80, 24);
        assert!(wait_for(&mut s, 20, |t| t.contains('$')).is_some(), "no prompt");
        ipc::write_msg(&mut s, &ClientMsg::Detach).expect("detach");
    }
    let mut s2 = ipc::connect(&d.session).expect("reconnect");
    hello(&mut s2, 100, 30);
    // `tput cols` reports what the pty believes, which is what we changed.
    ipc::write_msg(&mut s2, &ClientMsg::Input { data: b"tput cols\r".to_vec() }).expect("input");
    assert!(
        wait_for(&mut s2, 20, |t| t.contains("100")).is_some(),
        "pty was not resized to the reattaching client's viewport"
    );
}

#[test]
fn kill_terminates_the_daemon() {
    let d = Daemon::start("zmux-test-kill");
    assert!(ipc::is_running(&d.session));

    let mut s = ipc::connect(&d.session).expect("connect");
    ipc::write_msg(&mut s, &ClientMsg::Shutdown).expect("shutdown");
    let _ = ipc::read_msg::<_, ServerMsg>(&mut s);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline && ipc::is_running(&d.session) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(!ipc::is_running(&d.session), "daemon still listening after Shutdown");
}

#[test]
fn live_sessions_are_discoverable() {
    let d = Daemon::start("zmux-test-listing");
    let found = ipc::list_sessions();
    assert!(
        found.iter().any(|s| s == &d.session),
        "ls did not find the running session; saw {found:?}"
    );
}
