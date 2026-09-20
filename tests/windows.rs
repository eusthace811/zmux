//! Windows: several independent screenfuls of panes in one session.

mod common;

use common::Daemon;
use zmux::protocol::{Cmd, ServerMsg};

/// Type a command whose OUTPUT is `mark` but whose ECHOED TEXT is not.
///
/// `echo W0MARK` puts "W0MARK" on screen twice - once as the command line the
/// shell echoes, once as its output - so asserting on it cannot tell "the
/// command ran" from "the command was typed". Splitting the literal with ''
/// makes the two distinguishable. An earlier test in this suite matched an
/// echoed command line and passed for the wrong reason.
fn echo_mark(c: &mut common::Client, mark: &str) {
    let (head, tail) = mark.split_at(2);
    c.input(&format!("echo {head}''{tail}\r"));
}

#[test]
fn creating_a_window_gives_a_second_screen_with_its_own_shell() {
    let d = Daemon::start("zmux-test-win-new");
    let mut c = d.attach_ready(120, 30);
    assert!(
        c.wait_for(10, |t| t.contains("1 pane")).is_some(),
        "a fresh session should report one pane"
    );

    c.cmd(Cmd::NewWindow);
    let screen = c
        .wait_for(25, |t| t.contains("0 1*"))
        .expect("the status bar should list two windows with the second current");
    assert!(
        screen.contains("1 pane"),
        "a new window starts with exactly one pane:\n{screen}"
    );

    // And it is a real shell, not an empty grid.
    echo_mark(&mut c, "NEWWINDOWALIVE");
    assert!(
        c.wait_for(25, |t| t.contains("NEWWINDOWALIVE")).is_some(),
        "the new window's shell never responded"
    );
}

/// The whole point of windows: what is not on screen keeps running.
#[test]
fn a_background_window_keeps_its_output() {
    let d = Daemon::start("zmux-test-win-switch");
    let mut c = d.attach_ready(120, 30);

    echo_mark(&mut c, "ZEROMARK");
    assert!(c.wait_for(25, |t| t.contains("ZEROMARK")).is_some(), "window 0 never ran anything");

    c.cmd(Cmd::NewWindow);
    assert!(c.wait_for(25, |t| t.contains("0 1*")).is_some(), "second window never appeared");
    let screen = c.current(3).expect("a screen");
    assert!(
        !screen.contains("ZEROMARK"),
        "window 1 is showing window 0's output:\n{screen}"
    );

    echo_mark(&mut c, "ONEMARK");
    assert!(c.wait_for(25, |t| t.contains("ONEMARK")).is_some(), "window 1 never ran anything");

    // Back to window 0: its output is still there, untouched.
    c.cmd(Cmd::PrevWindow);
    let back = c
        .wait_for(25, |t| t.contains("ZEROMARK"))
        .expect("window 0 lost its output while it was in the background");
    assert!(
        !back.contains("ONEMARK"),
        "window 0 is showing window 1's output:\n{back}"
    );
    assert!(back.contains("0* 1"), "the status bar should mark window 0 as current:\n{back}");
}

#[test]
fn next_and_previous_wrap_around() {
    let d = Daemon::start("zmux-test-win-cycle");
    let mut c = d.attach_ready(120, 30);
    c.cmd(Cmd::NewWindow);
    assert!(c.wait_for(25, |t| t.contains("0 1*")).is_some(), "no second window");
    c.cmd(Cmd::NewWindow);
    assert!(c.wait_for(25, |t| t.contains("0 1 2*")).is_some(), "no third window");

    c.cmd(Cmd::NextWindow);
    assert!(
        c.wait_for(20, |t| t.contains("0* 1 2")).is_some(),
        "next from the last window should wrap to the first"
    );
    c.cmd(Cmd::PrevWindow);
    assert!(
        c.wait_for(20, |t| t.contains("0 1 2*")).is_some(),
        "previous from the first window should wrap to the last"
    );
}

