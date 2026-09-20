//! The mouse belongs to whoever is using it.
//!
//! These drive a real Git Bash through a real ConPTY: the pane turns mouse
//! reporting on the way any program does, with a DECSET, and `cat -v` prints
//! back exactly the bytes it is handed - so what the test asserts on is the
//! wire format a program would actually receive.

mod common;

use common::Daemon;
use zmux::protocol::{ClientMsg, Cmd, MouseIn, MouseKind};

fn press(x: u16, y: u16) -> ClientMsg {
    ClientMsg::Mouse(MouseIn { x, y, kind: MouseKind::Press(0), mods: 0 })
}

/// A program that asked for the mouse gets the event, in its own coordinates,
/// and zmux keeps its hands off - including off the click that would
/// otherwise have moved focus.
#[test]
fn a_program_that_asks_for_the_mouse_is_given_it() {
    let d = Daemon::start("mouse-forward");
    let mut c = d.attach_ready(80, 24);
    // `?1000h` is the ask, `?1006h` chooses SGR. `cat -v` then shows whatever
    // arrives on its stdin, with ESC printed as `^[`.
    c.input("printf '\\033[?1000h\\033[?1006h'; cat -v\n");
    assert!(c.wait_for_mouse(20, true), "the daemon never saw the program ask");

    c.send(&press(4, 9));
    // The pty is in canonical mode, so `cat` sees nothing until a line ends.
    // The report carries no newline of its own - a real program reading a
    // mouse would not be in canonical mode - so the test supplies one.
    c.input("\n");
    // Pane-local and 1-based: one pane fills the screen above the status bar,
    // so screen (4, 9) is the program's (5, 10).
    let want = "^[[<0;5;10M";
    let s = c.wait_for(20, |s| s.contains(want)).unwrap_or_default();
    assert!(s.contains(want), "expected {want} on screen: {s}");
}

/// And when it stops asking, the mouse goes back - otherwise one run of vim
/// would cost you selection for the rest of the session.
#[test]
fn the_mouse_comes_back_when_the_program_stops_asking() {
    let d = Daemon::start("mouse-return");
    let mut c = d.attach_ready(80, 24);
    c.input("printf '\\033[?1000h\\033[?1006h'\n");
    assert!(c.wait_for_mouse(20, true), "the daemon never saw the program ask");
    c.input("printf '\\033[?1000l\\033[?1006l'\n");
    assert!(c.wait_for_mouse(20, false), "the mouse was never handed back");
}

/// With nobody asking, a click does what it always did.
#[test]
fn a_click_still_focuses_a_pane_when_no_program_wants_it() {
    let d = Daemon::start("mouse-focus");
    let mut c = d.attach_ready(80, 24);
    c.cmd(Cmd::SplitHorizontal);
    assert!(d.wait_for_panes(2, 20), "the split never happened");
    assert!(d.wait_for_focus(2, 20), "a new pane takes focus");

    c.send(&press(1, 1));
    assert!(d.wait_for_focus(1, 20), "clicking the left-hand pane should focus it");
}
