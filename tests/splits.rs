//! Phase 3: splits. Driven through the real protocol against a real daemon,
//! so these exercise ConPTY, vt100, the layout tree and the compositor
//! together - not just the layout unit tests.

mod common;
use common::{text_of, wait_for, Daemon};

use zmux::ipc;
use zmux::protocol::{Cmd, ClientMsg, ServerMsg, PROTOCOL_VERSION};

type Stream = interprocess::local_socket::Stream;

fn hello(s: &mut Stream, cols: u16, rows: u16) {
    ipc::write_msg(s, &ClientMsg::Hello { version: PROTOCOL_VERSION, cols, rows }).expect("hello");
    match ipc::read_msg::<_, ServerMsg>(s).expect("reply") {
        Some(ServerMsg::Hello { .. }) => {}
        other => panic!("expected Hello, got {other:?}"),
    }
}

fn cmd(s: &mut Stream, c: Cmd) {
    ipc::write_msg(s, &ClientMsg::Command(c)).expect("command");
}

fn attached(d: &Daemon, cols: u16, rows: u16) -> Stream {
    let mut s = ipc::connect(&d.session).expect("connect");
    hello(&mut s, cols, rows);
    assert!(wait_for(&mut s, 20, |t| t.contains('$')).is_some(), "no prompt in first pane");
    s
}

#[test]
fn splitting_gives_two_independent_shells() {
    let d = Daemon::start("zmux-test-split2");
    let mut s = attached(&d, 100, 30);

    cmd(&mut s, Cmd::SplitHorizontal);
    // The status line reports the pane count, so it is a reliable signal that
    // the split actually took effect server-side.
    assert!(
        wait_for(&mut s, 20, |t| t.contains("2 panes")).is_some(),
        "status never reported two panes"
    );

    // Focus follows the new pane; type there.
    ipc::write_msg(&mut s, &ClientMsg::Input { data: b"echo RIGHT_PANE\r".to_vec() }).expect("in");
    assert!(
        wait_for(&mut s, 20, |t| t.contains("RIGHT_PANE")).is_some(),
        "new pane did not run the command"
    );

    // Move back to the original and prove it is a DIFFERENT shell.
    cmd(&mut s, Cmd::FocusLeft);
    ipc::write_msg(&mut s, &ClientMsg::Input { data: b"echo LEFT_PANE\r".to_vec() }).expect("in");
    let screen = wait_for(&mut s, 20, |t| t.contains("LEFT_PANE") && t.contains("RIGHT_PANE"))
        .expect("both panes' output should be on screen at once");

    // A vertical divider must separate them.
    assert!(screen.contains('│'), "no divider drawn; screen was:\n{screen}");
}

#[test]
fn a_divider_separates_stacked_panes() {
    let d = Daemon::start("zmux-test-splitv");
    let mut s = attached(&d, 100, 30);
    cmd(&mut s, Cmd::SplitVertical);
    let screen = wait_for(&mut s, 20, |t| t.contains("2 panes") && t.contains('─'))
        .expect("no horizontal divider after a vertical split");
    assert!(screen.contains('─'));
}

#[test]
fn closing_a_pane_returns_its_space_and_keeps_the_session() {
    let d = Daemon::start("zmux-test-close");
    let mut s = attached(&d, 100, 30);

    cmd(&mut s, Cmd::SplitHorizontal);
    assert!(wait_for(&mut s, 20, |t| t.contains("2 panes")).is_some());

    cmd(&mut s, Cmd::ClosePane);
    let screen = wait_for(&mut s, 20, |t| t.contains("1 pane |"))
        .expect("pane count never returned to one");
    assert!(!screen.contains('│'), "divider still drawn after close:\n{screen}");

    // The surviving shell still works.
    ipc::write_msg(&mut s, &ClientMsg::Input { data: b"echo STILL_ALIVE\r".to_vec() }).expect("in");
    assert!(wait_for(&mut s, 20, |t| t.contains("STILL_ALIVE")).is_some());
}

#[test]
fn closing_the_last_pane_is_refused() {
    let d = Daemon::start("zmux-test-lastpane");
    let mut s = attached(&d, 100, 30);
    cmd(&mut s, Cmd::ClosePane);
    std::thread::sleep(std::time::Duration::from_millis(400));
    assert!(ipc::is_running(&d.session), "closing the only pane killed the session");
}

#[test]
fn each_pane_gets_its_own_pty_size() {
    let d = Daemon::start("zmux-test-panesize");
    let mut s = attached(&d, 100, 30);
    cmd(&mut s, Cmd::SplitHorizontal);
    assert!(wait_for(&mut s, 20, |t| t.contains("2 panes")).is_some());

    // Each pane is about half of 100 columns, so neither should still be 100.
    ipc::write_msg(&mut s, &ClientMsg::Input { data: b"tput cols\r".to_vec() }).expect("in");
    let screen = wait_for(&mut s, 20, |t| t.contains("4") || t.contains("5"))
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
        let mut s = attached(&d, 100, 30);
        cmd(&mut s, Cmd::SplitHorizontal);
        assert!(wait_for(&mut s, 20, |t| t.contains("2 panes")).is_some());
        ipc::write_msg(&mut s, &ClientMsg::Detach).expect("detach");
    }
    std::thread::sleep(std::time::Duration::from_millis(400));

    let mut s2 = ipc::connect(&d.session).expect("reconnect");
    hello(&mut s2, 100, 30);
    let first = match ipc::read_msg::<_, ServerMsg>(&mut s2).expect("frame") {
        Some(ServerMsg::Frame(f)) => text_of(&f),
        other => panic!("expected a Frame on attach, got {other:?}"),
    };
    assert!(first.contains("2 panes"), "layout was lost across detach:\n{first}");
    assert!(first.contains('│'), "divider missing after reattach");
}

#[test]
fn a_too_small_terminal_refuses_to_split() {
    let d = Daemon::start("zmux-test-tiny");
    let mut s = ipc::connect(&d.session).expect("connect");
    hello(&mut s, 12, 6); // below MIN_COLS*2+1
    cmd(&mut s, Cmd::SplitHorizontal);
    std::thread::sleep(std::time::Duration::from_millis(600));
    // Still alive and still one pane: a refused split must not spawn a pty.
    assert!(ipc::is_running(&d.session));
}
