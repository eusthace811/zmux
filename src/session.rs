//! A session: one ConPTY plus its terminal state, owned by the daemon and
//! **outliving any individual client**.
//!
//! This is the whole point of the client/daemon split (DESIGN.md §3). An
//! earlier version owned the pane inside the connection handler, so detaching
//! dropped the ConPTY and killed the shell - persistence in name only.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use anyhow::Result;

use crate::protocol::{Frame, ServerMsg};
use crate::pty::{Pane, PtyEvent};
use crate::render::frame_from_screen;
use crate::term::Term;

pub const SCROLLBACK: usize = 10_000;

/// One attached client's outbound queue and its viewport size.
struct Client {
    id: u64,
    tx: Sender<ServerMsg>,
    cols: u16,
    rows: u16,
}

pub struct Session {
    pub name: String,
    pub term: Arc<Mutex<Term>>,
    pub pane: Arc<Pane>,
    clients: Mutex<Vec<Client>>,
    next_id: AtomicU64,
    pub running: Arc<AtomicBool>,
    /// Bumped on every terminal mutation; lets a client tell stale from current.
    pub revision: Arc<AtomicU64>,
}

impl Session {
    pub fn new(name: &str, cols: u16, rows: u16) -> Result<(Arc<Self>, Receiver<PtyEvent>)> {
        let term = Arc::new(Mutex::new(Term::new(cols, rows, SCROLLBACK)));
        let feed = Arc::clone(&term);

        // Feed vt100, then answer device queries. Answering is MANDATORY:
        // ConPTY emits ESC[6n at startup and deadlocks if ignored (§2.5).
        let (pane, events) = Pane::spawn(cols, rows, move |chunk| {
            let mut t = match feed.lock() {
                Ok(t) => t,
                Err(p) => p.into_inner(),
            };
            t.parser.process(chunk);
            let cur = t.cursor();
            t.responder.scan(chunk, cur)
        })?;

        Ok((
            Arc::new(Self {
                name: name.to_string(),
                term,
                pane: Arc::new(pane),
                clients: Mutex::new(Vec::new()),
                next_id: AtomicU64::new(1),
                running: Arc::new(AtomicBool::new(true)),
                revision: Arc::new(AtomicU64::new(0)),
            }),
            events,
        ))
    }

    fn lock_clients(&self) -> std::sync::MutexGuard<'_, Vec<Client>> {
        match self.clients.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    /// Register a client. Returns its id and the queue it should drain.
    pub fn attach(&self, cols: u16, rows: u16) -> (u64, Receiver<ServerMsg>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.lock_clients().push(Client { id, tx, cols, rows });
        self.apply_size();
        (id, rx)
    }

    pub fn detach(&self, id: u64) {
        self.lock_clients().retain(|c| c.id != id);
        self.apply_size();
    }

    pub fn client_count(&self) -> usize {
        self.lock_clients().len()
    }

    pub fn set_client_size(&self, id: u64, cols: u16, rows: u16) {
        {
            let mut cs = self.lock_clients();
            if let Some(c) = cs.iter_mut().find(|c| c.id == id) {
                c.cols = cols;
                c.rows = rows;
            }
        }
        self.apply_size();
    }

    /// Size the pty to the SMALLEST attached viewport, as tmux does: anything
    /// larger and a smaller client sees output it cannot display.
    fn apply_size(&self) {
        let (cols, rows) = {
            let cs = self.lock_clients();
            match cs.iter().map(|c| (c.cols, c.rows)).reduce(|a, b| (a.0.min(b.0), a.1.min(b.1))) {
                Some(v) => v,
                None => return, // no clients: keep the current size
            }
        };
        if cols == 0 || rows == 0 {
            return;
        }
        let _ = self.pane.resize(cols, rows);
        if let Ok(mut t) = self.term.lock() {
            t.resize(cols, rows);
        }
        self.revision.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> Frame {
        let t = match self.term.lock() {
            Ok(t) => t,
            Err(p) => p.into_inner(),
        };
        frame_from_screen(t.parser.screen())
    }

    /// Send to every attached client, dropping any whose receiver is gone.
    pub fn broadcast(&self, msg: &ServerMsg) {
        self.lock_clients().retain(|c| c.tx.send(msg.clone()).is_ok());
    }

    pub fn shutdown(&self) {
        self.running.store(false, Ordering::Relaxed);
        self.broadcast(&ServerMsg::ShuttingDown);
    }
}
