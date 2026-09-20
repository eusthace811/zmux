//! The control connection: what a script or a coding agent drives the session
//! through. Every test here goes over the real pipe to a real daemon, because
//! the whole point of the surface is that it works from outside the client.

mod common;
use common::{Control, Daemon};

use zmux::protocol::{ClientMsg, Cmd, Query, ServerMsg};

#[test]
fn info_reports_the_layout_a_command_produced() {
    let d = Daemon::start("zmux-test-ctl-info");
    let mut c = Control::open(&d.session);

    let before = c.info();
    assert_eq!(before.panes.len(), 1, "a new session has one pane");
    assert_eq!(before.focus, 1);
    assert_eq!(before.windows, 1);
    assert_eq!(before.window, 0);

    // A mutation answers with the resulting state, so one round trip is
    // enough to learn where the new pane landed.
    let reply = c.ask(&ClientMsg::Command(Cmd::SplitHorizontal));
    let ServerMsg::Info(after) = reply else { panic!("expected Info, got {reply:?}") };
    assert_eq!(after.panes.len(), 2, "split did not report two panes");
    assert_eq!(after.focus, 2, "focus follows the new pane, as C-b % does");

    // Geometry is real: two side-by-side panes are each narrower than the
    // session, and together they account for it bar the divider column.
    let w: u16 = after.panes.iter().map(|p| p.cols).sum();
    assert!(
        after.panes.iter().all(|p| p.cols < after.cols),
        "panes are not actually narrower than the session: {after:?}"
    );
    assert_eq!(w + 1, after.cols, "two panes plus one divider should fill the width");
}

#[test]
fn a_control_connection_is_not_an_attached_client() {
    // This is the reason control is a separate handshake. A control peer has
    // no viewport; if it were registered as a client the session would shrink
    // to whatever size it claimed - or to the 80x24 default - and the user's
    // screen would visibly jump.
    let d = Daemon::start("zmux-test-ctl-size");
    let mut attached = d.attach_ready(120, 40);
    attached.input("echo ready\r");
    assert!(attached.wait_for(20, |t| t.contains("ready")).is_some(), "pane never answered");

    let mut c = Control::open(&d.session);
    let info = c.info();
    assert_eq!(
        (info.cols, info.rows),
        (120, 40),
        "opening a control connection changed the session size"
    );

    // And it stays that way after the control peer issues commands.
    c.ask(&ClientMsg::Query(Query::Info));
    let info = c.info();
    assert_eq!((info.cols, info.rows), (120, 40));
}

#[test]
fn a_command_runs_in_a_pane_and_its_output_comes_back() {
    // The loop an agent actually needs: make a pane, run something in it,
    // read the answer - without attaching a terminal.
    let d = Daemon::start("zmux-test-ctl-run");
    let mut c = Control::open(&d.session);

    let ServerMsg::Info(after) = c.ask(&ClientMsg::Command(Cmd::SplitVertical)) else {
        panic!("split failed")
    };
    let new_pane = after.focus as u8;

    c.ask(&ClientMsg::InputTo {
        pane: new_pane,
        data: b"echo CONTROL_MARKER_OK\r".to_vec(),
    });

    let text = c
        .wait_for(20, Some(new_pane), |t| t.matches("CONTROL_MARKER_OK").count() >= 2)
        .unwrap_or_else(|| panic!("the pane never produced the output"));
    // Twice: the shell's echo of the command line, then the command's output.
    assert!(text.matches("CONTROL_MARKER_OK").count() >= 2, "captured:\n{text}");
}

