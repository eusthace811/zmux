//! The status bar, driven by a real shell.
//!
//! `render::status_spans` is pure and tested at every width, but that only
//! proves the bar formats what it is handed. These tests prove the daemon
//! hands it the truth: a real Git Bash runs a real command and the line
//! changes.

mod common;
use common::Daemon;

/// tmux names a window after what is running in it. So do we - and when
/// nothing is, after the shell.
#[test]
fn the_bar_names_the_window_after_what_is_running() {
    let d = Daemon::start("zmux-test-status-name");
    let mut c = d.attach_ready(100, 30);

    assert!(
        c.wait_for(25, |t| t.contains("| bash")).is_some(),
        "an idle window should be named after its shell"
    );

    c.input("sleep 5\r");
    assert!(
        c.wait_for(25, |t| t.contains("| sleep")).is_some(),
        "the bar never named the running command"
    );

    // And it goes back when the command does.
    assert!(
        c.wait_for(25, |t| t.contains("| bash")).is_some(),
        "the bar kept showing a command that had finished"
    );
}

/// The busy marker is the one thing the narrow rungs keep, so it had better
/// mean something.
#[test]
fn a_running_pane_is_dotted_and_an_idle_one_is_not() {
    let d = Daemon::start("zmux-test-status-busy");
    let mut c = d.attach_ready(100, 30);

    let idle = c.wait_for(25, |t| t.contains("| bash")).expect("no idle bar");
    assert!(!idle.contains('•'), "an idle pane must not be marked busy:\n{idle}");

    c.input("sleep 5\r");
    assert!(
        c.wait_for(25, |t| t.contains('•')).is_some(),
        "a running pane was never marked busy"
    );
}

/// The label follows the FOCUSED pane, so splitting and running something in
/// the new pane renames the window.
#[test]
fn the_label_follows_the_focused_pane() {
    let d = Daemon::start("zmux-test-status-focus");
    let mut c = d.attach_ready(120, 30);
    assert!(c.wait_for(25, |t| t.contains("| bash")).is_some(), "no idle bar");

    c.cmd(zmux::protocol::Cmd::SplitHorizontal);
    assert!(d.wait_for_panes(2, 20), "split did not happen");

    // Focus follows the new pane, so this runs there.
    c.input("sleep 5\r");
    assert!(
        c.wait_for(25, |t| t.contains("| sleep")).is_some(),
        "the window was not named after the focused pane's command"
    );
    // Pane 2 is the focused one and the busy one.
    assert!(
        c.wait_for(25, |t| t.contains("2*•")).is_some(),
        "the focused pane should be the one marked running"
    );
}
