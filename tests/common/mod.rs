//! Shared harness. Needs no console, so these run from WSL too.

use std::time::{Duration, Instant};

use zmux::ipc;
use zmux::protocol::{Frame, ServerMsg};

pub fn text_of(f: &Frame) -> String {
    let mut s = String::new();
    for row in 0..f.rows {
        for col in 0..f.cols {
            s.push(f.cells[row as usize * f.cols as usize + col as usize].ch);
        }
        s.push('\n');
    }
    s
}

pub struct Daemon {
    child: std::process::Child,
    pub session: String,
}

impl Daemon {
    pub fn start(session: &str) -> Self {
        // A leftover daemon from a previous run would silently serve the test.
        if ipc::is_running(session) {
            if let Ok(mut s) = ipc::connect(session) {
                let _ = ipc::write_msg(&mut s, &zmux::protocol::ClientMsg::Shutdown);
            }
            std::thread::sleep(Duration::from_millis(300));
        }
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

pub fn wait_for(
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
            Ok(None) | Err(_) => return None,
        }
    }
    None
}