#[test]
fn writing_to_a_pane_does_not_move_focus() {
    // Background work must stay in the background: a caller typing into pane 1
    // while the user works in pane 2 must not drag the focus - and therefore
    // the user's keystrokes - across the screen.
    let d = Daemon::start("zmux-test-ctl-focus");
    let mut c = Control::open(&d.session);
    c.ask(&ClientMsg::Command(Cmd::SplitHorizontal));
    assert_eq!(c.info().focus, 2);

    c.ask(&ClientMsg::InputTo { pane: 1, data: b"echo IN_PANE_ONE\r".to_vec() });
    assert_eq!(c.info().focus, 2, "writing to pane 1 stole the focus");

    // It really did land in pane 1, not the focused one.
    assert!(
        c.wait_for(20, Some(1), |t| t.contains("IN_PANE_ONE")).is_some(),
        "the text did not reach pane 1"
    );
    let two = c.capture(Some(2));
    assert!(!two.contains("IN_PANE_ONE"), "it also went to pane 2:\n{two}");
}

#[test]
fn captures_are_per_pane() {
    let d = Daemon::start("zmux-test-ctl-capture");
    let mut c = Control::open(&d.session);
    c.ask(&ClientMsg::Command(Cmd::SplitHorizontal));

    c.ask(&ClientMsg::InputTo { pane: 1, data: b"echo LEFT_SIDE\r".to_vec() });
    c.ask(&ClientMsg::InputTo { pane: 2, data: b"echo RIGHT_SIDE\r".to_vec() });

    assert!(c.wait_for(20, Some(1), |t| t.contains("LEFT_SIDE")).is_some(), "pane 1 silent");
    assert!(c.wait_for(20, Some(2), |t| t.contains("RIGHT_SIDE")).is_some(), "pane 2 silent");

    let one = c.capture(Some(1));
    let two = c.capture(Some(2));
    assert!(!one.contains("RIGHT_SIDE"), "pane 1 shows pane 2's output:\n{one}");
    assert!(!two.contains("LEFT_SIDE"), "pane 2 shows pane 1's output:\n{two}");
}

#[test]
fn a_pane_that_does_not_exist_is_refused_not_guessed() {
    let d = Daemon::start("zmux-test-ctl-nopane");
    let mut c = Control::open(&d.session);

    // One pane exists. Asking for the seventh must not fall back to it: a
    // caller that believes it wrote to pane 7 would otherwise corrupt pane 1.
    let reply = c.ask(&ClientMsg::InputTo { pane: 7, data: b"echo NOWHERE\r".to_vec() });
    assert!(matches!(reply, ServerMsg::Refused { .. }), "expected Refused, got {reply:?}");

    let reply = c.ask(&ClientMsg::Query(Query::Capture { pane: Some(7), lines: 0 }));
    assert!(matches!(reply, ServerMsg::Refused { .. }), "expected Refused, got {reply:?}");

    let one = c.capture(Some(1));
    assert!(!one.contains("NOWHERE"), "the refused write landed in pane 1 anyway:\n{one}");
}

#[test]
fn windows_can_be_driven_from_outside() {
    let d = Daemon::start("zmux-test-ctl-windows");
    let mut c = Control::open(&d.session);

    c.ask(&ClientMsg::Command(Cmd::NewWindow));
    c.ask(&ClientMsg::Command(Cmd::NewWindow));
    let info = c.info();
    assert_eq!(info.windows, 3);
    assert_eq!(info.window, 2, "a new window is the current one");

    let ServerMsg::Info(info) = c.ask(&ClientMsg::Command(Cmd::SelectWindow(0))) else {
        panic!("select-window failed")
    };
    assert_eq!(info.window, 0, "select-window did not switch");
    assert_eq!(info.windows, 3, "switching windows changed how many there are");
}

#[test]
fn capture_can_be_limited_to_the_last_lines() {
    let d = Daemon::start("zmux-test-ctl-lines");
    let mut c = Control::open(&d.session);
    c.ask(&ClientMsg::Input { data: b"echo TAIL_MARKER\r".to_vec() });
    assert!(c.wait_for(20, None, |t| t.contains("TAIL_MARKER")).is_some(), "pane silent");

    let all = c.capture(None);
    let ServerMsg::Capture { text: tail } =
        c.ask(&ClientMsg::Query(Query::Capture { pane: None, lines: 2 }))
    else {
        panic!("capture failed")
    };
    assert!(tail.lines().count() <= 2, "asked for 2 lines, got {}", tail.lines().count());
    assert!(all.lines().count() >= tail.lines().count());
    assert!(all.ends_with(&tail), "the tail is not the end of the screen:\n{all}\n---\n{tail}");
}

