//! Failure modes the bounds and lifecycle work exist to handle.
//!
//! These are the cases a review flagged as untested: a client that stops
//! reading, heavy output, many panes, and repeated attach/detach.

mod common;
use common::Daemon;

use std::time::{Duration, Instant};

use zmux::ipc;
use zmux::protocol::{ClientMsg, Cmd, ServerMsg, PROTOCOL_VERSION};

/// A client that handshakes and then never reads must not take the daemon with
/// it. Its queue fills, it gets dropped, and everyone else carries on.
#[test]
fn a_client_that_stops_reading_is_dropped_not_tolerated() {
    let d = Daemon::start("zmux-test-slowclient");
    let mut good = d.attach_ready(90, 24);

    // A deliberately silent peer: handshake, then never read again.
    let mut dead = ipc::connect(&d.session).expect("connect");
    ipc::write_msg(&mut dead, &ClientMsg::Hello { version: PROTOCOL_VERSION, cols: 90, rows: 24 })
        .expect("hello");

    // Generate far more frames than any queue bound.
    good.input("for i in $(seq 1 400); do echo FLOOD_$i; done\r");
    assert!(
        good.wait_for(45, |t| t.contains("FLOOD_400")).is_some(),
        "the healthy client stopped receiving while a silent peer was attached"
    );

    // The daemon must still be responsive afterwards.
    good.input("echo STILL_RESPONSIVE\r");
    assert!(good.wait_for(20, |t| t.contains("STILL_RESPONSIVE")).is_some());
    assert!(ipc::is_running(&d.session));
    drop(dead);
}

/// Heavy continuous output must not be paced into uselessness or lost.
#[test]
fn sustained_output_arrives_and_stays_paced() {
    let d = Daemon::start("zmux-test-flood");
    let mut c = d.attach_ready(100, 30);

    let start = Instant::now();
    c.input("for i in $(seq 1 2000); do echo BULK_$i; done\r");
    let screen = c
        .wait_for(60, |t| t.contains("BULK_2000"))
        .expect("bulk output never completed");
    assert!(screen.contains("BULK_2000"));

    // Frame pacing means we should not have spent minutes on 2000 lines.
    assert!(
        start.elapsed() < Duration::from_secs(50),
        "2000 lines took {:?} - pacing or backpressure is wrong",
        start.elapsed()
    );
}

/// Repeated attach/detach must not leak clients or pin geometry.
#[test]
fn repeated_attach_and_detach_leaves_no_residue() {
    let d = Daemon::start("zmux-test-churn");
    {
        let mut first = d.attach_ready(100, 30);
        first.send(&ClientMsg::Detach);
    }
    for _ in 0..6 {
        let mut c = d.attach(80, 24);
        assert!(c.next_msg(10_000).is_some(), "attach produced no frame");
        c.send(&ClientMsg::Detach);
        std::thread::sleep(Duration::from_millis(120));
    }
    std::thread::sleep(Duration::from_millis(600));

    // A final client at a LARGER size proves no detached client is still
    // constraining the session minimum.
    //
    // Retried rather than asked once: under parallel stress the daemon can be
    // slow to process the last Detach, and a single fixed sleep turns a
    // correct system into a flaky test.
    let mut last = d.attach(120, 34);
    assert!(last.next_msg(10_000).is_some());
    let deadline = Instant::now() + Duration::from_secs(25);
    let mut seen = String::new();
    while Instant::now() < deadline {
        last.input("echo W=$(tput cols)\r");
        if let Some(screen) = last.wait_for(6, |t| t.contains("W=120")) {
            seen = screen;
            break;
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    assert!(
        seen.contains("W=120"),
        "a detached client was still pinning the viewport size"
    );
}

/// Many splits, then closing them all, must leave exactly one working pane and
/// no orphaned ConPTY.
#[test]
fn many_splits_then_closing_them_all_ends_with_one_working_pane() {
    let d = Daemon::start("zmux-test-manysplits");
    let mut c = d.attach_ready(160, 44);

    // Alternate directions. Five successive HORIZONTAL splits would shrink 160
    // columns past MIN_COLS and the last would be correctly refused - the
    // layout is right and the arithmetic was mine.
    for dir in [
        Cmd::SplitHorizontal,
        Cmd::SplitVertical,
        Cmd::SplitHorizontal,
        Cmd::SplitVertical,
    ] {
        c.cmd(dir);
        std::thread::sleep(Duration::from_millis(350));
    }
    assert!(
        c.wait_for(20, |t| t.contains("5 panes")).is_some(),
        "splits did not all take effect"
    );

    for _ in 0..4 {
        c.cmd(Cmd::ClosePane);
        std::thread::sleep(Duration::from_millis(350));
    }
    assert!(
        c.wait_for(25, |t| t.contains("1 pane |")).is_some(),
        "panes did not all close"
    );

    // The survivor is a live shell, not a corpse.
    c.input("echo SURVIVOR\r");
    assert!(
        c.wait_for(20, |t| t.contains("SURVIVOR")).is_some(),
        "the last pane stopped working after its siblings closed"
    );
}

/// A peer sending garbage must be refused without disturbing the session.
#[test]
fn a_hostile_peer_cannot_disturb_a_healthy_session() {
    let d = Daemon::start("zmux-test-hostile");
    let mut good = d.attach_ready(90, 24);

    // Raw bytes that are not a valid length-prefixed bincode message.
    {
        use std::io::Write;
        let mut bad = ipc::connect(&d.session).expect("connect");
        let _ = bad.write_all(&[0xff, 0xff, 0xff, 0xff]); // oversized length prefix
        let _ = bad.flush();
    }
    // An oversized but well-formed Input must be rejected, not applied.
    //
    // Done on a DETACHED thread and never joined. A peer that handshakes and
    // then refuses to read wedges ITSELF: the daemon fills the pipe with
    // frames it never drains, so its own large write cannot complete. That is
    // self-inflicted and acceptable - what matters, and what this asserts, is
    // that the DAEMON stays healthy. Joining here would hang the test on the
    // bad client's own deadlock.
    let session = d.session.clone();
    std::thread::spawn(move || {
        if let Ok(mut big) = ipc::connect(&session) {
            let _ = ipc::write_msg(
                &mut big,
                &ClientMsg::Hello { version: PROTOCOL_VERSION, cols: 90, rows: 24 },
            );
            let _ = ipc::read_msg::<_, ServerMsg>(&mut big);
            let _ = ipc::write_msg(
                &mut big,
                &ClientMsg::Input { data: vec![b'x'; zmux::protocol::MAX_INPUT_BYTES + 1] },
            );
        }
    });
    std::thread::sleep(Duration::from_millis(1500));

    good.input("echo UNDISTURBED\r");
    assert!(
        good.wait_for(20, |t| t.contains("UNDISTURBED")).is_some(),
        "a hostile peer disrupted a healthy session"
    );
    assert!(ipc::is_running(&d.session), "the daemon died on malformed input");
}