#[test]
fn splits_belong_to_one_window_only() {
    let d = Daemon::start("zmux-test-win-splits");
    let mut c = d.attach_ready(120, 30);
    c.cmd(Cmd::NewWindow);
    assert!(c.wait_for(25, |t| t.contains("0 1*")).is_some(), "no second window");

    c.cmd(Cmd::SplitHorizontal);
    assert!(
        c.wait_for(25, |t| t.contains("2 panes")).is_some(),
        "the split did not happen in window 1"
    );

    c.cmd(Cmd::PrevWindow);
    let screen = c
        .wait_for(20, |t| t.contains("0* 1"))
        .expect("did not switch back to window 0");
    assert!(
        screen.contains("1 pane") && !screen.contains("2 panes"),
        "window 0 inherited window 1's split:\n{screen}"
    );
    assert!(!screen.contains('│'), "window 0 should have no divider:\n{screen}");
}

/// tmux closes a window when its last pane goes. Without that, a window
/// created by mistake could only be dismissed by exiting its shell.
#[test]
fn closing_the_last_pane_of_a_window_closes_the_window() {
    let d = Daemon::start("zmux-test-win-close");
    let mut c = d.attach_ready(120, 30);
    echo_mark(&mut c, "SURVIVOR");
    assert!(c.wait_for(25, |t| t.contains("SURVIVOR")).is_some(), "window 0 never ran anything");

    c.cmd(Cmd::NewWindow);
    assert!(c.wait_for(25, |t| t.contains("0 1*")).is_some(), "no second window");

    c.cmd(Cmd::ClosePane);
    let back = c
        .wait_for(25, |t| t.contains("SURVIVOR"))
        .expect("closing window 1's only pane should land us back in window 0");
    assert!(
        !back.contains("0 1"),
        "the window list should be gone with only one window left:\n{back}"
    );
}

/// A shell exiting in a window you are NOT looking at must close that window
/// without disturbing the one you are.
#[test]
fn a_shell_exiting_in_a_background_window_does_not_move_you() {
    let d = Daemon::start("zmux-test-win-bgexit");
    let mut c = d.attach_ready(120, 30);
    echo_mark(&mut c, "STAYPUT");
    assert!(c.wait_for(25, |t| t.contains("STAYPUT")).is_some(), "window 0 never ran anything");

    c.cmd(Cmd::NewWindow);
    assert!(c.wait_for(25, |t| t.contains("0 1*")).is_some(), "no second window");
    // Wait for window 1's shell to actually be up. The window list appears as
    // soon as the window exists, which is BEFORE bash is reading input, and a
    // command typed into a shell that has not started yet simply vanishes.
    echo_mark(&mut c, "BGREADY");
    assert!(c.wait_for(25, |t| t.contains("BGREADY")).is_some(), "window 1's shell never started");

    // Arm window 1's shell to exit shortly, then leave. The exit therefore
    // happens while we are somewhere else, which is the case under test -
    // exiting the shell you are watching is an ordinary close.
    c.input("sleep 3; exit\r");
    c.cmd(Cmd::PrevWindow);
    let on_zero = c
        .wait_for(20, |t| t.contains("0* 1"))
        .expect("did not switch back to window 0");
    assert!(on_zero.contains("STAYPUT"), "window 0 lost its output:\n{on_zero}");

    // Window 1 dies in the background. We must still be on window 0, with its
    // output intact, and the window list must be gone with one window left.
    let after = c
        .wait_for(25, |t| !t.contains("0*"))
        .expect("the background window's shell exited but its window stayed");
    assert!(
        after.contains("STAYPUT"),
        "a background window closing disturbed the window we were on:\n{after}"
    );
    assert!(after.contains("1 pane"), "should be back to a single window:\n{after}");
}

#[test]
fn windows_survive_detach_and_reattach() {
    let d = Daemon::start("zmux-test-win-detach");
    {
        let mut c = d.attach_ready(120, 30);
        echo_mark(&mut c, "BEFOREDETACH");
        assert!(c.wait_for(25, |t| t.contains("BEFOREDETACH")).is_some(), "nothing ran");
        c.cmd(Cmd::NewWindow);
        assert!(c.wait_for(25, |t| t.contains("0 1*")).is_some(), "no second window");
        // Dropping the client detaches without touching the session.
    }
    std::thread::sleep(std::time::Duration::from_millis(400));

    let mut c2 = d.attach_ready(120, 30);
    let screen = c2
        .wait_for(20, |t| t.contains("0 1*"))
        .expect("both windows should still be there, still on the second");
    assert!(
        !screen.contains("BEFOREDETACH"),
        "reattached onto the wrong window:\n{screen}"
    );

    c2.cmd(Cmd::PrevWindow);
    assert!(
        c2.wait_for(20, |t| t.contains("BEFOREDETACH")).is_some(),
        "window 0 lost its output across a detach"
    );
}

