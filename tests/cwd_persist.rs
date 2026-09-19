//! Phase 4: cwd tracking and session persistence.

mod common;
use common::Daemon;

use zmux::protocol::Cmd;

/// The shell must actually report its directory via OSC 7, and a new pane must
/// start there.
///
/// This is the whole reason we launch with `--rcfile` instead of `-l`: a login
/// shell ignores `--rcfile`, so the hook would never load.
#[test]
fn a_new_pane_inherits_the_focused_pane_directory() {
    let d = Daemon::start("zmux-test-cwd");
    let mut c = d.attach_ready(120, 30);

    // Move somewhere unmistakable, then split.
    c.input("cd /c/Windows\r");
    assert!(c.wait_for(20, |t| t.contains("Windows")).is_some(), "cd never echoed");
    // Let the prompt fire so OSC 7 is emitted.
    std::thread::sleep(std::time::Duration::from_millis(800));

    c.cmd(Cmd::SplitHorizontal);
    assert!(c.wait_for(20, |t| t.contains("2 panes")).is_some(), "split did not happen");

    // The new pane is focused; ask it where it is.
    c.input("pwd\r");
    let screen = c
        .wait_for(20, |t| t.matches("/c/Windows").count() >= 1)
        .expect("new pane did not start in the focused pane's directory");
    assert!(screen.contains("/c/Windows"), "screen was:\n{screen}");
}

/// Shell integration must not cost the user their profile. Dropping `-l` means
/// the rcfile performs login startup itself; if that regressed, PATH would be
/// missing the tools a login shell provides.
#[test]
fn the_shell_still_has_a_login_environment() {
    let d = Daemon::start("zmux-test-env");
    let mut c = d.attach_ready(120, 30);
    c.input("command -v git >/dev/null && echo GIT_ON_PATH || echo GIT_MISSING\r");
    let screen = c.wait_for(20, |t| t.contains("GIT_ON_PATH") || t.contains("GIT_MISSING"))
        .expect("no answer");
    assert!(
        screen.contains("GIT_ON_PATH"),
        "login environment was lost when we dropped -l:\n{screen}"
    );
}

#[test]
fn scrollback_shows_history_and_returns_to_live_output() {
    let d = Daemon::start("zmux-test-scroll");
    let mut c = d.attach_ready(100, 24);

    // Fill well past one screen so there is history to find.
    c.input("for i in $(seq 1 120); do echo LINE_$i; done\r");
    assert!(c.wait_for(30, |t| t.contains("LINE_120")).is_some(), "output never completed");

    // Early lines have scrolled off.
    let live = c.current(5).unwrap_or_default();
    assert!(!live.contains("LINE_1 "), "expected LINE_1 to have scrolled away");

    // Scroll back far enough to reach them.
    for _ in 0..12 {
        c.cmd(Cmd::ScrollPageUp);
    }
    let scrolled = c
        .wait_for(20, |t| t.contains("LINE_5") && t.contains("SCROLL"))
        .expect("scrollback never revealed earlier output");
    assert!(scrolled.contains("SCROLL"), "status did not show scroll mode");

    // Returning to the bottom shows live output again.
    c.cmd(Cmd::ScrollToBottom);
    let back = c
        .wait_for(20, |t| t.contains("LINE_120") && !t.contains("SCROLL"))
        .expect("never returned to live output");
    assert!(!back.contains("SCROLL"));
}

/// Typing while scrolled back must snap to the bottom, as a terminal does -
/// otherwise the session looks frozen.
#[test]
fn typing_snaps_back_to_live_output() {
    let d = Daemon::start("zmux-test-snap");
    let mut c = d.attach_ready(100, 24);
    c.input("for i in $(seq 1 80); do echo N_$i; done\r");
    assert!(c.wait_for(30, |t| t.contains("N_80")).is_some());

    for _ in 0..8 {
        c.cmd(Cmd::ScrollPageUp);
    }
    assert!(c.wait_for(20, |t| t.contains("SCROLL")).is_some(), "did not enter scrollback");

    c.input("echo SNAPPED\r");
    let after = c
        .wait_for(20, |t| t.contains("SNAPPED") && !t.contains("SCROLL"))
        .expect("typing did not return to live output");
    assert!(!after.contains("SCROLL"));
}

/// A restarted daemon must rebuild the same arrangement of panes in the same
/// directories. Processes cannot survive; layout and cwd can.
#[test]
fn layout_and_directories_survive_a_daemon_restart() {
    let name = "zmux-test-restart";
    zmux::persist::remove(name);

    {
        let d = Daemon::start(name);
        let mut c = d.attach_ready(120, 30);
        c.input("cd /c/Windows\r");
        assert!(c.wait_for(20, |t| t.contains("Windows")).is_some());
        std::thread::sleep(std::time::Duration::from_millis(800));
        c.cmd(Cmd::SplitHorizontal);
        assert!(c.wait_for(20, |t| t.contains("2 panes")).is_some(), "split failed");
        std::thread::sleep(std::time::Duration::from_millis(800));
        // Daemon is shut down cleanly when `d` drops.
    }
    std::thread::sleep(std::time::Duration::from_millis(600));

    // Fresh daemon, same session name.
    let d2 = Daemon::start(name);
    let mut c2 = d2.attach_ready(120, 30);
    let screen = c2
        .wait_for(20, |t| t.contains("2 panes"))
        .expect("restarted daemon did not restore the split layout");
    assert!(screen.contains('│'), "divider missing after restart:\n{screen}");

    // And the restored pane starts where it left off.
    c2.input("pwd\r");
    let pwd = c2
        .wait_for(20, |t| t.contains("/c/Windows"))
        .expect("restored pane did not start in its saved directory");
    assert!(pwd.contains("/c/Windows"));

    zmux::persist::remove(name);
}
