//! Phase 3: splits. Driven through the real protocol against a real daemon,
//! so these exercise ConPTY, vt100, the layout tree and the compositor
//! together - not just the layout unit tests.

mod common;
use common::Daemon;

use zmux::ipc;
use zmux::protocol::{ClientMsg, Cmd, ServerMsg};

#[test]
fn splitting_gives_two_independent_shells() {
    let d = Daemon::start("zmux-test-split2");
    let mut c = d.attach_ready(100, 30);

    c.cmd(Cmd::SplitHorizontal);
    // The status line reports the pane count, so it is a reliable signal that
    // the split actually took effect server-side.
    assert!(
        c.wait_for(20, |t| t.contains("2 panes")).is_some(),
        "status never reported two panes"
    );

    // Focus follows the new pane; type there.
    c.input("echo RIGHT_PANE\r");
    assert!(
        c.wait_for(20, |t| t.contains("RIGHT_PANE")).is_some(),
        "new pane did not run the command"
    );

    // Move back to the original and prove it is a DIFFERENT shell.
    c.cmd(Cmd::FocusLeft);
    c.input("echo LEFT_PANE\r");
    let screen = c.wait_for(20, |t| t.contains("LEFT_PANE") && t.contains("RIGHT_PANE"))
        .expect("both panes' output should be on screen at once");

    // A vertical divider must separate them.
    assert!(screen.contains('│'), "no divider drawn; screen was:\n{screen}");
}

#[test]
fn a_divider_separates_stacked_panes() {
    let d = Daemon::start("zmux-test-splitv");
    let mut c = d.attach_ready(100, 30);
    c.cmd(Cmd::SplitVertical);
    let screen = c.wait_for(20, |t| t.contains("2 panes") && t.contains('─'))
        .expect("no horizontal divider after a vertical split");
    assert!(screen.contains('─'));
}

#[test]
fn closing_a_pane_returns_its_space_and_keeps_the_session() {
    let d = Daemon::start("zmux-test-close");
    let mut c = d.attach_ready(100, 30);

    c.cmd(Cmd::SplitHorizontal);
    assert!(c.wait_for(20, |t| t.contains("2 panes")).is_some());

    c.cmd(Cmd::ClosePane);
    let screen = c.wait_for(20, |t| t.contains("1 pane |"))
        .expect("pane count never returned to one");
    assert!(!screen.contains('│'), "divider still drawn after close:\n{screen}");

    // The surviving shell still works.
    c.input("echo STILL_ALIVE\r");
    assert!(c.wait_for(20, |t| t.contains("STILL_ALIVE")).is_some());
}

#[test]
fn closing_the_last_pane_is_refused() {
    let d = Daemon::start("zmux-test-lastpane");
    let mut c = d.attach_ready(100, 30);
    c.cmd(Cmd::ClosePane);
    std::thread::sleep(std::time::Duration::from_millis(400));
    assert!(ipc::is_running(&d.session), "closing the only pane killed the session");
}

#[test]
fn each_pane_gets_its_own_pty_size() {
    let d = Daemon::start("zmux-test-panesize");
    let mut c = d.attach_ready(100, 30);
    c.cmd(Cmd::SplitHorizontal);
    assert!(c.wait_for(20, |t| t.contains("2 panes")).is_some());

    // Each pane is about half of 100 columns, so neither should still be 100.
    c.input("tput cols\r");
    let screen = c.wait_for(20, |t| t.contains("4") || t.contains("5"))
        .expect("pane never reported its width");
    assert!(
        !screen.contains("100"),
        "pane still believes it is full width; ConPTY was not resized:\n{screen}"
    );
}

#[test]
fn splits_survive_detach_and_reattach() {
    let d = Daemon::start("zmux-test-splitpersist");
    {
        let mut c = d.attach_ready(100, 30);
        c.cmd(Cmd::SplitHorizontal);
        assert!(c.wait_for(20, |t| t.contains("2 panes")).is_some());
        c.send(&ClientMsg::Detach);
    }
    std::thread::sleep(std::time::Duration::from_millis(400));

    let mut c2 = d.attach(100, 30);
    let first = match c2.next_msg(10_000) {
        Some(ServerMsg::Frame(f)) => common::text_of(&f),
        other => panic!("expected a Frame on attach, got {other:?}"),
    };
    assert!(first.contains("2 panes"), "layout was lost across detach:\n{first}");
    assert!(first.contains('│'), "divider missing after reattach");
}

#[test]
fn a_too_small_terminal_refuses_to_split() {
    let d = Daemon::start("zmux-test-tiny");
    let mut c = d.attach(12, 6); // below MIN_COLS*2+1
    c.cmd(Cmd::SplitHorizontal);
    std::thread::sleep(std::time::Duration::from_millis(600));
    // Still alive and still one pane: a refused split must not spawn a pty.
    assert!(ipc::is_running(&d.session));
}