/// Processes cannot survive a daemon restart; the window ARRANGEMENT can.
#[test]
fn windows_survive_a_daemon_restart() {
    let name = "zmux-test-win-restart";
    zmux::persist::remove(name);
    {
        let d = Daemon::start_preserving_state(name);
        let mut c = d.attach_ready(120, 30);
        c.cmd(Cmd::NewWindow);
        assert!(c.wait_for(25, |t| t.contains("0 1*")).is_some(), "no second window");
        c.cmd(Cmd::SplitHorizontal);
        assert!(c.wait_for(25, |t| t.contains("2 panes")).is_some(), "no split in window 1");
        std::thread::sleep(std::time::Duration::from_millis(800));
    }
    std::thread::sleep(std::time::Duration::from_millis(600));

    let d2 = Daemon::start_preserving_state(name);
    let mut c2 = d2.attach_ready(120, 30);
    assert!(
        c2.wait_for(25, |t| t.contains("0 1*") && t.contains("2 panes")).is_some(),
        "the restarted daemon did not restore both windows with window 1 split"
    );

    // The background window must have been rebuilt too, not left empty.
    //
    // Both conditions in ONE wait: the status bar updates the moment the
    // window switches, but the restored shell has not necessarily drawn its
    // prompt by then. Asserting on the frame that first showed the switch
    // fails under load and passes when idle.
    c2.cmd(Cmd::PrevWindow);
    assert!(
        c2.wait_for(25, |t| t.contains("0* 1") && t.contains('$')).is_some(),
        "window 0 was restored without a working shell"
    );

    zmux::persist::remove(name);
}

#[test]
fn a_new_window_inherits_the_focused_directory() {
    let d = Daemon::start("zmux-test-win-cwd");
    let mut c = d.attach_ready(120, 30);
    c.input("cd /c/Windows\r");
    assert!(c.wait_for(20, |t| t.contains("Windows")).is_some(), "cd did not take");
    // OSC 7 arrives with the next prompt; give the daemon a moment to see it.
    std::thread::sleep(std::time::Duration::from_millis(800));

    c.cmd(Cmd::NewWindow);
    assert!(c.wait_for(25, |t| t.contains("0 1*")).is_some(), "no second window");
    c.input("pwd\r");
    assert!(
        c.wait_for(25, |t| t.contains("/c/Windows")).is_some(),
        "a new window should start in the focused pane's directory, as a split does"
    );
}

/// A window switch must resend the WHOLE screen.
///
/// Every cell changes, so a row diff against the screen the client was showing
/// describes nothing. Worse, the client would refuse it and keep refusing -
/// nothing asks for a resync - and the display would freeze at the last
/// revision it managed to apply. `switch_window` gets this from
/// `reflow_locked`, which clears `last_published`; this asserts it directly
/// rather than trusting that the call stays.
#[test]
fn switching_windows_resends_the_whole_screen() {
    let d = Daemon::start("zmux-test-win-fullframe");
    let mut c = d.attach_ready(120, 30);
    c.cmd(Cmd::NewWindow);
    assert!(c.wait_for(25, |t| t.contains("0 1*")).is_some(), "no second window");

    // Let both shells go quiet, so the only thing that can produce a message
    // is the switch itself.
    let _ = c.current(3);
    std::thread::sleep(std::time::Duration::from_millis(800));
    let _ = c.current(1);

    c.cmd(Cmd::PrevWindow);
    let mut first = None;
    for _ in 0..40 {
        match c.next_msg(500) {
            Some(ServerMsg::Frame(_)) => {
                first = Some("frame");
                break;
            }
            Some(ServerMsg::Delta(_)) => {
                first = Some("delta");
                break;
            }
            Some(_) => {}
            None => break,
        }
    }
    assert_eq!(first, Some("frame"), "a window switch must send a full frame, not a delta");
}
