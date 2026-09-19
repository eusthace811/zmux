//! A session: a tiling layout of ConPTY panes, owned by the daemon and
//! **outliving any individual client**.
//!
//! An earlier version owned the pane inside the connection handler, so
//! detaching dropped the ConPTY and killed the shell - persistence in name
//! only. The session now outlives every connection (DESIGN.md §3).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{bail, Result};

use crate::layout::{Dir, Layout, Nav, PaneId, Rect};
use crate::protocol::{Frame, ServerMsg};
use crate::pty::{Pane, PtyEvent};
use crate::render::{composite, frame_from_screen, pane_area};
use crate::term::Term;

pub const SCROLLBACK: usize = 10_000;

pub struct PaneState {
    pub term: Arc<Mutex<Term>>,
    pub pty: Arc<Pane>,
}

struct Client {
    id: u64,
    tx: Sender<ServerMsg>,
    cols: u16,
    rows: u16,
}

pub struct Session {
    pub name: String,
    panes: Mutex<HashMap<PaneId, PaneState>>,
    layout: Mutex<Layout>,
    clients: Mutex<Vec<Client>>,
    next_client: AtomicU64,
    pub running: Arc<AtomicBool>,
    pub revision: Arc<AtomicU64>,
    /// Current composited size, i.e. the smallest attached viewport.
    size: Mutex<(u16, u16)>,
    /// Pane lifecycle events, tagged with which pane they came from.
    events_tx: Sender<(PaneId, PtyEvent)>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

impl Session {
    pub fn new(name: &str, cols: u16, rows: u16) -> Result<(Arc<Self>, Receiver<(PaneId, PtyEvent)>)> {
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let first = PaneId::alloc();
        let session = Arc::new(Self {
            name: name.to_string(),
            panes: Mutex::new(HashMap::new()),
            layout: Mutex::new(Layout::new(first)),
            clients: Mutex::new(Vec::new()),
            next_client: AtomicU64::new(1),
            running: Arc::new(AtomicBool::new(true)),
            revision: Arc::new(AtomicU64::new(0)),
            size: Mutex::new((cols, rows)),
            events_tx,
        });
        let area = pane_area(cols, rows);
        session.spawn_pane(first, area.w, area.h)?;
        Ok((session, events_rx))
    }

    /// Create the pty for `id` and start forwarding its events.
    fn spawn_pane(&self, id: PaneId, cols: u16, rows: u16) -> Result<()> {
        let term = Arc::new(Mutex::new(Term::new(cols.max(1), rows.max(1), SCROLLBACK)));
        let feed = Arc::clone(&term);

        // Feed vt100, then answer device queries. Answering is MANDATORY:
        // ConPTY emits ESC[6n at startup and deadlocks if ignored (§2.5).
        let (pty, events) = Pane::spawn(cols.max(1), rows.max(1), move |chunk| {
            let mut t = lock(&feed);
            t.parser.process(chunk);
            let cur = t.cursor();
            t.responder.scan(chunk, cur)
        })?;

        lock(&self.panes).insert(id, PaneState { term, pty: Arc::new(pty) });

        // Tag this pane's events so the pump can tell them apart.
        let tx = self.events_tx.clone();
        std::thread::spawn(move || {
            while let Ok(ev) = events.recv() {
                let exited = matches!(ev, PtyEvent::Exited(_));
                if tx.send((id, ev)).is_err() || exited {
                    break;
                }
            }
        });
        Ok(())
    }

    pub fn focus(&self) -> PaneId {
        lock(&self.layout).focus()
    }

    pub fn pane_count(&self) -> usize {
        lock(&self.panes).len()
    }

    pub fn size(&self) -> (u16, u16) {
        *lock(&self.size)
    }

    pub fn layout_snapshot(&self) -> Layout {
        lock(&self.layout).clone()
    }

    /// Write to the focused pane.
    pub fn write_focused(&self, data: &[u8]) -> Result<()> {
        let id = self.focus();
        let pty = {
            let panes = lock(&self.panes);
            match panes.get(&id) {
                Some(p) => Arc::clone(&p.pty),
                None => bail!("focused pane {id:?} has no pty"),
            }
        };
        // Lock released before writing: a blocked write must not stall the
        // whole session.
        pty.write_input(data)
    }

