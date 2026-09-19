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

    // Ask for an unambiguous marker rather than a bare number: a prompt or a
    // path contains "4" or "5" too, so the old check could pass before tput
    // had produced anything at all.
    c.input("echo COLS=$(tput cols)\r");
    let screen = c
        .wait_for(20, |t| t.contains("COLS=") && t.matches("COLS=").count() >= 2)
        .expect("pane never reported its width");
    let reported: Vec<u16> = screen
        .match_indices("COLS=")
        .filter_map(|(i, _)| screen[i + 5..].split_whitespace().next())
        .filter_map(|v| v.parse::<u16>().ok())
        .collect();
    assert!(!reported.is_empty(), "no numeric width in:\n{screen}");
    assert!(
        reported.iter().all(|&w| w < 100 && w > 0),
        "pane still believes it is full width; ConPTY was not resized: {reported:?}"
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
    let mut c = d.attach(16, 6); // below MIN_COLS*2+1 = 17
    assert!(c.wait_for(20, |t| !t.trim().is_empty()).is_some(), "no frame arrived");
    c.cmd(Cmd::SplitHorizontal);
    std::thread::sleep(std::time::Duration::from_millis(800));

    // Assert the split was REFUSED, not merely that we survived it. The
    // divider is the width-independent signal: a status line is truncated on a
    // terminal this narrow.
    let screen = c.current(5).unwrap_or_default();
    assert!(
        !screen.contains('│'),
        "a split that cannot fit must be refused, not spawned:\n{screen}"
    );
    assert!(ipc::is_running(&d.session), "the daemon should still be healthy");
}

/// Exiting one pane's shell closes that pane and leaves the session standing.
/// The counterpart to `typing_exit_in_the_only_pane_ends_the_session`.
#[test]
fn a_shell_exiting_closes_only_its_own_pane() {
    let d = Daemon::start("zmux-test-exit-one");
    let mut c = d.attach_ready(120, 30);
    c.cmd(Cmd::SplitHorizontal);
    assert!(c.wait_for(25, |t| t.contains("2 panes")).is_some(), "split failed");
    // Focus follows the new pane, so `exit` lands in it.
    assert!(c.wait_for(20, |t| t.contains('$')).is_some(), "no prompt in the new pane");

    c.input("exit\r");
    let screen = c
        .wait_for(25, |t| t.contains("1 pane"))
        .expect("the exited pane was never reclaimed");
    assert!(!screen.contains('│'), "the survivor should reclaim the divider:\n{screen}");
    assert!(ipc::is_running(&d.session), "one shell exiting must not end the session");
}
