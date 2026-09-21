//! Driving one pane from another: identity, addressing, and the things a
//! background caller must never do to the user's screen (DESIGN.md §12).

mod common;
use common::{Control, Daemon};

use zmux::protocol::{
    ClientMsg, Cmd, PaneState, Query, Report, ServerMsg, Source, Target, Until, WaitOutcome,
};

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
        let q = Query::CaptureAt { target: target.clone(), lines: 0, source: Source::Screen };
        match c.ask(&ClientMsg::Query(q)) {
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

/// A prompt goes in as one block - but only because the DAEMON looked at
/// the pane's live vt100 state and saw bracketed paste turned on. That
/// lookup is the thing worth proving end to end; `src/paste.rs` covers the
/// wrapping itself.
///
/// `cat -v` renders the escapes visibly, which is the same trick
/// tests/mouse.rs uses. It is in canonical mode, so nothing appears until
/// the Enter that rides in the same write arrives - which is also what makes
/// the ordering visible.
#[test]
fn a_prompt_is_pasted_when_the_program_asked_for_pastes() {
    let d = Daemon::start("zmux-test-paste");
    let mut c = Control::open(&d.session);

    // The marker is split by quoting so the ECHOED command line does not
    // contain it - waiting on one the echo satisfies would start pasting
    // before `cat` is running. tests/smoke.rs plays the same trick.
    c.input_at(Target::Focused, "printf '\\033[?2004h'; printf 'REA''DY\\n'; cat -v", true);
    assert!(
        c.wait_at(20, Target::Focused, |t| t.contains("READY")).is_some(),
        "the program never started"
    );

    let msg = ClientMsg::InputAt {
        target: Target::Focused,
        data: b"alpha bravo".to_vec(),
        paste: true,
        enter: true,
    };
    assert!(matches!(c.ask(&msg), ServerMsg::Info(_)), "the paste was refused");

    let screen = c
        .wait_at(20, Target::Focused, |t| t.contains("^[[200~"))
        .expect("nothing arrived at the program");
    assert!(
        screen.contains("^[[200~alpha bravo^[[201~"),
        "the daemon did not wrap the text for a program that asked for pastes:\n{screen}"
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

/// The rule is "at or below the last one ACCEPTED", and before the first
/// report there is no last one.
///
/// The counter used to be a plain `u64` starting at zero, which made zero
/// read as already-accepted: an agent numbering its reports from 0 lost its
/// first one silently, and lost another after every command boundary, because
/// a `133;C`/`133;D` puts the counter back.
#[test]
fn the_first_report_is_heard_even_when_it_is_numbered_zero() {
    let d = Daemon::start("zmux-test-report-zero");
    let mut c = Control::open(&d.session);
    let id = c.info().panes[0].id;
    let send = |c: &mut Control, state, seq| {
        let report = Report { state: Some(state), seq: Some(seq), ..Default::default() };
        c.ask(&ClientMsg::Report { target: Target::Id(id), report })
    };

    send(&mut c, PaneState::Working, 0);
    assert_eq!(
        state_of(&mut c, id),
        PaneState::Working,
        "a first report numbered zero was dropped"
    );
    // And zero is then a used number like any other.
    send(&mut c, PaneState::Idle, 0);
    assert_eq!(state_of(&mut c, id), PaneState::Working, "seq 0 was accepted twice");
    send(&mut c, PaneState::Idle, 1);
    assert_eq!(state_of(&mut c, id), PaneState::Idle);
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

/// Reading an agent's answer means reading past the visible rows - and doing
/// it without the user's pane jumping, which is why `capture` could only see
/// the screen before.
#[test]
fn recent_reads_past_the_visible_screen_without_moving_it() {
    let d = Daemon::start("zmux-test-scrollback");
    let mut c = Control::open(&d.session);
    let id = c.info().panes[0].id;

    // Comfortably more than one screenful.
    c.input_at(Target::Focused, "for i in $(seq 1 120); do echo LINE_$i; done", true);
    assert!(c
        .wait_at(20, Target::Focused, |t| t.contains("LINE_120"))
        .is_some(), "the output never appeared");

    let screen = c.capture_at(Target::Id(id));
    assert!(!screen.contains("LINE_1\n"), "120 lines fit on screen; the test proves nothing");

    let recent = capture_source(&mut c, id, 300, Source::Recent);
    assert!(recent.contains("LINE_5"), "scrollback was not read:\n{recent}");
    assert!(recent.contains("LINE_120"), "the live screen was lost:\n{recent}");
    // In order, and each line once.
    assert_eq!(recent.matches("LINE_42\n").count(), 1, "a row was collected twice");
    let at5 = recent.find("LINE_5\n").expect("LINE_5");
    let at90 = recent.find("LINE_90\n").expect("LINE_90");
    assert!(at5 < at90, "history came back out of order");

    // Fewer lines means the LAST lines, not the first.
    let tail = capture_source(&mut c, id, 5, Source::Recent);
    assert!(tail.contains("LINE_120"), "a short read lost the newest output:\n{tail}");
    assert!(!tail.contains("LINE_5\n"), "a short read returned too much:\n{tail}");
}

/// The reason `capture` refused to read history for so long: doing it moves
/// the pane's scroll position, which is the user's own state.
#[test]
fn reading_history_leaves_the_users_scroll_position_alone() {
    let d = Daemon::start("zmux-test-scroll-restore");
    let mut c = Control::open(&d.session);
    let id = c.info().panes[0].id;

    c.input_at(Target::Focused, "for i in $(seq 1 80); do echo ROW_$i; done", true);
    assert!(c.wait_at(20, Target::Focused, |t| t.contains("ROW_80")).is_some());

    // Scroll back, the way a user reading their own output would.
    for _ in 0..3 {
        assert!(matches!(c.ask(&ClientMsg::Command(Cmd::ScrollPageUp)), ServerMsg::Info(_)));
    }
    let before = c.capture_at(Target::Id(id));
    assert!(!before.contains("ROW_80"), "the scroll did not move the view");

    // A background read of the whole history.
    let _ = capture_source(&mut c, id, 300, Source::RecentUnwrapped);

    let after = c.capture_at(Target::Id(id));
    assert_eq!(after, before, "reading history moved the user's view");
}

/// A pane is as wide as its share of the window, so a long line comes back
/// broken at that width. Those breaks are the layout talking, not the text.
#[test]
fn unwrapped_joins_a_line_the_pane_was_too_narrow_to_hold() {
    let d = Daemon::start("zmux-test-unwrap");
    let mut c = Control::open(&d.session);
    let id = c.info().panes[0].id;
    let cols = c.info().panes[0].cols as usize;
    assert!(cols > 10, "the pane has no width to overflow");

    // One logical line, comfortably wider than the pane.
    let word = "x".repeat(cols * 2 + 5);
    c.input_at(Target::Focused, &format!("echo START_{word}_END"), true);
    assert!(c.wait_at(20, Target::Focused, |t| t.contains("_END")).is_some());

    let wrapped = capture_source(&mut c, id, 50, Source::Recent);
    let joined = capture_source(&mut c, id, 50, Source::RecentUnwrapped);
    let whole = format!("START_{word}_END");
    assert!(
        !wrapped.lines().any(|l| l.contains(&whole)),
        "the pane was wide enough after all; the test proves nothing"
    );
    assert!(
        joined.lines().any(|l| l.contains(&whole)),
        "soft wraps were not joined; the line is still in pieces"
    );
}

fn capture_source(c: &mut Control, id: u32, lines: u16, source: Source) -> String {
    let q = Query::CaptureAt { target: Target::Id(id), lines, source };
    match c.ask(&ClientMsg::Query(q)) {
        ServerMsg::Capture { text } => text,
        other => panic!("expected Capture, got {other:?}"),
    }
}

/// Waiting for an ordinary command to finish, which is what `busy` is for.
#[test]
fn a_wait_returns_when_the_command_finishes() {
    let d = Daemon::start("zmux-test-wait-quiet");
    let mut c = Control::open(&d.session);
    assert!(c.wait_at(20, Target::Focused, |t| t.contains('$')).is_some());

    c.input_at(Target::Focused, "sleep 2; echo SLEPT", true);

    // The race this exists to prove is closed: the shell has NOT read the
    // line yet, so it is not busy yet, and a wait for quiet with no settle
    // gate would be satisfied by the quiet before the command.
    let started = std::time::Instant::now();
    let outcome = waited_settling(&mut c, Target::Focused, Until::Quiet, 30_000, 3_000);
    assert_eq!(outcome, WaitOutcome::Met);
    let took = started.elapsed();
    assert!(took.as_millis() >= 1000, "the wait returned before the command could run: {took:?}");
    assert!(c.capture_at(Target::Focused).contains("SLEPT"), "waited for the wrong thing");
}

/// The wait an agent needs. `blocked` counts as settled: a caller told only
/// about a clean finish waits out its whole timeout on the one case that
/// needed a person.
#[test]
fn a_wait_for_settled_wakes_on_blocked_as_well_as_done() {
    let d = Daemon::start("zmux-test-wait-settled");
    let mut c = Control::open(&d.session);
    let id = c.info().panes[0].id;

    for state in [PaneState::Blocked, PaneState::Done, PaneState::Idle] {
        let report = Report { state: Some(state), ..Default::default() };
        c.ask(&ClientMsg::Report { target: Target::Id(id), report });
        assert_eq!(
            waited(&mut c, Target::Id(id), Until::Settled, 2_000),
            WaitOutcome::Met,
            "{state:?} should have settled the wait"
        );
    }

    let report = Report { state: Some(PaneState::Working), ..Default::default() };
    c.ask(&ClientMsg::Report { target: Target::Id(id), report });
    assert_eq!(
        waited(&mut c, Target::Id(id), Until::Settled, 700),
        WaitOutcome::Timeout,
        "a working pane satisfied a wait for settled"
    );
}

/// `--until <one specific state>`, which `Settled` does not cover.
///
/// `settled` is the usual wait and is tested above, but the CLI also takes a
/// state by name - `--until blocked` is how a caller watches for an agent
/// that stops to ask something, as opposed to one that merely stops. Nothing
/// exercised that arm of `Until`.
#[test]
fn a_wait_for_one_named_state_ignores_the_others() {
    let d = Daemon::start("zmux-test-wait-state");
    let mut c = Control::open(&d.session);
    let id = c.info().panes[0].id;
    let report = |c: &mut Control, state| {
        let r = Report { state: Some(state), ..Default::default() };
        c.ask(&ClientMsg::Report { target: Target::Id(id), report: r });
    };

    // Settled, but not the state asked for: `done` must not answer for
    // `blocked`, or a caller waiting for a question gets told about a finish.
    report(&mut c, PaneState::Done);
    assert_eq!(
        waited(&mut c, Target::Id(id), Until::State(PaneState::Blocked), 700),
        WaitOutcome::Timeout,
        "`done` satisfied a wait for `blocked`"
    );

    report(&mut c, PaneState::Blocked);
    assert_eq!(
        waited(&mut c, Target::Id(id), Until::State(PaneState::Blocked), 2_000),
        WaitOutcome::Met
    );

    // And a state nobody has reported is not reached by waiting.
    assert_eq!(
        waited(&mut c, Target::Id(id), Until::State(PaneState::Working), 700),
        WaitOutcome::Timeout
    );
}

/// `--until bell`, the attention signal that works for a program reporting
/// nothing about itself.
///
/// It must mean "rings FROM HERE", not "has ever rung" - `wait_for` reads the
/// count once before it starts for exactly that reason. A pane that rang an
/// hour ago is not asking for anything now, and a wait satisfied by that
/// history would return instantly forever after.
#[test]
fn a_wait_for_a_bell_means_the_next_one_not_the_last() {
    let d = Daemon::start("zmux-test-wait-bell");
    let mut c = Control::open(&d.session);
    assert!(c.wait_at(20, Target::Focused, |t| t.contains('$')).is_some());
    let id = c.info().panes[0].id;

    // Ring it once, and let the daemon see it.
    c.input_at(Target::Id(id), "printf '\\a'", true);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < deadline && pane_of(&mut c, id).bells == 0 {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(pane_of(&mut c, id).bells > 0, "the pane's bell was never counted");

    // That one is history. A wait starting now must not be satisfied by it.
    assert_eq!(
        waited(&mut c, Target::Id(id), Until::Bell, 700),
        WaitOutcome::Timeout,
        "a wait for a bell was answered by one that had already rung"
    );

    // A second connection, because `c` is about to block inside the wait.
    let mut ringer = Control::open(&d.session);
    let session = d.session.clone();
    let ring = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let _ = session;
        ringer.input_at(Target::Id(id), "printf '\\a'", true);
    });
    assert_eq!(
        waited(&mut c, Target::Id(id), Until::Bell, 20_000),
        WaitOutcome::Met,
        "a bell rung during the wait did not end it"
    );
    ring.join().expect("the ringer thread panicked");
}

/// Text already on screen matches at once. A caller that asks after the fact
/// must not be left waiting for something that has already happened.
#[test]
fn a_wait_for_output_matches_what_is_already_there() {
    let d = Daemon::start("zmux-test-wait-output");
    let mut c = Control::open(&d.session);

    c.input_at(Target::Focused, "echo ALREADY_PRINTED", true);
    assert!(c
        .wait_at(20, Target::Focused, |t| t.matches("ALREADY_PRINTED").count() >= 2)
        .is_some());

    let started = std::time::Instant::now();
    let until = Until::Output("ALREADY_PRINTED".into());
    assert_eq!(waited(&mut c, Target::Focused, until, 10_000), WaitOutcome::Met);
    assert!(started.elapsed().as_millis() < 2_000, "matching existing output was slow");

    let until = Until::Output("NEVER_PRINTED_AT_ALL".into());
    assert_eq!(waited(&mut c, Target::Focused, until, 700), WaitOutcome::Timeout);
}

/// The honest wait for a program that reports nothing: a measured length of
/// silence, with the caller choosing how much of it to believe.
///
/// The noise is produced FROM HERE, on a cadence this test controls. An
/// earlier version ran `echo; sleep 0.1` in the pane and assumed that meant
/// output every 100ms. In Git Bash `sleep` is a separate process, and with
/// the rest of the suite running its own daemons a spawn can take longer
/// than the 800ms of silence being waited for - so the pane really did go
/// quiet, the wait really was `Met`, and the test was the thing that was
/// wrong. It failed about 45% of the time. Typing a character is one write
/// with nothing to schedule.
#[test]
fn a_wait_for_silence_measures_it() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    let d = Daemon::start("zmux-test-wait-silence");
    let mut c = Control::open(&d.session);
    assert!(c.wait_at(20, Target::Focused, |t| t.contains('$')).is_some());

    // A second connection, because `c` is about to block inside a wait.
    let chatter = Control::open(&d.session);
    let talking = Arc::new(AtomicBool::new(true));
    let stop = talking.clone();

    // One write BEFORE the wait starts. Leaving even the first character to
    // the thread is a race: the pane has been silent at its prompt since the
    // daemon came up, so a wait that polls before the thread's first write
    // finds 800ms of silence already banked and returns `Met` at once.
    c.input_at(Target::Focused, ".", false);

    let noise = std::thread::spawn(move || {
        let mut chatter = chatter;
        // Echoed by the pane's line editor, so each one is real output. No
        // Enter: nothing runs, the line just grows.
        while stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
            chatter.input_at(Target::Focused, ".", false);
        }
    });

    // A pane that is still talking cannot satisfy it. 100ms of cadence
    // against 800ms of required silence leaves eight times the margin the
    // scheduler needs to be slow in.
    assert_eq!(
        waited(&mut c, Target::Focused, Until::QuietFor(800), 1_500),
        WaitOutcome::Timeout,
        "a pane producing output was called quiet"
    );

    talking.store(false, Ordering::Relaxed);
    noise.join().expect("the chatter thread panicked");

    // Once it stops, it does.
    assert_eq!(
        waited(&mut c, Target::Focused, Until::QuietFor(800), 30_000),
        WaitOutcome::Met
    );
}

/// A wait pins the pane it resolved. Otherwise asking to wait on position 2,
/// having that pane exit, and another sliding into position 2 would let the
/// replacement answer for it.
#[test]
fn a_wait_ends_when_the_pane_it_watches_goes_away() {
    let d = Daemon::start("zmux-test-wait-gone");
    let mut c = Control::open(&d.session);
    assert!(matches!(c.ask(&ClientMsg::Command(Cmd::SplitVertical)), ServerMsg::Info(_)));
    assert!(d.wait_for_panes(2, 10), "the split never appeared");

    let info = c.info();
    let victim = info.focus_id;
    assert!(info.panes.len() == 2);

    // Wait on something that will never come true, while the pane exits.
    let mut victim_c = Control::open(&d.session);
    let handle = std::thread::spawn(move || {
        waited(&mut victim_c, Target::Id(victim), Until::Output("NEVER".into()), 30_000)
    });
    std::thread::sleep(std::time::Duration::from_millis(600));
    c.input_at(Target::Id(victim), "exit", true);

    assert_eq!(handle.join().expect("the waiting thread panicked"), WaitOutcome::Gone);
}

fn waited(c: &mut Control, target: Target, until: Until, timeout_ms: u32) -> WaitOutcome {
    // No settle gate: these tests set the pane up first and then wait, so
    // there is nothing to wait for the pane to start doing.
    waited_settling(c, target, until, timeout_ms, 0)
}

fn waited_settling(
    c: &mut Control,
    target: Target,
    until: Until,
    timeout_ms: u32,
    settle_ms: u32,
) -> WaitOutcome {
    match c.ask(&ClientMsg::Wait { target, until, timeout_ms, settle_ms }) {
        ServerMsg::Waited { outcome, .. } => outcome,
        other => panic!("expected Waited, got {other:?}"),
    }
}

/// Without the settle gate, "run it, then wait for quiet" is satisfied by
/// the quiet BEFORE the command - the shell has not read the line yet. This
/// pins that the gate is what closes it, rather than luck.
#[test]
fn without_a_settle_gate_a_wait_can_answer_about_the_wrong_moment() {
    let d = Daemon::start("zmux-test-settle");
    let mut c = Control::open(&d.session);
    assert!(c.wait_at(20, Target::Focused, |t| t.contains('$')).is_some());

    c.input_at(Target::Focused, "sleep 3; echo GATED", true);
    let started = std::time::Instant::now();
    assert_eq!(
        waited_settling(&mut c, Target::Focused, Until::Quiet, 30_000, 0),
        WaitOutcome::Met
    );
    let ungated = started.elapsed();

    // It came back essentially at once, before `sleep 3` could have ended.
    assert!(
        ungated.as_millis() < 2_500,
        "the ungated race did not reproduce, so the gate proves nothing: {ungated:?}"
    );
    assert!(
        !c.capture_at(Target::Focused).contains("GATED"),
        "the command had already finished; the timing above was a coincidence"
    );

    // The same wait, gated, waits for the command it was asked about.
    assert_eq!(
        waited_settling(&mut c, Target::Focused, Until::Quiet, 30_000, 3_000),
        WaitOutcome::Met
    );
    assert!(
        c.wait_at(20, Target::Focused, |t| t.contains("GATED")).is_some(),
        "the gated wait still returned before the command finished"
    );
}

/// A TUI that stamps OSC 133 zone markers around its own messages must not
/// lose the state it reported.
///
/// pi and ampere put `133;A` / `133;B` / `133;C` around every assistant and
/// user message so a terminal can jump between them
/// (`modes/interactive/components/assistant-message.ts`). zmux used to read
/// `133;C` as "a new foreground command started" and clear the pane's claim,
/// which threw away the agent's own report of itself once per message. Found
/// by running two real agents, not by reading the code.
#[test]
fn a_tui_that_marks_its_own_message_zones_keeps_what_it_reported() {
    let d = Daemon::start("zmux-test-zone-markers");
    let mut c = Control::open(&d.session);

    // Claim something, then stamp the zone markers a message-rendering TUI
    // emits, then keep holding the foreground - exactly pi's shape.
    let keys = "state=working;agent=ampere:some-model;name=reviewer";
    let zones = r"printf '\033]133;A\007\033]133;B\007\033]133;C\007'";
    let cmd = format!(r"printf '\033]1341;zmux;{keys}\033\\'; {zones}; read -r _");
    c.input_at(Target::Focused, &cmd, true);

    let p = wait_for_state(&mut c, PaneState::Working, 20).expect("the report never arrived");
    assert_eq!(p.agent.as_deref(), Some("ampere:some-model"), "the zone markers ate the agent");
    assert_eq!(p.name.as_deref(), Some("reviewer"), "the zone markers ate the name");

    // And the thing that DOES mean the process is gone still works: let the
    // `read` finish, and OSC 133;D clears everything it claimed.
    c.input_at(Target::Focused, "", true);
    let gone = wait_for_state(&mut c, PaneState::Unknown, 20).expect("the claim never cleared");
    assert_eq!(gone.agent, None, "a claim outlived the process that made it");
    assert_eq!(gone.name, None);
}

/// The settle gate must hold for a pane that was ALREADY busy.
///
/// An agent is one long foreground command: OSC 133;C set `busy` when it
/// started and it stays set for as long as the agent lives. The gate tested
/// `now.busy` as a LEVEL, so for the one pane it exists to protect - an agent
/// being prompted - it opened on the very first poll, and the wait was
/// answered by the state the pane was in BEFORE the prompt. `--settle` says
/// it waits "until the pane has been seen doing something"; having been busy
/// since before the wait began is not something that happened since.
///
/// `start` is already read up front so `Until::Bell` can mean "from here on".
/// This is the same rule applied to the other two signals the gate watches.
#[test]
fn a_settle_gate_holds_for_a_pane_that_was_already_busy() {
    let d = Daemon::start("zmux-test-settle-busy");
    let mut c = Control::open(&d.session);
    assert!(c.wait_at(20, Target::Focused, |t| t.contains('$')).is_some());

    // An agent's shape: claim a settled state, then hold the foreground.
    let keys = "state=idle;agent=ampere";
    let cmd = format!(r"printf '\033]1341;zmux;{keys}\033\\'; read -r _");
    c.input_at(Target::Focused, &cmd, true);
    let p = wait_for_state(&mut c, PaneState::Idle, 20).expect("the report never arrived");
    assert!(p.busy, "the pane is not busy, so this test would prove nothing");

    // Nothing has happened since that report. The gate must hold its full
    // time rather than handing back the state that was already there.
    let started = std::time::Instant::now();
    assert_eq!(
        waited_settling(&mut c, Target::Focused, Until::Settled, 30_000, 1_500),
        WaitOutcome::Met
    );
    let held = started.elapsed();
    assert!(
        held.as_millis() >= 1_400,
        "the gate opened at once for a pane that was busy before the wait began: {held:?}"
    );
}
