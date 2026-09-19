//! A session: a tiling layout of ConPTY panes, owned by the daemon and
//! **outliving any individual client**.
//!
//! An earlier version owned the pane inside the connection handler, so
//! detaching dropped the ConPTY and killed the shell - persistence in name
//! only. The session now outlives every connection (DESIGN.md §3).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{bail, Result};

use crate::layout::{Dir, Layout, Nav, PaneId, Rect};
use crate::protocol::{clamp_size, Frame, ServerMsg, MAX_INPUT_BYTES, MAX_PANES};
use crate::pty::{Pane, PtyEvent};
use crate::persist::{self, SessionSnapshot};
use crate::render::{composite, frame_from_screen, pane_area_with};
use crate::term::Term;

use crate::config::Config;

/// A live session plus the channel its panes report on.
pub type SessionHandle = (Arc<Session>, Receiver<(PaneId, PtyEvent)>);

pub struct PaneState {
    pub term: Arc<Mutex<Term>>,
    pub pty: Arc<Pane>,
}

/// Control messages only. Frames do not queue - see `Outbound`.
const CLIENT_QUEUE: usize = 32;

/// Bounded for the same reason as the per-pane queue.
const EVENT_QUEUE: usize = 512;

/// What a connection's writer thread is told to do.
///
/// Frames are COALESCED rather than queued: only the newest matters, so the
/// session keeps one slot per client and the channel carries a wake-up marker.
/// The previous design queued 64 frames and dropped any client that fell
/// behind - about one second at 16ms pacing, which a momentarily busy client
/// exceeds easily. That disconnected healthy clients and silently discarded
/// the commands they had already sent.
pub enum Outbound {
    /// A newer frame is waiting in the client's slot.
    FrameReady,
    /// Never coalesced; losing one of these loses the protocol.
    Control(ServerMsg),
}

