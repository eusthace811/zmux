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
    /// When false, saved layout is cleared on start and drop so tests do not
    /// inherit each other's state. Restart tests set it true deliberately.
    preserve_state: bool,
}

impl Daemon {
    /// Start with a clean slate: any saved layout for this name is discarded.
    ///
    /// Persistence is real, so a layout saved by an EARLIER RUN would be
    /// restored and the test would start from someone else's state. Not
    /// hypothetical: it turned every split test red once the saved layouts had
    /// grown to several panes.
    pub fn start(session: &str) -> Self {
        zmux::persist::remove(session);
        Self::spawn(session, false)
    }

    /// Start WITHOUT clearing saved state, for tests that assert persistence
    /// across a restart.
    pub fn start_preserving_state(session: &str) -> Self {
        Self::spawn(session, true)
    }

    fn spawn(session: &str, preserve_state: bool) -> Self {
        // A leftover daemon from a previous run would silently serve the test.
        // The daemon now refuses to start when one is already listening, so
        // wait for the old one to actually go rather than racing it.
        if ipc::is_running(session) {
            if let Ok(mut s) = ipc::connect(session) {
                let _ = ipc::write_msg(&mut s, &ClientMsg::Shutdown);
            }
            let gone = Instant::now() + Duration::from_secs(10);
            while Instant::now() < gone && ipc::is_running(session) {
                std::thread::sleep(Duration::from_millis(100));
            }
            assert!(!ipc::is_running(session), "a previous daemon for {session} would not exit");
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
        Self { child, session: session.into(), preserve_state }
    }

    /// Wait until the current window holds `n` panes, asking the DAEMON.
    ///
    /// Tests used to match the status bar - `t.contains("2 panes")` - which
    /// asserts a layout fact through a spelling. The bar is presentation and
    /// has now changed shape; `info` is the thing that actually knows. A
    /// control connection is not registered as a client, so polling one does
    /// not disturb the session's size or its idle timer.
    /// How many panes the current window holds, right now.
    ///
    /// `info().panes` spans every window, so this filters. A test that means
    /// "the visible layout" must say so, or creating a window silently
    /// changes what it is asserting.
    pub fn panes(&self) -> usize {
        panes_in_current(&Control::open(&self.session).info())
    }

    /// Every pane in the session, whichever window shows it.
    pub fn all_panes(&self) -> usize {
        Control::open(&self.session).info().panes.len()
    }

    pub fn wait_for_panes(&self, n: usize, secs: u64) -> bool {
        self.wait_until(secs, |i| panes_in_current(i) == n)
    }


    /// Wait until the session is on window `index` of `of` windows.
    pub fn wait_for_window(&self, index: usize, of: usize, secs: u64) -> bool {
        self.wait_until(secs, |i| i.window == index && i.windows == of)
    }

    /// Wait until the focused pane is at `position` in the current window.
    pub fn wait_for_focus(&self, position: usize, secs: u64) -> bool {
        self.wait_until(secs, |i| i.focus == position)
    }

    fn wait_until(
        &self,
        secs: u64,
        pred: impl Fn(&zmux::protocol::SessionInfo) -> bool,
    ) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        loop {
            if pred(&Control::open(&self.session).info()) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
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
        let session = self.session.clone();
        let preserve = self.preserve_state;
        // Ask nicely first so ConPTYs are torn down; killing orphans the shells.
        if ipc::is_running(&self.session) {
            if let Ok(mut s) = ipc::connect(&self.session) {
                let _ = ipc::write_msg(&mut s, &ClientMsg::Shutdown);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    if !preserve {
                        zmux::persist::remove(&session);
                    }
                    return;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => break,
            }
        }
        // Reaching here means the daemon ignored a clean Shutdown for five
        // seconds. That is a teardown bug, so make it loud rather than
        // quietly killing the process and letting the suite stay green.
        eprintln!(
            "WARNING: daemon for session {session} did not exit on Shutdown; killing it. \
             This hides teardown deadlocks - investigate."
        );
        let _ = self.child.kill();
        let _ = self.child.wait();
        if !preserve {
            zmux::persist::remove(&session);
        }
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
    /// Reassembled view, so tests see whole screens whether the daemon sent a
    /// full frame or just the changed rows.
    frame: Option<Frame>,
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
        Self { stream, rx, frame: None }
    }

    /// Fold a message into the running view. Returns the screen if it changed.
    fn absorb(&mut self, msg: ServerMsg) -> Option<String> {
        match msg {
            ServerMsg::Frame(f) => {
                let t = text_of(&f);
                self.frame = Some(f);
                Some(t)
            }
            ServerMsg::Delta(d) => {
                let f = self.frame.as_mut()?;
                f.apply_delta(&d).then(|| text_of(f))
            }
            _ => None,
        }
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
    ///
    /// Checks the screen we ALREADY hold before blocking. The daemon sends
    /// nothing while nothing changes, so once a screen has settled there is no
    /// further frame to absorb - and waiting for a condition that is already
    /// true waits the full deadline and then fails. That is not hypothetical:
    /// it made the daemon-restart test fail under load and pass in isolation,
    /// purely because a slower restore let both restored shells finish drawing
    /// before the client attached.
    pub fn wait_for(&mut self, secs: u64, pred: impl Fn(&str) -> bool) -> Option<String> {
        if let Some(now) = self.frame.as_ref().map(text_of) {
            if pred(&now) {
                return Some(now);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            match self.rx.recv_timeout(Duration::from_millis(250)) {
                Ok(msg) => match self.absorb(msg) {
                    Some(screen) if pred(&screen) => return Some(screen),
                    _ => {}
                },
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        None
    }

    /// Wait until the daemon reports that the focused pane's program does
    /// (or does not) want the mouse.
    ///
    /// This is the flag the real client uses to decide whether to hold the
    /// host terminal's mouse, so a test that asserts on it is asserting on
    /// exactly what a user would see happen.
    pub fn wait_for_mouse(&mut self, secs: u64, want: bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if self.frame.as_ref().is_some_and(|f| f.mouse == want) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            match self.rx.recv_timeout(Duration::from_millis(250)) {
                Ok(msg) => {
                    self.absorb(msg);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return false,
            }
        }
    }

    /// The most recent screen, after letting updates settle.
    ///
    /// Drains everything currently queued rather than returning the first
    /// message to arrive - the previous version returned the OLDEST pending
    /// update while claiming to be the newest.
    pub fn current(&mut self, secs: u64) -> Option<String> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut latest = None;
        // Take at least one update, then drain whatever else is already there.
        while Instant::now() < deadline {
            match self.rx.recv_timeout(Duration::from_millis(300)) {
                Ok(msg) => {
                    if let Some(s) = self.absorb(msg) {
                        latest = Some(s);
                    }
                }
                Err(_) => break,
            }
        }
        latest.or_else(|| self.frame.as_ref().map(text_of))
    }
}

/// A control connection: request in, one reply out, no frames.
/// Panes of the window that is on screen.
pub fn panes_in_current(i: &zmux::protocol::SessionInfo) -> usize {
    i.panes.iter().filter(|p| p.window == i.window).count()
}

pub struct Control {
    stream: interprocess::local_socket::Stream,
}

impl Control {
    pub fn open(session: &str) -> Self {
        let mut stream = ipc::connect(session).expect("connect");
        ipc::write_msg(&mut stream, &ClientMsg::Control { version: PROTOCOL_VERSION })
            .expect("control hello");
        match ipc::read_msg::<_, ServerMsg>(&mut stream).expect("handshake reply") {
            Some(ServerMsg::Hello { .. }) => {}
            other => panic!("expected Hello, got {other:?}"),
        }
        Self { stream }
    }

    pub fn ask(&mut self, msg: &ClientMsg) -> ServerMsg {
        ipc::write_msg(&mut self.stream, msg).expect("request");
        ipc::read_msg::<_, ServerMsg>(&mut self.stream)
            .expect("reply")
            .expect("the daemon closed the control connection")
    }

    pub fn info(&mut self) -> zmux::protocol::SessionInfo {
        match self.ask(&ClientMsg::Query(zmux::protocol::Query::Info)) {
            ServerMsg::Info(i) => i,
            other => panic!("expected Info, got {other:?}"),
        }
    }

    pub fn capture(&mut self, pane: Option<u8>) -> String {
        match self.ask(&ClientMsg::Query(zmux::protocol::Query::Capture { pane, lines: 0 })) {
            ServerMsg::Capture { text } => text,
            other => panic!("expected Capture, got {other:?}"),
        }
    }

    /// Capture by `Target`, which unlike a position can reach a pane in a
    /// window that is not on screen.
    pub fn capture_at(&mut self, target: zmux::protocol::Target) -> String {
        let q = zmux::protocol::Query::CaptureAt { target, lines: 0 };
        match self.ask(&ClientMsg::Query(q)) {
            ServerMsg::Capture { text } => text,
            other => panic!("expected Capture, got {other:?}"),
        }
    }

    /// Type into the pane `target` names, without moving anybody's focus.
    pub fn input_at(&mut self, target: zmux::protocol::Target, text: &str, enter: bool) {
        let msg = ClientMsg::InputAt {
            target,
            data: text.as_bytes().to_vec(),
            paste: false,
            enter,
        };
        match self.ask(&msg) {
            ServerMsg::Info(_) => {}
            other => panic!("input was refused: {other:?}"),
        }
    }

    /// Poll a target's screen until it matches.
    pub fn wait_at(
        &mut self,
        secs: u64,
        target: zmux::protocol::Target,
        pred: impl Fn(&str) -> bool,
    ) -> Option<String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        loop {
            let text = self.capture_at(target.clone());
            if pred(&text) {
                return Some(text);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    /// Poll `capture` until it matches, or give up after `secs`.
    pub fn wait_for(&mut self, secs: u64, pane: Option<u8>, pred: impl Fn(&str) -> bool) -> Option<String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        loop {
            let text = self.capture(pane);
            if pred(&text) {
                return Some(text);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}