    pub fn split(&self, dir: Dir) -> Result<Option<PaneId>> {
        let (cols, rows) = self.size();
        let area = pane_area(cols, rows);
        let new_id = PaneId::alloc();
        let accepted = lock(&self.layout).split(dir, area, new_id).is_some();
        if !accepted {
            return Ok(None); // too small; no pty is created for a pane that cannot fit
        }
        let rect = lock(&self.layout).rect_of(new_id, area).unwrap_or(Rect::new(0, 0, 1, 1));
        self.spawn_pane(new_id, rect.w, rect.h)?;
        self.reflow();
        Ok(Some(new_id))
    }

    /// Close the focused pane. Returns false when it is the last one.
    pub fn close_focused(&self) -> bool {
        let removed = { lock(&self.layout).close_focused() };
        match removed {
            Some(id) => {
                lock(&self.panes).remove(&id); // drops the pty, closing the ConPTY
                self.reflow();
                true
            }
            None => false,
        }
    }

    /// A pane's process exited: remove it, and end the session if it was last.
    pub fn pane_exited(&self, id: PaneId) -> bool {
        let is_last = lock(&self.layout).len() <= 1;
        if is_last {
            return true;
        }
        {
            let mut l = lock(&self.layout);
            l.set_focus(id);
            l.close_focused();
        }
        lock(&self.panes).remove(&id);
        self.reflow();
        false
    }

    pub fn focus_dir(&self, nav: Nav) -> bool {
        let (cols, rows) = self.size();
        lock(&self.layout).focus_dir(nav, pane_area(cols, rows))
    }

    pub fn resize_focused(&self, nav: Nav, step: u16) -> bool {
        let (cols, rows) = self.size();
        let changed = lock(&self.layout).resize_focused(nav, pane_area(cols, rows), step);
        if changed {
            self.reflow();
        }
        changed
    }

    pub fn focus_at(&self, x: u16, y: u16) -> bool {
        let (cols, rows) = self.size();
        let hit = lock(&self.layout).pane_at(pane_area(cols, rows), x, y);
        match hit {
            Some(id) => lock(&self.layout).set_focus(id),
            None => false,
        }
    }

    /// Resize every pty to match its current rect. Called whenever geometry
    /// changes: split, close, divider move, or client resize.
    pub fn reflow(&self) {
        let (cols, rows) = self.size();
        let area = pane_area(cols, rows);
        let (rects, _) = lock(&self.layout).compute(area);
        let panes = lock(&self.panes);
        for (id, r) in rects {
            if let Some(p) = panes.get(&id) {
                if r.w > 0 && r.h > 0 {
                    let _ = p.pty.resize(r.w, r.h);
                    lock(&p.term).resize(r.w, r.h);
                }
            }
        }
        drop(panes);
        self.revision.fetch_add(1, Ordering::Relaxed);
    }

    // --- clients -----------------------------------------------------------

    fn lock_clients(&self) -> MutexGuard<'_, Vec<Client>> {
        lock(&self.clients)
    }

    pub fn attach(&self, cols: u16, rows: u16) -> (u64, Receiver<ServerMsg>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let id = self.next_client.fetch_add(1, Ordering::Relaxed);
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

    /// Size to the SMALLEST attached viewport, as tmux does: anything larger
    /// and a smaller client sees output it cannot display.
    fn apply_size(&self) {
        let next = {
            let cs = self.lock_clients();
            cs.iter().map(|c| (c.cols, c.rows)).reduce(|a, b| (a.0.min(b.0), a.1.min(b.1)))
        };
        let Some((cols, rows)) = next else { return }; // no clients: keep current
        if cols == 0 || rows < 2 {
            return;
        }
        {
            let mut s = lock(&self.size);
            if *s == (cols, rows) {
                return;
            }
            *s = (cols, rows);
        }
        self.reflow();
    }

    pub fn status_line(&self) -> String {
        let l = lock(&self.layout);
        format!(
            " {} | {} pane{} | pane {} | C-b ? help ",
            self.name,
            l.len(),
            if l.len() == 1 { "" } else { "s" },
            l.focus().0
        )
    }

    pub fn snapshot(&self) -> Frame {
        let (cols, rows) = self.size();
        let layout = self.layout_snapshot();
        let status = self.status_line();
        let panes = lock(&self.panes);
        composite(
            cols,
            rows,
            &layout,
            |id| panes.get(&id).map(|p| frame_from_screen(lock(&p.term).parser.screen())),
            &status,
        )
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
