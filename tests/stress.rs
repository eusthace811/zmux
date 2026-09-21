//! Failure modes the bounds and lifecycle work exist to handle.
//!
//! These are the cases a review flagged as untested: a client that stops
//! reading, heavy output, many panes, and repeated attach/detach.

mod common;
use common::Daemon;

use std::time::{Duration, Instant};

use zmux::ipc;
use zmux::protocol::{ClientMsg, Cmd, ServerMsg, PROTOCOL_VERSION};

/// A client that handshakes and then never reads must not take the daemon or
/// the other clients with it. Its writer thread wedges; nobody else notices.
///
/// NOT what this proves: that the silent peer is dropped. It is not. Its
/// writer blocks in `write_msg` once the pipe buffer fills, a blocking write
/// on a Windows pipe never times out, and the serve loop only gives up when
/// `writer.is_finished()` - which a wedged writer never is. So it stays
/// registered, holding the idle timer open and its viewport in the `min()`
/// that sizes the session. This test asserts containment, which is what the
/// design promises; see the README for the rest.
#[test]
fn a_client_that_stops_reading_is_contained_not_dropped() {
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
    assert!(d.wait_for_panes(5, 20), "splits did not all take effect");

    for _ in 0..4 {
        c.cmd(Cmd::ClosePane);
        std::thread::sleep(Duration::from_millis(350));
    }
    assert!(d.wait_for_panes(1, 25), "panes did not all close");

    // The survivor is a live shell, not a corpse.
    c.input("echo SURVIVOR\r");
    assert!(
        c.wait_for(20, |t| t.contains("SURVIVOR")).is_some(),
        "the last pane stopped working after its siblings closed"
    );
}

/// Adding and removing panes must not break the delta chain either.
///
/// `a_burst_of_commands_does_not_break_the_delta_chain` covers scrolling, and
/// `switching_windows_resends_the_whole_screen` covers a window switch. A
/// SPLIT and a CLOSE go through the same `reflow_locked` - which bumps the
/// revision and clears the delta base so the next publish is a whole frame -
/// but nothing asserted it, and they are the two layout changes a person
/// makes constantly.
///
/// The failure this catches is not a wrong pixel, it is a FROZEN one: a delta
/// derived against a base the client never had is refused, nothing asks for a
/// resync, and the screen stops moving while the session carries on. So the
/// assertion is that output keeps ARRIVING after each change, not that any
/// particular frame was full.
#[test]
fn adding_and_removing_panes_does_not_break_the_delta_chain() {
    let d = Daemon::start("zmux-test-layout-chain");
    let mut c = d.attach_ready(100, 30);

    for round in 0..4 {
        c.cmd(Cmd::SplitVertical);
        assert!(d.wait_for_panes(2, 20), "round {round}: the split never appeared");
        // Focus follows a plain split, so this lands in the NEW pane.
        c.input(&format!("echo SPLIT_{round}\r"));
        assert!(
            c.wait_for(25, |t| t.contains(&format!("SPLIT_{round}"))).is_some(),
            "round {round}: the screen stopped updating after a split"
        );

        c.cmd(Cmd::ClosePane);
        assert!(d.wait_for_panes(1, 20), "round {round}: the pane never closed");
        c.input(&format!("echo CLOSED_{round}\r"));
        assert!(
            c.wait_for(25, |t| t.contains(&format!("CLOSED_{round}"))).is_some(),
            "round {round}: the screen stopped updating after a close"
        );
    }

    // Still tracking live output after eight layout changes, which is what
    // says the chain is intact rather than merely having recovered once.
    c.input("echo CHAIN_STILL_GOOD\r");
    assert!(
        c.wait_for(25, |t| t.contains("CHAIN_STILL_GOOD")).is_some(),
        "the client stopped tracking output after repeated layout changes"
    );
}

/// A pane flooding output with NOBODY ATTACHED must not wedge the daemon.
///
/// Every other test here attaches a client first, which is the wrong shape
/// for the thing this is actually built for: a session driven entirely over
/// control connections has no viewport at all, and the pump takes a different
/// path for it - it drains pane events and skips compositing, because there
/// is nothing to composite for.
///
/// That path has its own backpressure. Each pane's reader hands output to a
/// 256-slot queue and the session forwards it into a 512-slot one; the
/// forwarding send BLOCKS when the pump is behind. If the pump ever stopped
/// draining while panes kept producing, those sends would pile up and the
/// session would stop answering - and with no client attached there would be
/// nothing on screen to show it had.
#[test]
fn a_headless_session_survives_a_pane_that_floods_it() {
    use zmux::protocol::{Query, Source, Target};

    let d = Daemon::start("zmux-test-headless-flood");
    let mut c = common::Control::open(&d.session);
    let id = c.info().panes[0].id;

    // Far more than either queue holds, and more than the scrollback keeps.
    c.input_at(Target::Id(id), "for i in $(seq 1 20000); do echo GUSH_$i; done", true);
    assert!(
        c.wait_at(90, Target::Id(id), |t| t.contains("GUSH_20000")).is_some(),
        "the flood never finished with nobody attached"
    );

    // The daemon still answers, and the pane is still a working shell.
    let info = c.info();
    assert_eq!(info.panes.len(), 1, "the session lost a pane to the flood");
    c.input_at(Target::Id(id), "echo AFTER_THE_FLOOD", true);
    assert!(
        c.wait_at(30, Target::Id(id), |t| t.matches("AFTER_THE_FLOOD").count() >= 2).is_some(),
        "the pane stopped accepting input after flooding a headless session"
    );

    // And a history read still works, which is the thing a caller does next.
    let q = Query::CaptureAt { target: Target::Id(id), lines: 50, source: Source::RecentUnwrapped };
    match c.ask(&ClientMsg::Query(q)) {
        ServerMsg::Capture { text } => {
            assert!(text.contains("AFTER_THE_FLOOD"), "the read missed the newest output")
        }
        other => panic!("capture failed after a flood: {other:?}"),
    }
    assert!(ipc::is_running(&d.session));
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

/// Rapid commands must not break the delta chain.
///
/// Frames are coalesced, so a burst can overwrite a frame before the writer
/// takes it. If the daemon then sent a delta based on that never-delivered
/// frame, the client would refuse it - and keep refusing every delta after,
/// because nothing asks for a resync. Observed exactly that: the client froze
/// at revision 11 while bases climbed past 19.
#[test]
fn a_burst_of_commands_does_not_break_the_delta_chain() {
    let d = Daemon::start("zmux-test-burst");
    let mut c = d.attach_ready(100, 24);

    c.input("for i in $(seq 1 120); do echo BURST_$i; done\r");
    assert!(c.wait_for(30, |t| t.contains("BURST_120")).is_some(), "output never finished");

    // No pauses: this is what makes frames coalesce.
    for _ in 0..12 {
        c.cmd(Cmd::ScrollPageUp);
    }
    assert!(
        c.wait_for(20, |t| t.contains("SCROLL") && t.contains("BURST_5")).is_some(),
        "the client fell behind and never recovered - the delta chain broke"
    );

    // And it still tracks live output afterwards, proving the chain is intact
    // rather than merely having caught up once.
    c.cmd(Cmd::ScrollToBottom);
    c.input("echo CHAIN_OK\r");
    assert!(
        c.wait_for(20, |t| t.contains("CHAIN_OK") && !t.contains("SCROLL")).is_some(),
        "client stopped tracking output after the burst"
    );
}
