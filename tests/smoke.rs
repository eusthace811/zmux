//! End-to-end: spawn the daemon, attach as a client, drive a real Git Bash,
//! and read the rendered grid back.
//!
//! Needs no console, so it runs anywhere - including from WSL against the
//! Windows binaries.

use std::time::{Duration, Instant};

use zmux::ipc;
use zmux::protocol::{ClientMsg, Frame, ServerMsg, PROTOCOL_VERSION};

fn text_of(f: &Frame) -> String {
    let mut s = String::new();
    for row in 0..f.rows {
        for col in 0..f.cols {
            s.push(f.cells[row as usize * f.cols as usize + col as usize].ch);
        }
        s.push('\n');
    }
    s
}

struct Daemon {
    child: std::process::Child,
    session: String,
}

impl Daemon {
    fn start(session: &str) -> Self {
        let child = std::process::Command::new(env!("CARGO_BIN_EXE_zmuxd"))
            .arg(session)
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn zmuxd");
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline && !ipc::is_running(session) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(ipc::is_running(session), "daemon never started listening");
        Self { child, session: session.into() }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Collect frames until `pred` matches or we time out.
fn wait_for(
    stream: &mut interprocess::local_socket::Stream,
    secs: u64,
    pred: impl Fn(&str) -> bool,
) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        match ipc::read_msg::<_, ServerMsg>(stream) {
            Ok(Some(ServerMsg::Frame(f))) => {
                let t = text_of(&f);
                if pred(&t) {
                    return Some(t);
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => return None,
            Err(_) => return None,
        }
    }
    None
}

#[test]
fn daemon_runs_git_bash_and_renders_output() {
    let d = Daemon::start("zmux-test-echo");
    let mut s = ipc::connect(&d.session).expect("connect");

    ipc::write_msg(&mut s, &ClientMsg::Hello { version: PROTOCOL_VERSION, cols: 80, rows: 24 })
        .expect("hello");
    match ipc::read_msg::<_, ServerMsg>(&mut s).expect("reply") {
        Some(ServerMsg::Hello { version, .. }) => assert_eq!(version, PROTOCOL_VERSION),
        other => panic!("expected Hello, got {other:?}"),
    }

    // A prompt proves ConPTY started bash AND that the startup DSR-CPR
    // handshake was answered - without the reply the shell never initialises
    // and this hangs (DESIGN.md §2.5).
    assert!(
        wait_for(&mut s, 20, |t| t.contains('$')).is_some(),
        "no shell prompt appeared - CPR handshake likely unanswered"
    );

    ipc::write_msg(&mut s, &ClientMsg::Input { data: b"echo ZMUX_MARKER_OK\r".to_vec() })
        .expect("send input");

    let screen = wait_for(&mut s, 20, |t| t.matches("ZMUX_MARKER_OK").count() >= 2)
        .expect("command output never appeared");
    // Twice: once echoed by the shell, once as the command's output.
    assert!(screen.matches("ZMUX_MARKER_OK").count() >= 2, "screen was:\n{screen}");
}

#[test]
fn rejects_a_mismatched_protocol_version() {
    let d = Daemon::start("zmux-test-version");
    let mut s = ipc::connect(&d.session).expect("connect");

    ipc::write_msg(&mut s, &ClientMsg::Hello { version: 9999, cols: 80, rows: 24 }).expect("hello");
    match ipc::read_msg::<_, ServerMsg>(&mut s).expect("reply") {
        Some(ServerMsg::Refused { reason }) => assert!(reason.contains("9999"), "{reason}"),
        other => panic!("expected Refused, got {other:?}"),
    }
}