/// Pipe names are sanitised lossily, so `a/b` and `a_b` reach ONE daemon.
/// The interactive client has always refused that mismatch; a control caller
/// that did not would run its command in somebody else's session, and nothing
/// on either side would say so.
///
/// Drives the real `zmux.exe`, because the check lives in the client.
#[test]
fn a_control_command_refuses_a_session_whose_name_only_collides() {
    let d = Daemon::start("zmux-test-collide_x");
    let zmux = env!("CARGO_BIN_EXE_zmux");

    // The name that really is served: this must work.
    let ok = std::process::Command::new(zmux)
        .args(["info", &d.session])
        .output()
        .expect("run zmux info");
    assert!(ok.status.success(), "info failed: {}", String::from_utf8_lossy(&ok.stderr));
    assert!(String::from_utf8_lossy(&ok.stdout).contains(&d.session));

    // A different name that sanitises to the same pipe: this must not.
    let collided = d.session.replace('_', "/");
    assert_ne!(collided, d.session, "the test needs two spellings of one pipe name");
    let bad = std::process::Command::new(zmux)
        .args(["info", &collided])
        .output()
        .expect("run zmux info");
    assert!(!bad.status.success(), "a colliding name was accepted: {bad:?}");
    let err = String::from_utf8_lossy(&bad.stderr);
    assert!(err.contains("collide"), "unhelpful error: {err}");

    // `kill` most of all: being wrong there ends every shell in somebody
    // else's session, and `Shutdown` is acted on before any handshake, so it
    // has to ask the name first.
    let bad = std::process::Command::new(zmux)
        .args(["kill", &collided])
        .output()
        .expect("run zmux kill");
    assert!(!bad.status.success(), "kill accepted a colliding name: {bad:?}");
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("collide"),
        "unhelpful error: {}",
        String::from_utf8_lossy(&bad.stderr)
    );
    assert!(
        zmux::ipc::is_running(&d.session),
        "the session was killed by a name that only collides with it"
    );
}

/// A command that could not be carried out must SAY so. An attached human
/// sees that nothing happened; a script cannot, and one told its split
/// succeeded goes on to type into a pane that was never created - the same
/// wrong-shell accident an out-of-range `-p` is refused to avoid.
#[test]
fn a_command_that_cannot_be_carried_out_is_refused_not_reported_as_done() {
    let d = Daemon::start("zmux-test-ctl-refused");
    let mut c = Control::open(&d.session);

    // The session's last pane has nowhere to go.
    let reply = c.ask(&ClientMsg::Command(Cmd::ClosePane));
    let ServerMsg::Refused { reason } = reply else { panic!("expected Refused, got {reply:?}") };
    assert!(reason.contains("last pane"), "unhelpful reason: {reason}");
    assert_eq!(c.info().panes.len(), 1, "the pane went anyway");

    // A viewport too narrow for two panes cannot be divided. Attaching a
    // client is what sets the session size; 16 columns is below MIN_COLS*2+1.
    let mut tiny = d.attach(16, 6);
    assert!(tiny.wait_for(20, |t| !t.trim().is_empty()).is_some(), "no frame arrived");

    let reply = c.ask(&ClientMsg::Command(Cmd::SplitHorizontal));
    let ServerMsg::Refused { reason } = reply else {
        panic!("a split that cannot fit was reported as done: {reply:?}")
    };
    assert!(reason.contains("split refused"), "unhelpful reason: {reason}");
    assert_eq!(c.info().panes.len(), 1, "a pane was created anyway");
}

