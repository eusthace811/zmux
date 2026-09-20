//! Driving one pane from another: identity, addressing, and the things a
//! background caller must never do to the user's screen (DESIGN.md §12).

mod common;
use common::{Control, Daemon};

use zmux::protocol::{ClientMsg, Cmd, Query, ServerMsg, Target};

/// A pane has to be able to say which pane it is, or `-p current` and every
/// self-report are guesswork. The id, not the position - positions renumber.
#[test]
fn a_pane_knows_which_pane_it_is() {
    let d = Daemon::start("zmux-test-pane-env");
    let mut c = Control::open(&d.session);
    let id = c.info().panes[0].id;

    c.input_at(Target::Focused, "echo PANE:[$ZMUX_PANE]", true);
    let screen = c
        .wait_at(20, Target::Focused, |t| t.contains(&format!("PANE:[{id}]")))
        .unwrap_or_else(|| panic!("the pane never reported its own id (expected {id})"));
    assert!(screen.contains(&format!("PANE:[{id}]")), "screen was:\n{screen}");
}

/// Two panes must get DIFFERENT ids. One variable set on the daemon would
/// reach both with the same value, which is the bug this guards.
#[test]
fn each_pane_gets_its_own_id() {
    let d = Daemon::start("zmux-test-pane-env-split");
    let mut c = Control::open(&d.session);
    assert!(matches!(c.ask(&ClientMsg::Command(Cmd::SplitVertical)), ServerMsg::Info(_)));
    assert!(d.wait_for_panes(2, 10), "the split never appeared");

    let info = c.info();
    let ids: Vec<u32> = info.panes.iter().map(|p| p.id).collect();
    assert_eq!(ids.len(), 2);

    let mut seen = Vec::new();
    for p in &info.panes {
        let t = Target::Id(p.id);
        c.input_at(t.clone(), "echo MINE:[$ZMUX_PANE]", true);
        // Twice: the shell echoes the command line unexpanded, so the FIRST
        // match is the literal `$ZMUX_PANE` the caller typed. Only the last
        // one is the shell's answer.
        let screen = c
            .wait_at(20, t, |s| s.matches("MINE:[").count() >= 2)
            .unwrap_or_else(|| panic!("pane @{} never answered", p.id));
        let reported = screen
            .rsplit("MINE:[")
            .next()
            .and_then(|s| s.split(']').next())
            .map(str::to_string)
            .unwrap_or_default();
        seen.push(reported);
    }
    assert_eq!(seen, ids.iter().map(u32::to_string).collect::<Vec<_>>(),
               "panes reported ids that are not their own: {seen:?} vs {ids:?}");
}

/// The point of an id: it reaches a pane in a window that is NOT on screen.
/// A position cannot - it means "the Nth pane of whatever window is current".
#[test]
fn an_id_reaches_a_pane_in_a_background_window() {
    let d = Daemon::start("zmux-test-background-window");
    let mut c = Control::open(&d.session);
    let first = c.info().panes[0].id;

    // Make a second window. The first one is now in the background.
    assert!(matches!(c.ask(&ClientMsg::Command(Cmd::NewWindow)), ServerMsg::Info(_)));
    assert!(d.wait_for_window(1, 2, 10), "the window was never created");

    let info = c.info();
    assert_eq!(info.window, 1, "we should be looking at the second window");
    // Both windows' panes are listed, each tagged with its window.
    assert_eq!(info.panes.len(), 2, "info hid a background window's panes: {info:?}");
    let bg = info.panes.iter().find(|p| p.id == first).expect("window 0's pane vanished");
    assert_eq!(bg.window, 0);
    assert_eq!(bg.position, 1, "position is within its own window");

    // Type into the pane we cannot see, and read it back.
    c.input_at(Target::Id(first), "echo OFFSCREEN_OK", true);
    let screen = c
        .wait_at(20, Target::Id(first), |t| t.matches("OFFSCREEN_OK").count() >= 2)
        .expect("a background window's pane never ran the command");
    assert!(screen.contains("OFFSCREEN_OK"), "screen was:\n{screen}");

    // And the user is still looking at the window they were on.
    assert_eq!(c.info().window, 1, "driving a background pane switched windows");
}

/// `focus` is a position within a window, so it needs `window` beside it to
/// mean anything. `focus_id` is the handle that does not need either.
#[test]
fn info_names_the_focused_pane_unambiguously() {
    let d = Daemon::start("zmux-test-focus-id");
    let mut c = Control::open(&d.session);
    let info = c.info();
    let focused = info
        .panes
        .iter()
        .find(|p| p.window == info.window && p.position == info.focus)
        .expect("focus does not name a pane in the current window");
    assert_eq!(focused.id, info.focus_id);
    assert_ne!(info.focus_id, 0);
}

/// A target that resolves to nothing is refused BY NAME. Redirecting it to a
/// nearby pane is how a background helper types into somebody's real shell.
#[test]
fn a_target_that_does_not_resolve_is_refused_not_redirected() {
    let d = Daemon::start("zmux-test-bad-target");
    let mut c = Control::open(&d.session);

    let cases = [
        (Target::Id(9999), "@9999"),
        (Target::Position(7), "position 7"),
        (Target::Name("reviewer".into()), "'reviewer'"),
    ];
    for (target, expected) in cases {
        let msg = ClientMsg::InputAt {
            target: target.clone(),
            data: b"echo NEVER_RUNS".to_vec(),
            paste: false,
            enter: true,
        };
        match c.ask(&msg) {
            ServerMsg::Refused { reason } => assert!(
                reason.contains(expected),
                "refusal does not say which target was wrong: {reason}"
            ),
            other => panic!("{target:?} was not refused: {other:?}"),
        }
        match c.ask(&ClientMsg::Query(Query::CaptureAt { target: target.clone(), lines: 0 })) {
            ServerMsg::Refused { .. } => {}
            other => panic!("capture of {target:?} was not refused: {other:?}"),
        }
    }

    // And nothing reached the real pane.
    let screen = c.capture(None);
    assert!(!screen.contains("NEVER_RUNS"), "a refused write still landed:\n{screen}");
}

/// A `-p` write is background work: it must not drag the user's view back to
/// the bottom, and it must not move the focus.
#[test]
fn writing_by_target_moves_neither_focus_nor_scroll() {
    let d = Daemon::start("zmux-test-target-focus");
    let mut c = Control::open(&d.session);
    assert!(matches!(c.ask(&ClientMsg::Command(Cmd::SplitVertical)), ServerMsg::Info(_)));
    assert!(d.wait_for_panes(2, 10), "the split never appeared");

    let before = c.info();
    let other = before
        .panes
        .iter()
        .find(|p| p.position != before.focus)
        .expect("two panes, one of them not focused")
        .id;

    c.input_at(Target::Id(other), "echo BACKGROUND_OK", true);
    assert!(c
        .wait_at(20, Target::Id(other), |t| t.matches("BACKGROUND_OK").count() >= 2)
        .is_some());

    let after = c.info();
    assert_eq!(after.focus_id, before.focus_id, "a background write moved the focus");
}