struct Client {
    id: u64,
    tx: SyncSender<Outbound>,
    /// Newest frame not yet written. Replaced, never accumulated.
    latest: Arc<Mutex<Option<ServerMsg>>>,
    cols: u16,
    rows: u16,
    /// Set once the client has fallen too far behind to keep.
    lagged: bool,
    /// A newly attached client, or one whose delta did not apply, needs a
    /// whole frame before deltas mean anything.
    needs_full: bool,
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
    /// Last frame sent to clients, as the base for deltas.
    last_published: Mutex<Option<Frame>>,
    /// Set when a client was dropped mid-broadcast; the pump recomputes size
    /// outside the broadcast path, where taking the geometry lock is safe.
    size_dirty: std::sync::atomic::AtomicBool,
    /// Serialises every geometry mutation.
    ///
    /// apply_size previously read the client list, released the lock, then
    /// resized ConPTY and vt100 separately. Two threads could interleave and
    /// leave the pty at one size and the parser at another. Every path that
    /// changes geometry now holds this for the whole read-compute-apply.
    geometry: Mutex<()>,
    /// Pane lifecycle events, tagged with which pane they came from.
    events_tx: SyncSender<(PaneId, PtyEvent)>,
    pub cfg: Config,
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
    pub fn new(name: &str, cols: u16, rows: u16) -> Result<SessionHandle> {
        let (cols, rows) = clamp_size(cols, rows);
        // Bounded: forwarding each pane's bounded queue into an unbounded one
        // would defeat the bound entirely.
        let (events_tx, events_rx) = std::sync::mpsc::sync_channel(EVENT_QUEUE);
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
            last_published: Mutex::new(None),
            size_dirty: std::sync::atomic::AtomicBool::new(false),
            geometry: Mutex::new(()),
            cfg: Config::load(),
            events_tx,
        });
        let area = session.area(cols, rows);
        let (rects, _) = lock(&session.layout).compute(area);
        let cwds = restored.map(|(_, c)| c).unwrap_or_default();
        for (id, rect) in rects {
            let dir = cwds
                .get(&id.0)
                .filter(|d| crate::cwd::is_safe_local_dir(d))
                .map(std::path::PathBuf::from)
                .filter(|p| p.is_dir());
            if let Err(e) = session.spawn_pane_retrying(id, rect.w, rect.h, dir) {
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
        // Capture layout and panes together. Taking them under separate locks
        // allowed a split to land in between, producing a snapshot whose tree
        // and directories described different moments.
        let (root, focus, cwds) = {
            let l = lock(&self.layout);
            let panes = lock(&self.panes);
            let cwds = panes
                .iter()
                .filter_map(|(id, p)| lock(&p.term).cwd_windows().map(|d| (id.0, d)))
                .collect();
            (l.root_snapshot(), l.focus(), cwds)
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
        // Check the STRING before touching the filesystem: is_dir() on a UNC
        // path makes Windows negotiate SMB with a host the pane chose.
        if !crate::cwd::is_safe_local_dir(&dir) {
            return None;
        }
        let path = std::path::PathBuf::from(dir);
        path.is_dir().then_some(path)
    }

    /// Create the pty for `id` and start forwarding its events.
    fn spawn_pane(&self, id: PaneId, cols: u16, rows: u16) -> Result<()> {
        self.spawn_pane_in(id, cols, rows, None)
    }

    /// Spawn a pane, retrying a transient failure.
    ///
    /// Creating a ConPTY and a process can fail under resource pressure. The
    /// first version gave up immediately and dropped the pane, which meant a
    /// session restore could silently come back with fewer panes than it had -
    /// observed under parallel load. Losing a pane is worth a couple of
    /// retries before accepting.
    fn spawn_pane_retrying(
        &self,
        id: PaneId,
        cols: u16,
        rows: u16,
        cwd: Option<std::path::PathBuf>,
    ) -> Result<()> {
        const ATTEMPTS: usize = 3;
        let mut last = None;
        for attempt in 1..=ATTEMPTS {
            match self.spawn_pane_in(id, cols, rows, cwd.clone()) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    eprintln!("[session] pane {} spawn attempt {attempt}/{ATTEMPTS} failed: {e}", id.0);
                    last = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(200 * attempt as u64));
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("pane {} could not be created", id.0)))
    }

    fn spawn_pane_in(
        &self,
        id: PaneId,
        cols: u16,
        rows: u16,
        cwd: Option<std::path::PathBuf>,
    ) -> Result<()> {
        let term = Arc::new(Mutex::new(Term::new(cols.max(1), rows.max(1), self.cfg.scrollback)));
        let feed = Arc::clone(&term);

        // Feed vt100, then answer device queries. Answering is MANDATORY:
        // ConPTY emits ESC[6n at startup and deadlocks if ignored (§2.5).
        let (pty, events) = Pane::spawn_with(cols.max(1), rows.max(1), cwd.as_deref(), &self.cfg, move |chunk| {
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

    fn area(&self, cols: u16, rows: u16) -> Rect {
        pane_area_with(cols, rows, self.cfg.status_bar)
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
        let area = self.area(cols, rows);
        let new_id = PaneId::alloc();
        let accepted = lock(&self.layout).split(dir, area, new_id).is_some();
        if !accepted {
            eprintln!("[session] split refused: area {}x{} too small", area.w, area.h);
            return Ok(None); // too small; no pty is created for a pane that cannot fit
        }
        let rect = lock(&self.layout).rect_of(new_id, area).unwrap_or(Rect::new(0, 0, 1, 1));
        // Inherit the focused pane's directory, as tmux does.
        let inherit = self.focused_cwd();
        if let Err(e) = self.spawn_pane_retrying(new_id, rect.w, rect.h, inherit) {
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
                let pane = lock(&self.panes).remove(&id);
                self.reflow_locked();
                drop(_g);
                // Close OUTSIDE the geometry lock. ClosePseudoConsole can hang
                // on pre-24H2 Windows, and holding `geometry` across it would
                // freeze every layout and resize operation in the session.
                if let Some(p) = pane {
                    std::thread::spawn(move || p.pty.close());
                }
                self.save_state();
                true
            }
            None => false,
        }
    }

    /// A pane's process exited: remove it, and end the session if it was last.
    pub fn pane_exited(&self, id: PaneId) -> bool {
        let _g = lock(&self.geometry);
        // A manual close can race an already-queued Exited for the same pane.
        // Without this check set_focus(id) fails, its result is ignored, and
        // close_focused() removes whichever pane happens to be FOCUSED -
        // killing a live pane and orphaning it in the pane map.
        if !lock(&self.layout).panes().contains(&id) {
            lock(&self.panes).remove(&id);
            return false;
        }
        if lock(&self.layout).len() <= 1 {
            return true;
        }
        {
            let mut l = lock(&self.layout);
            let previous = l.focus();
            if !l.set_focus(id) {
                return false; // vanished between the check and here
            }
            l.close_focused();
            // Keep the user's focus where it was unless it was the dead pane.
            if previous != id {
                l.set_focus(previous);
            }
        }
        let pane = lock(&self.panes).remove(&id);
        self.reflow_locked();
        drop(_g);
        if let Some(p) = pane {
            std::thread::spawn(move || p.pty.close());
        }
        false
    }

    pub fn focus_dir(&self, nav: Nav) -> bool {
        let (cols, rows) = self.size();
        let changed = lock(&self.layout).focus_dir(nav, self.area(cols, rows));
        if changed {
            self.revision.fetch_add(1, Ordering::Relaxed);
        }
        changed
    }

    pub fn resize_focused(&self, nav: Nav, step: u16) -> bool {
        let _g = lock(&self.geometry);
        let (cols, rows) = self.size();
        let changed = lock(&self.layout).resize_focused(nav, self.area(cols, rows), step);
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
        drop(t);
        drop(panes);
        self.revision.fetch_add(1, Ordering::Relaxed);
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
        let hit = lock(&self.layout).pane_at(self.area(cols, rows), x, y);
        match hit {
            Some(id) => {
                let changed = lock(&self.layout).set_focus(id);
                if changed {
                    self.revision.fetch_add(1, Ordering::Relaxed);
                }
                changed
            }
            None => false,
        }
    }

    /// Resize every pty to match its current rect.
    ///
    /// Callers must already hold `geometry`; see `reflow()` for the public
    /// entry point.
    fn reflow_locked(&self) {
        let (cols, rows) = self.size();
        let area = self.area(cols, rows);
        let (rects, _) = lock(&self.layout).compute(area);
        let panes = lock(&self.panes);
        for (id, r) in rects {
            if let Some(p) = panes.get(&id) {
                if r.w > 0 && r.h > 0 {
                    // Only resize the parser if the pty accepted the new size.
                    // Resizing it regardless left vt100 and ConPTY disagreeing
                    // about the grid, which renders as misplaced output.
                    match p.pty.resize(r.w, r.h) {
                        Ok(()) => lock(&p.term).resize(r.w, r.h),
                        Err(e) => eprintln!("[session] pane {} resize failed: {e}", id.0),
                    }
                }
            }
        }
        drop(panes);
        self.revision.fetch_add(1, Ordering::Relaxed);
        // Geometry changed, so no delta can describe the transition.
        *lock(&self.last_published) = None;
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

    /// Compose once, then send each client either a full frame or just the
    /// rows that changed.
    ///
    /// Typing changes one or two rows out of thirty, so a full grid per tick is
    /// almost entirely waste.
    pub fn publish(&self) {
        let frame = self.snapshot();
        // Hold `last_published` across derive-send-update. Releasing it between
        // steps let two publishers derive deltas from the SAME base and then
        // update that base out of order, so a client could receive a delta
        // whose base it had already moved past.
        let mut last = lock(&self.last_published);
        let msg_delta = last.as_ref().and_then(|prev| frame.delta_from(prev)).map(ServerMsg::Delta);
        let full = ServerMsg::Frame(frame.clone());

        {
            let mut cs = self.lock_clients();
            for c in cs.iter_mut() {
                let mut slot = lock(&c.latest);
                // If the slot still holds a frame, the writer has not taken it
                // and that frame will NEVER be delivered - we are about to
                // overwrite it. Any delta we send now would be based on a
                // state the client never saw, and it would be refused forever
                // because nothing asks for a resync. Coalescing therefore
                // forces a FULL frame, which is self-contained.
                let coalescing = slot.is_some();
                let msg = match (&msg_delta, c.needs_full || coalescing) {
                    (Some(d), false) => d,
                    _ => {
                        c.needs_full = false;
                        &full
                    }
                };
                *slot = Some(msg.clone());
                drop(slot);
                match c.tx.try_send(Outbound::FrameReady) {
                    // Full just means a wake-up is already pending, and the
                    // slot now holds the newer frame. Nothing is lost.
                    Ok(()) | Err(TrySendError::Full(_)) => {}
                    Err(TrySendError::Disconnected(_)) => c.lagged = true,
                }
            }
            let dropped = cs.iter().any(|c| c.lagged);
            cs.retain(|c| !c.lagged);
            drop(cs);
            // A dropped client's dimensions were constraining the session, so
            // the remaining viewports must be recomputed - otherwise one
            // wedged peer pins everyone else's geometry forever.
            if dropped {
                self.size_dirty.store(true, Ordering::Relaxed);
            }
        }
        *last = Some(frame);
    }

    // --- clients -----------------------------------------------------------

    fn lock_clients(&self) -> MutexGuard<'_, Vec<Client>> {
        lock(&self.clients)
    }

    /// Register a client. The caller must send it a full snapshot before
    /// forwarding anything from the returned queue.
    pub fn attach(
        &self,
        cols: u16,
        rows: u16,
    ) -> (u64, Receiver<Outbound>, Arc<Mutex<Option<ServerMsg>>>) {
        // Raw, for the same reason as set_client_size; apply_size filters and
        // then clamps the surviving minimum.
        let (tx, rx) = std::sync::mpsc::sync_channel(CLIENT_QUEUE);
        let latest = Arc::new(Mutex::new(None));
        let id = self.next_client.fetch_add(1, Ordering::Relaxed);
        self.lock_clients().push(Client {
            id,
            tx,
            latest: Arc::clone(&latest),
            cols,
            rows,
            lagged: false,
            needs_full: true,
        });
        self.apply_size();
        (id, rx, latest)
    }

    pub fn detach(&self, id: u64) {
        self.lock_clients().retain(|c| c.id != id);
        self.apply_size();
    }

    pub fn client_count(&self) -> usize {
        self.lock_clients().len()
    }

    pub fn set_client_size(&self, id: u64, cols: u16, rows: u16) {
        // Store the RAW size. Clamping here made the "ignore degenerate
        // clients" filter in apply_size dead code: a minimised client
        // reporting 0x0 became 8x3 and pinned every other viewport.
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
            let (cols, rows) = clamp_size(cols, rows);
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
        let width = self.size().0 as usize;

        // Drop the least useful parts first rather than letting the whole line
        // be truncated. On a narrow pane the old format showed a clipped
        // session name and nothing else - the pane count, which is what you
        // actually need, fell off the end.
        let full = format!(
            " {} | {} pane{} | pane {}{} ",
            self.name,
            n,
            if n == 1 { "" } else { "s" },
            focus,
            scrolled
        );
        if full.chars().count() <= width {
            return full;
        }
        let medium = format!(" {} pane{} | pane {}{} ", n, if n == 1 { "" } else { "s" }, focus, scrolled);
        if medium.chars().count() <= width {
            return medium;
        }
        format!(" {n}p{scrolled} ")
    }

    pub fn snapshot(&self) -> Frame {
        let (cols, rows) = self.size();
        let layout = self.layout_snapshot();
        let status = self.cfg.status_bar.then(|| self.status_line());
        let revision = self.revision.load(Ordering::Relaxed);
        let panes = lock(&self.panes);
        let mut f = composite(
            cols,
            rows,
            &layout,
            |id| panes.get(&id).map(|p| frame_from_screen(lock(&p.term).parser.screen())),
            status.as_deref(),
        );
        f.revision = revision;
        f
    }

    /// Send to every attached client.
    ///
    /// These are never coalesced - a client that misses ShuttingDown or
    /// PaneExited misses the protocol. A full control queue really does mean a
    /// stuck peer, and that one is dropped.
    pub fn broadcast(&self, msg: &ServerMsg) {
        let mut cs = self.lock_clients();
        for c in cs.iter_mut() {
            match c.tx.try_send(Outbound::Control(msg.clone())) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    c.lagged = true;
                    eprintln!("[zmuxd] dropping client {} - control queue full", c.id);
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
    /// Recompute viewport size if a broadcast dropped a client.
    pub fn settle_size(&self) {
        if self.size_dirty.swap(false, Ordering::Relaxed) {
            self.apply_size();
        }
    }

    pub fn shutdown(&self) {
        if !self.running.swap(false, Ordering::Relaxed) {
            return; // already shutting down
        }
        self.broadcast(&ServerMsg::ShuttingDown);
        // Save the final directories: cwd was otherwise only persisted on
        // structural changes, so a plain `cd` before shutdown was lost.
        self.save_state();
        // Take `geometry` so a concurrent split cannot insert a pane AFTER the
        // map is drained, leaving a ConPTY outside teardown. Released before
        // closing.
        let panes: Vec<_> = {
            let _g = lock(&self.geometry);
            lock(&self.panes).drain().map(|(_, p)| p).collect()
        };
        // Close OFF-THREAD and wait with a bound. ClosePseudoConsole can wait
        // indefinitely before Win11 24H2, and doing it inline here wedged
        // `kill`, idle exit and last-pane exit - the daemon would never die.
        let handles: Vec<_> = panes
            .into_iter()
            .map(|p| std::thread::spawn(move || p.pty.close()))
            .collect();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        for h in handles {
            while !h.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if h.is_finished() {
                let _ = h.join();
            } else {
                eprintln!("[session] a ConPTY did not close within 5s; exiting anyway");
            }
        }
    }
}
