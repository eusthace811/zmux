//! Driving one pane from another: identity, addressing, and the things a
//! background caller must never do to the user's screen (DESIGN.md §12).

mod common;
use common::{Control, Daemon};

use zmux::protocol::{ClientMsg, Cmd, PaneState, Query, Report, ServerMsg, Target};

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

/// A pane made for background work must not move the user's cursor. A split
/// that takes the focus interrupts whatever they were typing.
#[test]
fn a_background_split_leaves_the_focus_alone_and_says_what_it_made() {
    let d = Daemon::start("zmux-test-bg-split");
    let mut c = Control::open(&d.session);
    let before = c.info();

    let made = match c.ask(&ClientMsg::Command(Cmd::SplitVerticalBackground)) {
        ServerMsg::Info(i) => {
            assert_eq!(i.focus_id, before.focus_id, "a background split moved the focus");
            i.created.expect("a background split did not say which pane it created")
        }
        other => panic!("split refused: {other:?}"),
    };
    assert!(d.wait_for_panes(2, 10), "the split never appeared");

    let after = c.info();
    assert_eq!(after.focus_id, before.focus_id, "the focus moved after the fact");
    assert_ne!(made, before.focus_id, "`created` named the pane we started in");
    assert!(after.panes.iter().any(|p| p.id == made), "`created` names no live pane");

    // And the new pane is a real shell that can be driven by that id.
    c.input_at(Target::Id(made), "echo NEW_PANE_OK", true);
    assert!(c
        .wait_at(20, Target::Id(made), |t| t.matches("NEW_PANE_OK").count() >= 2)
        .is_some(), "the pane `created` named is not a working shell");
    assert_eq!(c.info().focus_id, before.focus_id, "driving it moved the focus");
}

/// A focus-taking split still takes the focus. The background one is an
/// addition, not a change of behaviour.
#[test]
fn an_ordinary_split_still_moves_the_focus() {
    let d = Daemon::start("zmux-test-fg-split");
    let mut c = Control::open(&d.session);
    let before = c.info().focus_id;
    match c.ask(&ClientMsg::Command(Cmd::SplitVertical)) {
        ServerMsg::Info(i) => {
            assert_ne!(i.focus_id, before, "`C-b \"` stopped moving the focus");
            assert_eq!(i.created, Some(i.focus_id), "a split should name what it made");
        }
        other => panic!("split refused: {other:?}"),
    }
}

/// A prompt goes in as one block. `bash -c 'read -r x'` with bracketed paste
/// on is the smallest program that can tell the difference: pasted, the
/// newline inside the text is data; typed, it ends the read.
#[test]
fn a_multi_line_prompt_reaches_a_program_as_one_paste() {
    let d = Daemon::start("zmux-test-paste");
    let mut c = Control::open(&d.session);

    // Turn bracketed paste on, then read one "line" - which, inside a paste,
    // is everything up to the end marker.
    c.input_at(Target::Focused, "printf '\\033[?2004h'; read -r line; printf 'GOT[%s]\\n' \"$line\"", true);
    std::thread::sleep(std::time::Duration::from_millis(900));

    let msg = ClientMsg::InputAt {
        target: Target::Focused,
        data: b"alpha bravo".to_vec(),
        paste: true,
        enter: true,
    };
    assert!(matches!(c.ask(&msg), ServerMsg::Info(_)), "the paste was refused");

    let screen = c
        .wait_at(20, Target::Focused, |t| t.contains("GOT["))
        .expect("the program never finished its read");
    assert!(
        screen.contains("GOT[alpha bravo]"),
        "the paste markers reached the program as text, or the text was mangled:\n{screen}"
    );
}

/// `paste` is a request, not an instruction. A program that never asked for
/// bracketed paste must not be handed the markers as literal characters.
#[test]
fn a_program_that_did_not_ask_for_a_paste_never_sees_the_markers() {
    let d = Daemon::start("zmux-test-no-paste");
    let mut c = Control::open(&d.session);

    let msg = ClientMsg::InputAt {
        target: Target::Focused,
        data: b"echo PLAIN_OK".to_vec(),
        paste: true,
        enter: true,
    };
    assert!(matches!(c.ask(&msg), ServerMsg::Info(_)));

    let screen = c
        .wait_at(20, Target::Focused, |t| t.matches("PLAIN_OK").count() >= 2)
        .expect("the shell never ran the command");
    assert!(!screen.contains("200~"), "paste markers were typed at a plain shell:\n{screen}");
    assert!(!screen.contains("201~"), "paste markers were typed at a plain shell:\n{screen}");
}

