//! Shared harness. Needs no console, so these run from WSL too.

#![allow(dead_code)]

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use zmux::ipc;
use zmux::protocol::{ClientMsg, Frame, ServerMsg, PROTOCOL_VERSION};

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
                let _ = ipc::write_msg(&mut s, &ClientMsg::Shutdown);
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        // Null ALL of the daemon's stdio. Inheriting the harness's pipes means
        // the daemon - and every bash it spawns - holds them open, so cargo
        // waits forever for output that never ends.
        let child = std::process::Command::new(env!("CARGO_BIN_EXE_zmuxd"))
            .arg(session)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
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

    pub fn attach(&self, cols: u16, rows: u16) -> Client {
        Client::connect(&self.session, cols, rows)
    }

    /// Attach and wait for a shell prompt in the first pane.
    pub fn attach_ready(&self, cols: u16, rows: u16) -> Client {
        let mut c = self.attach(cols, rows);
        assert!(c.wait_for(20, |t| t.contains('$')).is_some(), "no shell prompt appeared");
        c
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Ask nicely first so ConPTYs are torn down; killing orphans the shells.
        if ipc::is_running(&self.session) {
            if let Ok(mut s) = ipc::connect(&self.session) {
                let _ = ipc::write_msg(&mut s, &ClientMsg::Shutdown);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// An attached test client.
///
/// Frames are read on a background thread into a channel. Reading inline would
/// block forever whenever the session is idle: the daemon coalesces output and
/// sends nothing when nothing changes, so a deadline checked only between
/// messages is no deadline at all.
pub struct Client {
    stream: interprocess::local_socket::Stream,
    rx: Receiver<ServerMsg>,
}

impl Client {
    pub fn connect(session: &str, cols: u16, rows: u16) -> Self {
        let mut stream = ipc::connect(session).expect("connect");
        ipc::write_msg(&mut stream, &ClientMsg::Hello { version: PROTOCOL_VERSION, cols, rows })
            .expect("hello");
        match ipc::read_msg::<_, ServerMsg>(&mut stream).expect("handshake reply") {
            Some(ServerMsg::Hello { .. }) => {}
            other => panic!("expected Hello, got {other:?}"),
        }
        let mut reader = {
            use interprocess::TryClone as _;
            stream.try_clone().expect("clone stream")
        };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(Some(msg)) = ipc::read_msg::<_, ServerMsg>(&mut reader) {
                if tx.send(msg).is_err() {
                    break;
                }
            }
        });
        Self { stream, rx }
    }

    pub fn send(&mut self, msg: &ClientMsg) {
        ipc::write_msg(&mut self.stream, msg).expect("send");
    }

    pub fn input(&mut self, s: &str) {
        self.send(&ClientMsg::Input { data: s.as_bytes().to_vec() });
    }

    pub fn cmd(&mut self, c: zmux::protocol::Cmd) {
        self.send(&ClientMsg::Command(c));
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.send(&ClientMsg::Resize { cols, rows });
    }

    /// Next message, or `None` if none arrives within `ms`.
    pub fn next_msg(&mut self, ms: u64) -> Option<ServerMsg> {
        match self.rx.recv_timeout(Duration::from_millis(ms)) {
            Ok(m) => Some(m),
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => None,
        }
    }

    /// Collect frames until one satisfies `pred`, or the deadline passes.
    pub fn wait_for(&mut self, secs: u64, pred: impl Fn(&str) -> bool) -> Option<String> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut last = String::new();
        while Instant::now() < deadline {
            match self.rx.recv_timeout(Duration::from_millis(250)) {
                Ok(ServerMsg::Frame(f)) => {
                    last = text_of(&f);
                    if pred(&last) {
                        return Some(last);
                    }
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        let _ = last;
        None
    }

    /// The most recent frame, after letting things settle.
    pub fn current(&mut self, secs: u64) -> Option<String> {
        self.wait_for(secs, |_| true)
    }
}