/// Writing to a pane snaps it back to live output, as any terminal does, even
/// when the write produces no output of its own to push it there - and
/// `zmux keys <session> ""` sends exactly that.
///
/// Asserted through `capture`, which reads the daemon's own view of the pane,
/// so it does not depend on a frame happening to be delivered.
#[test]
fn an_input_that_echoes_nothing_still_snaps_the_pane_back_to_live_output() {
    let d = Daemon::start("zmux-test-ctl-unscroll");
    let mut c = Control::open(&d.session);

    // Wait for the SHELL's answer, not the echoed command line: "60" is in
    // the echo of `seq 1 60` immediately, and scrolling a screen that has not
    // filled yet finds no history to scroll into. `$(( ))` is not expanded in
    // the echo, so FILLED_60 can only have come from the shell.
    c.ask(&ClientMsg::Input { data: b"seq 1 60; echo FILLED_$((30+30))\r".to_vec() });
    assert!(c.wait_for(20, None, |t| t.contains("FILLED_60")).is_some(), "the pane never filled");

    c.ask(&ClientMsg::Command(Cmd::ScrollPageUp));
    let history = c.capture(None);
    assert!(
        !history.contains("FILLED_60"),
        "the pane did not scroll into its history at all:\n{history}"
    );

    // An input with no bytes in it: nothing is typed, but the view must come
    // back to the bottom all the same.
    c.ask(&ClientMsg::Input { data: Vec::new() });
    let live = c.capture(None);
    assert!(
        live.contains("FILLED_60"),
        "writing to the pane left it showing history it had already left:\n{live}"
    );
}

/// The whole "what is running" pipeline, end to end through a real ConPTY:
/// our rcfile emits OSC 133/633, the daemon parses them, and `info` says so.
///
/// Each piece is unit-tested, but only this proves they are wired together -
/// and that the sequences survive a real Git Bash rather than the hand-written
/// approximation they were designed against.
#[test]
fn info_reports_what_a_pane_is_running() {
    let d = Daemon::start("zmux-test-ctl-busy");
    let mut c = Control::open(&d.session);

    // Wait for the shell to reach its first prompt, which is the first time
    // the integration reports anything at all.
    assert!(
        c.wait_for(25, None, |t| t.contains('$')).is_some(),
        "no prompt, so the rcfile never ran"
    );

    // Something slow enough to still be running when we look.
    c.ask(&ClientMsg::Input { data: b"sleep 4; echo SLEPT\r".to_vec() });

    let mut seen: Option<zmux::protocol::PaneInfo> = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        let p = c.info().panes.into_iter().next().expect("a pane");
        if p.busy {
            seen = Some(p);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let busy = seen.expect("the pane never reported itself busy");
    assert_eq!(
        busy.command.as_deref(),
        Some("sleep 4; echo SLEPT"),
        "the WHOLE command line, not just the first of its parts"
    );

    // And it goes quiet again, carrying the exit status.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
    loop {
        let p = c.info().panes.into_iter().next().expect("a pane");
        if !p.busy {
            assert_eq!(p.last_exit, Some(0), "the finished command's status");
            break;
        }
        assert!(std::time::Instant::now() < deadline, "the pane never went idle again");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// A failing command is reported as having failed, which is the thing tmux
/// cannot tell you and the reason the exit status is on the wire at all.
#[test]
fn a_failed_command_reports_its_status() {
    let d = Daemon::start("zmux-test-ctl-exit");
    let mut c = Control::open(&d.session);
    assert!(c.wait_for(25, None, |t| t.contains('$')).is_some(), "no prompt");

    c.ask(&ClientMsg::Input { data: b"(exit 3)\r".to_vec() });
    assert!(
        c.wait_for(25, None, |t| t.contains("exit 3")).is_some(),
        "the command never echoed"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let p = c.info().panes.into_iter().next().expect("a pane");
        if p.last_exit == Some(3) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "never saw status 3, last was {:?} (busy={})",
            p.last_exit,
            p.busy
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
