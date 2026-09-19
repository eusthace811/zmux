//! A session: a tiling layout of ConPTY panes, owned by the daemon and
//! **outliving any individual client**.
//!
//! An earlier version owned the pane inside the connection handler, so
//! detaching dropped the ConPTY and killed the shell - persistence in name
//! only. The session now outlives every connection (DESIGN.md §3).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{bail, Result};

use crate::layout::{Dir, Layout, Nav, PaneId, Rect};
use crate::protocol::{clamp_size, Frame, ServerMsg, MAX_INPUT_BYTES, MAX_PANES};
use crate::pty::{Pane, PtyEvent};
use crate::persist::{self, SessionSnapshot};
use crate::render::{composite, frame_from_screen, pane_area};
use crate::term::Term;

pub const SCROLLBACK: usize = 10_000;

pub struct PaneState {
    pub term: Arc<Mutex<Term>>,
    pub pty: Arc<Pane>,
}

/// Bounded so a client that stops draining cannot grow the daemon's memory
/// without limit. When the queue is full the client is dropped: a peer that is
/// this far behind is not usefully attached any more.
const CLIENT_QUEUE: usize = 64;

struct Client {
    id: u64,
    tx: SyncSender<ServerMsg>,
    cols: u16,
    rows: u16,
    /// Set once the client has fallen too far behind to keep.
    lagged: bool,
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
    /// Serialises every geometry mutation.
    ///
    /// apply_size previously read the client list, released the lock, then
    /// resized ConPTY and vt100 separately. Two threads could interleave and
    /// leave the pty at one size and the parser at another. Every path that
    /// changes geometry now holds this for the whole read-compute-apply.
    geometry: Mutex<()>,
    /// Pane lifecycle events, tagged with which pane they came from.
    events_tx: Sender<(PaneId, PtyEvent)>,
}

