//! The REAL client, hosted in a ConPTY, typed into the way a terminal types.
//!
//! Every other test talks to the daemon over the pipe and never runs
//! `zmux.exe` at all, so nothing covered how the client reads its console -
//! which is where pastes were being lost: under crossterm a paste arrived as
//! one keystroke per character, and the program in the pane never saw a
//! paste. Here the test is the terminal. A ConPTY is exactly what mintty and
//! Windows Terminal put between themselves and a native program, so what
//! passes through this one passes through theirs.

mod common;

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{Control, Daemon};
use zmux::protocol::Target;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// A terminal with `zmux` attached inside it.
struct Host {
    output: Arc<Mutex<Vec<u8>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    // Held so the pseudoconsole outlives the test body.
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl Host {
    fn attach(session: &str) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 })
            .expect("openpty");
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_zmux"));
        cmd.arg(session);
        // The same scrub `pty::pane_command` does, for the same reason.
        cmd.env_clear();
        for (k, v) in std::env::vars_os() {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd).expect("spawn zmux");
        drop(pair.slave);

        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::new(Mutex::new(pair.master.take_writer().expect("writer")));
        let mut reader = pair.master.try_clone_reader().expect("reader");
        let (out, answer) = (Arc::clone(&output), Arc::clone(&writer));
        // Drain continuously - a full pipe stalls the client's drawing - and
        // answer the cursor query ConPTY sends at startup, which a real
        // terminal would (README landmine 1).
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if buf[..n].windows(4).any(|w| w == b"\x1b[6n") {
                    let _ = answer.lock().unwrap().write_all(b"\x1b[1;1R");
                }
                out.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Self { output, writer, child, _master: pair.master }
    }

    fn saw(&self, needle: &[u8]) -> bool {
        self.output.lock().unwrap().windows(needle.len()).any(|w| w == needle)
    }

    fn wait_to_see(&self, needle: &[u8], secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if self.saw(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// What the terminal sends: keystrokes, or a paste.
    fn send(&self, bytes: &[u8]) {
        let mut w = self.writer.lock().unwrap();
        w.write_all(bytes).expect("write to the client's console");
        w.flush().unwrap();
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// Start `cat -v` in the focused pane, having asked for bracketed paste, and
/// wait until it is reading. The marker is split by quoting so the echoed
/// command line cannot satisfy the wait (tests/agents.rs plays the same trick).
fn start_cat(c: &mut Control, ask_for_pastes: bool) {
    let ask = if ask_for_pastes { "printf '\\033[?2004h'; " } else { "" };
    c.input_at(Target::Focused, &format!("{ask}printf 'REA''DY\\n'; cat -v"), true);
    assert!(
        c.wait_at(20, Target::Focused, |t| t.contains("READY")).is_some(),
        "cat never started"
    );
}

/// Waits for the client to have taken over the console.
///
/// The session taking this terminal's width says the client said hello; that
/// happens BEFORE it switches the console to VT input, so the test also
/// waits for a drawn prompt, which only comes after.
fn attached(host: &Host, c: &mut Control) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while c.info().cols != 100 {
        assert!(Instant::now() < deadline, "the client never attached");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(host.wait_to_see(b"$", 20), "the client never drew the session");
}

/// The bug. A host paste has to reach the pane's program as ONE bracketed
/// block. Before the fix `cat -v` showed `alpha bravo` with no markers: the
/// text had been typed at it, a keystroke per character.
#[test]
fn a_host_paste_reaches_the_program_as_one_bracketed_block() {
    let d = Daemon::start("zmux-test-host-paste");
    let mut c = Control::open(&d.session);
    let host = Host::attach(&d.session);
    attached(&host, &mut c);
    // Without this the host never brackets a paste in the first place, so it
    // is part of the contract, not a detail: it proves ConPTY passes the
    // request on to whatever terminal is outside.
    assert!(host.wait_to_see(b"\x1b[?2004h", 5), "the client never asked the host for pastes");

    start_cat(&mut c, true);
    host.send(b"\x1b[200~alpha bravo\x1b[201~");
    // Typed, not pasted: canonical mode shows nothing until a newline.
    host.send(b"\r");

    let screen = c
        .wait_at(20, Target::Focused, |t| t.contains("alpha bravo"))
        .expect("the paste never reached the program");
    assert!(
        screen.contains("^[[200~alpha bravo^[[201~"),
        "the paste arrived, but not as one bracketed block:\n{screen}"
    );
}

/// A program that never asked for pastes is typed at, markers and all
/// removed - the daemon's rule, reached now from a real terminal.
#[test]
fn a_host_paste_is_plain_text_to_a_program_that_did_not_ask() {
    let d = Daemon::start("zmux-test-host-plain");
    let mut c = Control::open(&d.session);
    let host = Host::attach(&d.session);
    attached(&host, &mut c);

    start_cat(&mut c, false);
    host.send(b"\x1b[200~charlie delta\x1b[201~\r");

    let screen = c
        .wait_at(20, Target::Focused, |t| t.contains("charlie delta"))
        .expect("the paste never reached the program");
    assert!(!screen.contains("200~"), "markers reached a program that never asked:\n{screen}");
}

/// Typing, the prefix, and a split - the paths the switch away from
/// crossterm rewrote, which must still do what they did.
#[test]
fn typing_and_the_prefix_still_work_through_a_real_console() {
    let d = Daemon::start("zmux-test-host-keys");
    let mut c = Control::open(&d.session);
    let host = Host::attach(&d.session);
    attached(&host, &mut c);
    assert!(c.wait_at(20, Target::Focused, |t| t.contains('$')).is_some(), "no prompt");

    host.send(b"echo TYP''ED\r");
    assert!(
        c.wait_at(20, Target::Focused, |t| t.lines().any(|l| l.trim() == "TYPED")).is_some(),
        "typed input never ran"
    );

    let prefix = zmux::config::Config::load().prefix_byte();
    host.send(&[prefix]);
    host.send(b"%");
    assert!(d.wait_for_panes(2, 20), "the prefix command never split the pane");
}

/// An arrow is forwarded in the form the PANE asked for, which the host
/// cannot know. `cat -v` shows which one arrived.
#[test]
fn arrows_follow_the_panes_cursor_mode() {
    let d = Daemon::start("zmux-test-host-decckm");
    let mut c = Control::open(&d.session);
    let host = Host::attach(&d.session);
    attached(&host, &mut c);

    c.input_at(Target::Focused, "printf '\\033[?1h'; printf 'REA''DY\\n'; cat -v", true);
    assert!(c.wait_at(20, Target::Focused, |t| t.contains("READY")).is_some(), "cat never started");
    // Give the frame carrying the mode time to reach the client.
    std::thread::sleep(Duration::from_millis(500));
    host.send(b"\x1b[A\r");

    let screen = c
        .wait_at(20, Target::Focused, |t| t.contains("^[OA") || t.contains("^[[A"))
        .expect("the arrow never arrived");
    assert!(screen.contains("^[OA"), "application cursor mode was not honoured:\n{screen}");
}

/// The mouse now arrives as SGR reports in the byte stream, not as crossterm
/// events, so a click has to survive the decoder to focus a pane.
#[test]
fn a_host_click_still_focuses_a_pane() {
    let d = Daemon::start("zmux-test-host-click");
    let mut c = Control::open(&d.session);
    let host = Host::attach(&d.session);
    attached(&host, &mut c);

    let prefix = zmux::config::Config::load().prefix_byte();
    host.send(&[prefix]);
    host.send(b"%");
    assert!(d.wait_for_panes(2, 20), "the split never happened");
    assert!(d.wait_for_focus(2, 20), "a new pane takes focus");

    // 1-based, so this is screen (1, 1): the left-hand pane.
    host.send(b"\x1b[<0;2;2M\x1b[<0;2;2m");
    assert!(d.wait_for_focus(1, 20), "clicking the left-hand pane should focus it");
}

/// Alt is an ESC in front of the key. pi's image paste on Windows is Alt+V,
/// read as `ESC v`, and it has to arrive as one keystroke - an ESC delivered
/// on its own would be the Escape key followed by a plain `v`.
#[test]
fn alt_v_reaches_the_program_as_escape_v() {
    let d = Daemon::start("zmux-test-host-altv");
    let mut c = Control::open(&d.session);
    let host = Host::attach(&d.session);
    attached(&host, &mut c);

    start_cat(&mut c, false);
    host.send(b"\x1bv\r");
    let screen = c
        .wait_at(20, Target::Focused, |t| t.contains("^[v"))
        .expect("Alt+V never reached the program");
    assert!(screen.contains("^[v"), "{screen}");
}