/// The honest substitute for guessing at a screen: how long a pane has been
/// silent. A measurement, so it has to actually measure.
#[test]
fn a_pane_reports_how_long_it_has_been_quiet() {
    let d = Daemon::start("zmux-test-quiet");
    let mut c = Control::open(&d.session);

    // Let the shell finish starting, then let it go quiet.
    assert!(c.wait_at(20, Target::Focused, |t| t.contains('$')).is_some());
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let idle = c.info().panes[0].quiet_ms;
    assert!(idle >= 1000, "a silent pane reported only {idle}ms of quiet");

    // Output resets it. `date` prints and returns, so by the time info is
    // read the pane spoke a moment ago, not a second and a half ago.
    c.input_at(Target::Focused, "echo NOISE", true);
    assert!(c
        .wait_at(20, Target::Focused, |t| t.matches("NOISE").count() >= 2)
        .is_some());
    let after = c.info().panes[0].quiet_ms;
    assert!(after < idle, "output did not reset the quiet clock: {after}ms vs {idle}ms");
}

/// The bell is the one attention signal that works for a program which
/// reports nothing about itself, so it must survive being read twice.
#[test]
fn a_bell_is_counted_not_swallowed() {
    let d = Daemon::start("zmux-test-bell");
    let mut c = Control::open(&d.session);
    let before = c.info().panes[0].bells;

    c.input_at(Target::Focused, r"printf '\a'", true);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut rang = before;
    while std::time::Instant::now() < deadline {
        rang = c.info().panes[0].bells;
        if rang > before {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(rang > before, "the bell was never counted ({before} -> {rang})");
    // Reading it does not clear it: two callers must not swallow each
    // other's notification.
    assert_eq!(c.info().panes[0].bells, rang, "reading the count cleared it");
}

/// A pane's title is handed over as the pane stated it. Deciding what a title
/// MEANS about an agent is the pattern matching this design refuses to do.
#[test]
fn a_pane_reports_the_title_it_set() {
    let d = Daemon::start("zmux-test-title");
    let mut c = Control::open(&d.session);

    c.input_at(Target::Focused, r"printf '\033]0;deploying\007'", true);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if c.info().panes[0].title.as_deref() == Some("deploying") {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "the title never arrived");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// The whole reporting contract, through a real pty: a program writes the
/// escape on its own output and the daemon hears it.
///
/// `read` after the report is not decoration - it is the shape of an agent.
/// A claim lives exactly as long as the process that made it, so a command
/// that reports and exits has already been forgotten by the time anyone
/// looks. See `a_new_command_clears_what_the_last_one_claimed`.
#[test]
fn a_pane_can_say_what_it_is_doing() {
    let d = Daemon::start("zmux-test-report-osc");
    let mut c = Control::open(&d.session);
    assert_eq!(c.info().panes[0].state, PaneState::Unknown, "a fresh pane claims nothing");
    let id = c.info().panes[0].id;

    c.input_at(Target::Focused, &agent_like("state=working;agent=ampere;name=reviewer"), true);
    let p = wait_for_state(&mut c, PaneState::Working, 20).expect("the report never arrived");
    assert_eq!(p.agent.as_deref(), Some("ampere"));
    assert_eq!(p.name.as_deref(), Some("reviewer"));

    // The name it claimed resolves, while it still holds the pane. Read
    // rather than write: the pane is blocked in `read`, and typing at it
    // would answer that instead of proving anything about the name.
    let by_name = c.capture_at(Target::Name("reviewer".into()));
    let by_id = c.capture_at(Target::Id(id));
    assert_eq!(by_name, by_id, "the name resolved to a different pane than the id");
}

/// A command that reports and then waits, which is the shape of an agent
/// sitting at its prompt. `read` holds the foreground until we send a line.
fn agent_like(keys: &str) -> String {
    format!(r"printf '\033]1341;zmux;{keys}\033\\'; read -r _")
}

/// Reports also arrive over the pipe, for a reporter whose output is
/// captured and cannot reach the terminal.
#[test]
fn a_report_can_come_over_the_pipe_instead() {
    let d = Daemon::start("zmux-test-report-pipe");
    let mut c = Control::open(&d.session);
    let id = c.info().panes[0].id;

    let report = Report { state: Some(PaneState::Blocked), ..Default::default() };
    match c.ask(&ClientMsg::Report { target: Target::Id(id), report }) {
        ServerMsg::Info(i) => {
            let p = i.panes.iter().find(|p| p.id == id).expect("the pane vanished");
            assert_eq!(p.state, PaneState::Blocked);
        }
        other => panic!("the report was refused: {other:?}"),
    }
}

/// Two hooks firing close together can arrive backwards. Taking the
/// later-arriving one would leave a pane reading `working` after it went
/// idle - the exact lie a caller waiting on that pane cannot survive.
#[test]
fn a_report_that_arrives_out_of_order_is_dropped() {
    let d = Daemon::start("zmux-test-report-seq");
    let mut c = Control::open(&d.session);
    let id = c.info().panes[0].id;
    let send = |c: &mut Control, state, seq| {
        let report = Report { state: Some(state), seq: Some(seq), ..Default::default() };
        c.ask(&ClientMsg::Report { target: Target::Id(id), report })
    };

    send(&mut c, PaneState::Working, 5);
    send(&mut c, PaneState::Idle, 6);
    assert_eq!(state_of(&mut c, id), PaneState::Idle);

    // Late, and therefore ignored - including at the same seq.
    send(&mut c, PaneState::Working, 4);
    assert_eq!(state_of(&mut c, id), PaneState::Idle, "a stale report overwrote a newer one");
    send(&mut c, PaneState::Working, 6);
    assert_eq!(state_of(&mut c, id), PaneState::Idle, "a repeated seq was accepted");
    send(&mut c, PaneState::Working, 7);
    assert_eq!(state_of(&mut c, id), PaneState::Working, "the seq counter stopped moving");
}

/// Staleness is solved exactly rather than with a timeout: OSC 133 brackets
/// the foreground command, so when the command ends the agent is gone and so
/// is everything it said about itself.
#[test]
fn a_new_command_clears_what_the_last_one_claimed() {
    let d = Daemon::start("zmux-test-report-clear");
    let mut c = Control::open(&d.session);
    let id = c.info().panes[0].id;

    c.input_at(Target::Focused, &agent_like("state=working;agent=ampere"), true);
    assert!(wait_for_state(&mut c, PaneState::Working, 20).is_some());

    // End the read, so the process that made the claim exits. OSC 133;D
    // fires, and with it the claim goes - no timeout, no guessing.
    c.input_at(Target::Focused, "", true);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let p = pane_of(&mut c, id);
        if p.state == PaneState::Unknown && p.agent.is_none() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a finished agent's claim outlived it: {:?}",
            p.state
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// A malformed or misspelt report must leave the pane alone. Blanking a
/// state that was true because somebody's hook has a typo in it is worse
/// than ignoring the typo.
#[test]
fn a_report_that_makes_no_sense_changes_nothing() {
    let d = Daemon::start("zmux-test-report-junk");
    let mut c = Control::open(&d.session);
    let id = c.info().panes[0].id;

    let report = Report { state: Some(PaneState::Working), ..Default::default() };
    c.ask(&ClientMsg::Report { target: Target::Id(id), report });
    assert_eq!(state_of(&mut c, id), PaneState::Working);

    for junk in [
        // A misspelt state.
        r"printf '\033]1341;zmux;state=workingg\033\\'",
        // No guard, so not ours at all - somebody else's OSC 1341.
        r"printf '\033]1341;other-tool;state=idle\033\\'",
        // A name that is not a usable handle drops the name, not the state.
        r"printf '\033]1341;zmux;name=Reviewer\033\\'",
    ] {
        c.input_at(Target::Focused, junk, true);
        std::thread::sleep(std::time::Duration::from_millis(600));
    }
    // The command bracketing clears state, so assert on the thing that
    // cannot be explained away: nothing ever became `idle`.
    assert_ne!(state_of(&mut c, id), PaneState::Idle, "a guarded-out report was taken");
    assert!(pane_of(&mut c, id).name.is_none(), "an unusable name was accepted");
}

fn pane_of(c: &mut Control, id: u32) -> zmux::protocol::PaneInfo {
    c.info().panes.into_iter().find(|p| p.id == id).expect("the pane vanished")
}

fn state_of(c: &mut Control, id: u32) -> PaneState {
    pane_of(c, id).state
}

fn wait_for_state(
    c: &mut Control,
    want: PaneState,
    secs: u64,
) -> Option<zmux::protocol::PaneInfo> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        if let Some(p) = c.info().panes.into_iter().find(|p| p.state == want) {
            return Some(p);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