/// Restore the pane arrangement from disk, if there is a usable one.
///
/// Processes cannot be restored; directories can. Each restored pane gets a
/// fresh shell started in the directory that pane last reported.
fn restore_layout(name: &str) -> Option<(Layout, HashMap<u32, String>)> {
    let snap = persist::load(name)?;
    let layout = Layout::from_saved(snap.root, snap.focus);
    // A corrupt or absurd saved layout must not spawn a shell per leaf.
    if layout.panes().len() > MAX_PANES {
        eprintln!(
            "[session] saved layout has {} panes (max {MAX_PANES}); starting fresh",
            layout.panes().len()
        );
        return None;
    }
    Some((layout, snap.cwds))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

impl Session {
    pub fn new(name: &str, cols: u16, rows: u16) -> Result<(Arc<Self>, Receiver<(PaneId, PtyEvent)>)> {
        let (cols, rows) = clamp_size(cols, rows);
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let restored = restore_layout(name);
        let layout = match &restored {
            Some((l, _)) => l.clone(),
            None => Layout::new(PaneId::alloc()),
        };
        let session = Arc::new(Self {
            name: name.to_string(),
            panes: Mutex::new(HashMap::new()),
            layout: Mutex::new(layout),
            clients: Mutex::new(Vec::new()),
            next_client: AtomicU64::new(1),
            running: Arc::new(AtomicBool::new(true)),
            revision: Arc::new(AtomicU64::new(0)),
            size: Mutex::new((cols, rows)),
            geometry: Mutex::new(()),
            events_tx,
        });
        let area = pane_area(cols, rows);
        let (rects, _) = lock(&session.layout).compute(area);
        let cwds = restored.map(|(_, c)| c).unwrap_or_default();
        for (id, rect) in rects {
            let dir = cwds
                .get(&id.0)
                .map(std::path::PathBuf::from)
                .filter(|p| p.is_dir());
            if let Err(e) = session.spawn_pane_in(id, rect.w, rect.h, dir) {
                // One dead pane must not prevent the session from starting:
                // drop it from the layout and carry on with the rest.
                eprintln!("[session] could not restore pane {}: {e}", id.0);
                let mut l = lock(&session.layout);
                if l.len() > 1 {
                    l.set_focus(id);
                    l.close_focused();
                }
            }
        }
        // A restored layout whose panes all failed leaves nothing to show.
        if lock(&session.panes).is_empty() {
            let id = PaneId::alloc();
            *lock(&session.layout) = Layout::new(id);
            session.spawn_pane(id, area.w, area.h)?;
        }
        session.save_state();
        Ok((session, events_rx))
    }

    /// Persist the layout and each pane's directory.
    ///
    /// Called after any structural change. Failures are logged, never fatal:
    /// not being able to save a layout is no reason to lose a working session.
    pub fn save_state(&self) {
        let (root, focus) = {
            let l = lock(&self.layout);
            (l.root_snapshot(), l.focus())
        };
        let cwds = {
            let panes = lock(&self.panes);
            panes
                .iter()
                .filter_map(|(id, p)| lock(&p.term).cwd_windows().map(|d| (id.0, d)))
                .collect()
        };
        let snap = SessionSnapshot {
            version: persist::FORMAT_VERSION,
            name: self.name.clone(),
            root,
            focus,
            cwds,
        };
        if let Err(e) = persist::save(&snap) {
            eprintln!("[session] could not save layout: {e}");
        }
    }

    /// The focused pane's reported working directory, if the shell has sent
    /// one. New panes start there, as tmux does.
    fn focused_cwd(&self) -> Option<std::path::PathBuf> {
        let id = self.focus();
        let panes = lock(&self.panes);
        let p = panes.get(&id)?;
        let dir = lock(&p.term).cwd_windows()?;
        let path = std::path::PathBuf::from(dir);
        path.is_dir().then_some(path)
    }

    /// Create the pty for `id` and start forwarding its events.
    fn spawn_pane(&self, id: PaneId, cols: u16, rows: u16) -> Result<()> {
        self.spawn_pane_in(id, cols, rows, None)
    }

    fn spawn_pane_in(
        &self,
        id: PaneId,
        cols: u16,
        rows: u16,
        cwd: Option<std::path::PathBuf>,
    ) -> Result<()> {
        let term = Arc::new(Mutex::new(Term::new(cols.max(1), rows.max(1), SCROLLBACK)));
        let feed = Arc::clone(&term);

        // Feed vt100, then answer device queries. Answering is MANDATORY:
        // ConPTY emits ESC[6n at startup and deadlocks if ignored (§2.5).
        let (pty, events) = Pane::spawn_in(cols.max(1), rows.max(1), cwd.as_deref(), move |chunk| {
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
        if data.len() > MAX_INPUT_BYTES {
            bail!("input of {} bytes exceeds MAX_INPUT_BYTES", data.len());
        }
        self.follow_focused();
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
        let _g = lock(&self.geometry);
        if lock(&self.layout).len() >= MAX_PANES {
            return Ok(None);
        }
        let (cols, rows) = self.size();
        let area = pane_area(cols, rows);
        let new_id = PaneId::alloc();
        let accepted = lock(&self.layout).split(dir, area, new_id).is_some();
        if !accepted {
            return Ok(None); // too small; no pty is created for a pane that cannot fit
        }
        let rect = lock(&self.layout).rect_of(new_id, area).unwrap_or(Rect::new(0, 0, 1, 1));
        // Inherit the focused pane's directory, as tmux does.
        let inherit = self.focused_cwd();
        if let Err(e) = self.spawn_pane_in(new_id, rect.w, rect.h, inherit) {
            // Roll the tree back: a leaf with no runtime would render as a
            // dead hole and confuse focus and reflow.
            let mut l = lock(&self.layout);
            l.set_focus(new_id);
            l.close_focused();
            return Err(e);
        }
        self.reflow_locked();
        drop(_g);
        self.save_state();
        Ok(Some(new_id))
    }

    /// Close the focused pane. Returns false when it is the last one.
    pub fn close_focused(&self) -> bool {
        let _g = lock(&self.geometry);
        let removed = { lock(&self.layout).close_focused() };
        match removed {
            Some(id) => {
                // Explicit close so the ConPTY is torn down and its threads
                // joined, rather than relying on drop order (DESIGN.md §2.5).
                if let Some(p) = lock(&self.panes).remove(&id) {
                    p.pty.close();
                }
                self.reflow_locked();
                drop(_g);
                self.save_state();
                true
            }
            None => false,
        }
    }

    /// A pane's process exited: remove it, and end the session if it was last.
    pub fn pane_exited(&self, id: PaneId) -> bool {
        let _g = lock(&self.geometry);
        if lock(&self.layout).len() <= 1 {
            return true;
        }
        {
            let mut l = lock(&self.layout);
            let previous = l.focus();
            l.set_focus(id);
            l.close_focused();
            // Keep the user's focus where it was unless it was the dead pane.
            if previous != id {
                l.set_focus(previous);
            }
        }
        if let Some(p) = lock(&self.panes).remove(&id) {
            p.pty.close();
        }
        self.reflow_locked();
        false
    }

    pub fn focus_dir(&self, nav: Nav) -> bool {
        let (cols, rows) = self.size();
        lock(&self.layout).focus_dir(nav, pane_area(cols, rows))
    }

    pub fn resize_focused(&self, nav: Nav, step: u16) -> bool {
        let _g = lock(&self.geometry);
        let (cols, rows) = self.size();
        let changed = lock(&self.layout).resize_focused(nav, pane_area(cols, rows), step);
        if changed {
            self.reflow_locked();
        }
        changed
    }

    /// Scroll the focused pane's history. `None` snaps back to live output.
    pub fn scroll_focused(&self, lines: Option<i64>) {
        let id = self.focus();
        let panes = lock(&self.panes);
        let Some(p) = panes.get(&id) else { return };
        let mut t = lock(&p.term);
        match lines {
            None => t.scroll_to_bottom(),
            Some(n) if n > 0 => t.scroll_up(n as usize),
            Some(n) => t.scroll_down((-n) as usize),
        }
    }

    /// True when the focused pane is showing history rather than live output.
    pub fn focused_is_scrolled(&self) -> bool {
        let id = self.focus();
        let panes = lock(&self.panes);
        panes.get(&id).is_some_and(|p| lock(&p.term).scroll_offset() > 0)
    }

    /// Typing snaps the view back to live output, as a terminal does.
    pub fn follow_focused(&self) {
        let id = self.focus();
        let panes = lock(&self.panes);
        if let Some(p) = panes.get(&id) {
            lock(&p.term).follow_output();
        }
    }

    pub fn focus_at(&self, x: u16, y: u16) -> bool {
        let (cols, rows) = self.size();
        let hit = lock(&self.layout).pane_at(pane_area(cols, rows), x, y);
        match hit {
            Some(id) => lock(&self.layout).set_focus(id),
            None => false,
        }
    }

    /// Resize every pty to match its current rect.
    ///
    /// Callers must already hold `geometry`; see `reflow()` for the public
    /// entry point.
    fn reflow_locked(&self) {
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

    /// Public reflow: takes the geometry lock, then republishes so attached
    /// clients see the new grid immediately instead of waiting for the next
    /// byte of pty output.
    pub fn reflow(&self) {
        {
            let _g = lock(&self.geometry);
            self.reflow_locked();
        }
        self.publish();
    }

    /// Compose once and send to every client.
    pub fn publish(&self) {
        let frame = self.snapshot();
        self.broadcast(&ServerMsg::Frame(frame));
    }

    // --- clients -----------------------------------------------------------

    fn lock_clients(&self) -> MutexGuard<'_, Vec<Client>> {
        lock(&self.clients)
    }

    /// Register a client. The caller must send it a full snapshot before
    /// forwarding anything from the returned queue.
    pub fn attach(&self, cols: u16, rows: u16) -> (u64, Receiver<ServerMsg>) {
        let (cols, rows) = clamp_size(cols, rows);
        let (tx, rx) = std::sync::mpsc::sync_channel(CLIENT_QUEUE);
        let id = self.next_client.fetch_add(1, Ordering::Relaxed);
        self.lock_clients().push(Client { id, tx, cols, rows, lagged: false });
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
        let (cols, rows) = clamp_size(cols, rows);
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
    ///
    /// Held under `geometry` for the whole read-compute-apply, so two clients
    /// resizing at once cannot leave ConPTY and vt100 disagreeing.
    fn apply_size(&self) {
        let changed = {
            let _g = lock(&self.geometry);
            let next = {
                let cs = self.lock_clients();
                cs.iter()
                    // A minimised or degenerate client would otherwise drag the
                    // session minimum to zero and freeze every other viewport.
                    .filter(|c| c.cols >= crate::protocol::MIN_USABLE_COLS
                             && c.rows >= crate::protocol::MIN_USABLE_ROWS)
                    .map(|c| (c.cols, c.rows))
                    .reduce(|a, b| (a.0.min(b.0), a.1.min(b.1)))
            };
            let Some((cols, rows)) = next else { return }; // no usable client: keep current
            let mut s = lock(&self.size);
            if *s == (cols, rows) {
                false
            } else {
                *s = (cols, rows);
                drop(s);
                self.reflow_locked();
                true
            }
        };
        // Publish outside the geometry lock: a resize is a state change, and
        // without this clients keep the old grid until the pty happens to emit.
        if changed {
            self.publish();
        }
    }

    pub fn status_line(&self) -> String {
        let (n, focus) = {
            let l = lock(&self.layout);
            (l.len(), l.focus().0)
        };
        let scrolled = if self.focused_is_scrolled() { " | SCROLL" } else { "" };
        format!(
            " {} | {} pane{} | pane {}{} ",
            self.name,
            n,
            if n == 1 { "" } else { "s" },
            focus,
            scrolled
        )
    }

    pub fn snapshot(&self) -> Frame {
        let (cols, rows) = self.size();
        let layout = self.layout_snapshot();
        let status = self.status_line();
        let revision = self.revision.load(Ordering::Relaxed);
        let panes = lock(&self.panes);
        let mut f = composite(
            cols,
            rows,
            &layout,
            |id| panes.get(&id).map(|p| frame_from_screen(lock(&p.term).parser.screen())),
            &status,
        );
        f.revision = revision;
        f
    }

    /// Send to every attached client.
    ///
    /// Queues are bounded, so a client that stops draining is DROPPED rather
    /// than allowed to grow the daemon's memory without limit. Losing a
    /// wedged client is strictly better than losing the daemon.
    pub fn broadcast(&self, msg: &ServerMsg) {
        let mut cs = self.lock_clients();
        for c in cs.iter_mut() {
            match c.tx.try_send(msg.clone()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    c.lagged = true;
                    eprintln!("[zmuxd] dropping client {} - {} frames behind", c.id, CLIENT_QUEUE);
                }
                Err(TrySendError::Disconnected(_)) => c.lagged = true,
            }
        }
        cs.retain(|c| !c.lagged);
    }

    /// Stop the session: tell clients, then tear every ConPTY down cleanly.
    ///
    /// Explicit teardown matters on Windows - closing a pseudoconsole without
    /// draining it can hang (DESIGN.md §2.5), and `process::exit` would skip
    /// this entirely.
    pub fn shutdown(&self) {
        if !self.running.swap(false, Ordering::Relaxed) {
            return; // already shutting down
        }
        self.broadcast(&ServerMsg::ShuttingDown);
        let panes: Vec<_> = lock(&self.panes).drain().map(|(_, p)| p).collect();
        for p in panes {
            p.pty.close();
        }
    }
}
