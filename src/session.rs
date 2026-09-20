//! A session: one or more windows of tiled ConPTY panes, owned by the daemon
//! and **outliving any individual client**.
//!
//! Panes live in one flat map regardless of which window shows them
//! (DESIGN.md §9), so a hidden window's shells keep running.
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
use crate::windows::{Closed, Windows};
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
    /// EVERY pane in the session, whichever window shows it. Panes do not
    /// move between windows and are not suspended when a window is hidden.
    panes: Mutex<HashMap<PaneId, PaneState>>,
    windows: Mutex<Windows>,
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
fn restore_layout(name: &str) -> Option<(Windows, HashMap<u32, String>)> {
    let snap = persist::load(name)?;
    // A corrupt or absurd saved layout must not spawn a shell per leaf. The
    // bound is across ALL windows: a background window's ptys are as real as
    // the visible one's.
    let total = snap.windows.panes().len();
    if total > MAX_PANES {
        eprintln!("[session] saved layout has {total} panes (max {MAX_PANES}); starting fresh");
        return None;
    }
    Some((snap.windows, snap.cwds))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// Build the status line.
///
/// Free and pure so the degradation ladder can be tested at every width
/// without starting a daemon and a shell - it used to be reachable only
/// through a live session, so only the widest rung was ever exercised.
#[allow(clippy::too_many_arguments)]
fn status_text(
    name: &str,
    windows: usize,
    current: usize,
    panes: usize,
    focus: u32,
    scrolled: bool,
    width: usize,
) -> String {
    let scrolled = if scrolled { " | SCROLL" } else { "" };
    let plural = if panes == 1 { "" } else { "s" };

    // Both window forms appear only once there IS more than one window. With
    // a single window they would be a constant "0*" and "0/1" taking space
    // from the parts that actually vary on a narrow terminal.
    let (wins, wshort) = if windows > 1 {
        let list = (0..windows)
            .map(|i| if i == current { format!("{i}*") } else { i.to_string() })
            .collect::<Vec<_>>()
            .join(" ");
        (format!("{list} | "), format!("{current}/{windows} | "))
    } else {
        (String::new(), String::new())
    };

    // Drop the least useful parts first rather than letting the whole line be
    // truncated. On a narrow pane the old format showed a clipped session name
    // and nothing else - the pane count, which is what you actually need, fell
    // off the end. Which window you are on outranks the pane count: getting
    // lost between windows is worse than not knowing how many panes this one
    // holds.
    [
        format!(" {name} | {wins}{panes} pane{plural} | pane {focus}{scrolled} "),
        format!(" {wins}{panes} pane{plural} | pane {focus}{scrolled} "),
        format!(" {wshort}{panes}p{scrolled} "),
    ]
    .into_iter()
    .find(|s| s.chars().count() <= width)
    // Nothing fits; this is the shortest thing that is still true.
    .unwrap_or_else(|| format!(" {panes}p{scrolled} "))
}

impl Session {
    pub fn new(name: &str, cols: u16, rows: u16) -> Result<SessionHandle> {
        let (cols, rows) = clamp_size(cols, rows);
        // Bounded: forwarding each pane's bounded queue into an unbounded one
        // would defeat the bound entirely.
        let (events_tx, events_rx) = std::sync::mpsc::sync_channel(EVENT_QUEUE);
        let (windows, cwds) = match restore_layout(name) {
            Some(restored) => restored,
            None => (Windows::new(Layout::new(PaneId::alloc())), HashMap::new()),
        };
        let session = Arc::new(Self {
            name: name.to_string(),
            panes: Mutex::new(HashMap::new()),
            windows: Mutex::new(windows),
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
        // Panes from EVERY window, not just the visible one. A background
        // window restored without shells would look empty the moment you
        // switched to it.
        let planned: Vec<(PaneId, Rect)> = {
            let w = lock(&session.windows);
            w.all().iter().flat_map(|l| l.compute(area).0).collect()
        };
        for (id, rect) in planned {
            let dir = cwds
                .get(&id.0)
                .filter(|d| crate::cwd::is_safe_local_dir(d))
                .map(std::path::PathBuf::from)
                .filter(|p| p.is_dir());
            if let Err(e) = session.spawn_pane_retrying(id, rect.w, rect.h, dir) {
                // One dead pane must not prevent the session from starting:
                // drop it, and its window with it if that was its only pane.
                eprintln!("[session] could not restore pane {}: {e}", id.0);
                lock(&session.windows).close_pane(id);
            }
        }
        // A restored layout whose panes all failed leaves nothing to show.
        if lock(&session.panes).is_empty() {
            let id = PaneId::alloc();
            *lock(&session.windows) = Windows::new(Layout::new(id));
            session.spawn_pane_retrying(id, area.w, area.h, None)?;
        }
        session.save_state();
        Ok((session, events_rx))
    }

    /// Persist the layout and each pane's directory.
    ///
    /// Called after any structural change. Failures are logged, never fatal:
    /// not being able to save a layout is no reason to lose a working session.
    fn save_state(&self) {
        // Capture layout and panes together. Taking them under separate locks
        // allowed a split to land in between, producing a snapshot whose tree
        // and directories described different moments.
        let (windows, cwds) = {
            let w = lock(&self.windows);
            let panes = lock(&self.panes);
            let cwds = panes
                .iter()
                .filter_map(|(id, p)| lock(&p.term).cwd_windows().map(|d| (id.0, d)))
                .collect();
            (w.clone(), cwds)
        };
        let snap = SessionSnapshot {
            version: persist::FORMAT_VERSION,
            name: self.name.clone(),
            windows,
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
        lock(&self.windows).current().focus()
    }

    pub fn size(&self) -> (u16, u16) {
        *lock(&self.size)
    }

    /// The CURRENT window's layout - the one that gets composited.
    fn layout_snapshot(&self) -> Layout {
        lock(&self.windows).current().clone()
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
        // Across every window: a hidden pane costs a ConPTY and a shell
        // exactly like a visible one.
        if lock(&self.windows).panes().len() >= MAX_PANES {
            return Ok(None);
        }
        let (cols, rows) = self.size();
        let area = self.area(cols, rows);
        let new_id = PaneId::alloc();
        // Inherit the focused pane's directory, as tmux does.
        //
        // Read it BEFORE splitting. `Layout::split` moves focus to the NEW
        // pane, which has no runtime yet, so asking afterwards looked up a
        // pane that does not exist and always answered None - splits silently
        // started in the daemon's directory instead. The test that was meant
        // to catch this matched the old pane's own prompt on the composited
        // screen, so it passed either way.
        let inherit = self.focused_cwd();
        let accepted = lock(&self.windows).current_mut().split(dir, area, new_id).is_some();
        if !accepted {
            eprintln!("[session] split refused: area {}x{} too small", area.w, area.h);
            return Ok(None); // too small; no pty is created for a pane that cannot fit
        }
        let rect =
            lock(&self.windows).current().rect_of(new_id, area).unwrap_or(Rect::new(0, 0, 1, 1));
        if let Err(e) = self.spawn_pane_retrying(new_id, rect.w, rect.h, inherit) {
            // Roll the tree back: a leaf with no runtime would render as a
            // dead hole and confuse focus and reflow.
            lock(&self.windows).current_mut().close(new_id);
            return Err(e);
        }
        self.reflow_locked();
        drop(_g);
        self.save_state();
        Ok(Some(new_id))
    }

    /// Close the focused pane. Returns false when it is the session's last.
    ///
    /// Closing a window's last pane closes the WINDOW, as tmux does - without
    /// that, a window created with `NewWindow` could only be dismissed by
    /// exiting its shell.
    pub fn close_focused(&self) -> bool {
        matches!(self.remove_pane(self.focus()), Closed::Pane | Closed::Window)
    }

    /// Remove one pane and tear its pty down, whichever window holds it.
    ///
    /// The single path for "this pane is going away", whether the user asked
    /// or the shell exited. Both used to open-code it, and they disagreed.
    fn remove_pane(&self, id: PaneId) -> Closed {
        // Once shutdown() has started it is killing every pane on purpose, and
        // each one duly reports its exit. Editing the layout and re-saving it
        // here would replace the arrangement shutdown() deliberately persisted
        // with whatever panes happened to still be standing when the last pty
        // closed - a restarted session came back with one window instead of
        // two. There is nothing left to remove from, so say so.
        if !self.running.load(Ordering::Relaxed) {
            return Closed::NotFound;
        }
        let _g = lock(&self.geometry);
        let outcome = lock(&self.windows).close_pane(id);
        match outcome {
            // Nothing was removed from the tree in either case.
            Closed::LastPane => return outcome,
            Closed::NotFound => {
                // A stale exit for a pane already closed by hand. The tree has
                // forgotten it; make sure the pane map has too.
                lock(&self.panes).remove(&id);
                return outcome;
            }
            Closed::Pane | Closed::Window => {}
        }
        let pane = lock(&self.panes).remove(&id);
        self.reflow_locked();
        drop(_g);
        // Close OUTSIDE the geometry lock. ClosePseudoConsole can hang on
        // pre-24H2 Windows, and holding `geometry` across it would freeze
        // every layout and resize operation in the session.
        if let Some(p) = pane {
            std::thread::spawn(move || p.pty.close());
        }
        self.save_state();
        outcome
    }

    /// A pane's process exited: remove it, and end the session if it was the
    /// last one anywhere. Returns true when there is nothing left to show.
    ///
    /// A shell exiting in a background window closes that window without
    /// disturbing the one being looked at.
    pub fn pane_exited(&self, id: PaneId) -> bool {
        self.remove_pane(id) == Closed::LastPane
    }

    /// Create a window with one pane and switch to it.
    ///
    /// `Ok(None)` means the session is already at `MAX_PANES`.
    pub fn new_window(&self) -> Result<Option<usize>> {
        let _g = lock(&self.geometry);
        if lock(&self.windows).panes().len() >= MAX_PANES {
            eprintln!("[session] new window refused: already at {MAX_PANES} panes");
            return Ok(None);
        }
        let (cols, rows) = self.size();
        let area = self.area(cols, rows);
        let id = PaneId::alloc();
        // Inherit the focused pane's directory, as a split does.
        let inherit = self.focused_cwd();
        // Spawn BEFORE touching the window list. A new window occupies the
        // whole area, so unlike a split there is no rect to compute from the
        // tree first - which means no half-built state to roll back if the
        // shell will not start.
        self.spawn_pane_retrying(id, area.w, area.h, inherit)?;
        let index = lock(&self.windows).push(Layout::new(id));
        self.reflow_locked();
        drop(_g);
        self.save_state();
        Ok(Some(index))
    }

    pub fn next_window(&self) -> bool {
        self.switch_window(|w| w.select_next())
    }

    pub fn prev_window(&self) -> bool {
        self.switch_window(|w| w.select_previous())
    }

    /// Change which window is composited.
    ///
    /// `reflow_locked` is what makes this correct rather than just convenient:
    /// it clears `last_published`, and a window switch replaces every cell on
    /// screen, so no delta could describe it. Without that the client would be
    /// sent row diffs against a screen it is no longer showing.
    fn switch_window(&self, f: impl FnOnce(&mut Windows) -> bool) -> bool {
        let changed = {
            let _g = lock(&self.geometry);
            let changed = f(&mut lock(&self.windows));
            if changed {
                self.reflow_locked();
            }
            changed
        };
        if changed {
            self.save_state();
        }
        changed
    }

    pub fn focus_dir(&self, nav: Nav) -> bool {
        let (cols, rows) = self.size();
        let changed = lock(&self.windows).current_mut().focus_dir(nav, self.area(cols, rows));
        if changed {
            self.revision.fetch_add(1, Ordering::Relaxed);
        }
        changed
    }

    pub fn resize_focused(&self, nav: Nav, step: u16) -> bool {
        let _g = lock(&self.geometry);
        let (cols, rows) = self.size();
        let changed =
            lock(&self.windows).current_mut().resize_focused(nav, self.area(cols, rows), step);
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
    fn focused_is_scrolled(&self) -> bool {
        let id = self.focus();
        let panes = lock(&self.panes);
        panes.get(&id).is_some_and(|p| lock(&p.term).scroll_offset() > 0)
    }

    /// Typing snaps the view back to live output, as a terminal does.
    fn follow_focused(&self) {
        let id = self.focus();
        let panes = lock(&self.panes);
        if let Some(p) = panes.get(&id) {
            lock(&p.term).follow_output();
        }
    }

    pub fn focus_at(&self, x: u16, y: u16) -> bool {
        let (cols, rows) = self.size();
        let hit = lock(&self.windows).current().pane_at(self.area(cols, rows), x, y);
        match hit {
            Some(id) => {
                let changed = lock(&self.windows).current_mut().set_focus(id);
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
        // EVERY window, not just the visible one. A hidden pane left at an old
        // size keeps producing output wrapped for that size, and switching to
        // it later cannot undo text that was already laid out wrongly.
        let rects: Vec<(PaneId, Rect)> = {
            let w = lock(&self.windows);
            w.all().iter().flat_map(|l| l.compute(area).0).collect()
        };
        let panes = lock(&self.panes);
        for (id, r) in rects {
            let Some(p) = panes.get(&id) else { continue };
            if r.w == 0 || r.h == 0 {
                continue;
            }
            // Skip panes already at this size. `ResizePseudoConsole` is not
            // free and makes the shell redraw, and reflow runs on every window
            // switch - where nothing has actually moved - so resizing
            // unconditionally would churn every pane in every window on each
            // C-b n. The parser's size is the pty's: they are only ever set
            // together, below.
            let (cur_rows, cur_cols) = lock(&p.term).parser.screen().size();
            if (cur_cols, cur_rows) == (r.w, r.h) {
                continue;
            }
            // Only resize the parser if the pty accepted the new size.
            // Resizing it regardless left vt100 and ConPTY disagreeing about
            // the grid, which renders as misplaced output.
            match p.pty.resize(r.w, r.h) {
                Ok(()) => lock(&p.term).resize(r.w, r.h),
                Err(e) => eprintln!("[session] pane {} resize failed: {e}", id.0),
            }
        }
        drop(panes);
        self.revision.fetch_add(1, Ordering::Relaxed);
        // Geometry changed, so no delta can describe the transition.
        *lock(&self.last_published) = None;
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

    fn status_line(&self) -> String {
        let (panes, focus, windows, current) = {
            let w = lock(&self.windows);
            (w.current().len(), w.current().focus().0, w.len(), w.index())
        };
        status_text(
            &self.name,
            windows,
            current,
            panes,
            focus,
            self.focused_is_scrolled(),
            self.size().0 as usize,
        )
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

#[cfg(test)]
mod tests {
    use super::status_text;

    /// Three windows, on the second, showing two panes with pane 7 focused.
    fn at(width: usize) -> String {
        status_text("work", 3, 1, 2, 7, false, width)
    }

    #[test]
    fn a_single_window_shows_no_window_list() {
        // The list would be a constant "0*", which tells you nothing.
        assert_eq!(status_text("work", 1, 0, 1, 1, false, 80), " work | 1 pane | pane 1 ");
    }

    #[test]
    fn several_windows_mark_the_current_one() {
        assert_eq!(status_text("work", 3, 1, 2, 7, false, 80), " work | 0 1* 2 | 2 panes | pane 7 ");
    }

    #[test]
    fn scrolling_is_flagged() {
        let s = status_text("work", 1, 0, 1, 1, true, 80);
        assert!(s.ends_with("| SCROLL "), "{s}");
    }

    #[test]
    fn the_session_name_goes_first_when_it_will_not_fit() {
        let full = at(80);
        let narrower = at(full.chars().count() - 1);
        assert!(!narrower.contains("work"), "the name should have been dropped: {narrower}");
        assert!(narrower.contains("0 1* 2"), "the window list must survive: {narrower}");
        assert!(narrower.contains("2 panes"), "{narrower}");
    }

    #[test]
    fn the_window_position_outlives_the_pane_count() {
        // Getting lost between windows is worse than not knowing how many
        // panes the current one holds.
        let s = at(12);
        assert_eq!(s, " 1/3 | 2p ");
    }

    #[test]
    fn something_true_is_always_returned_however_narrow() {
        for width in 0..40 {
            let s = at(width);
            assert!(s.contains("2p") || s.contains("2 pane"), "width {width}: {s}");
        }
    }

    /// A wider terminal must never produce a shorter line: if it did, the
    /// ladder would be skipping a rung.
    #[test]
    fn the_ladder_never_gets_longer_as_the_terminal_narrows() {
        let widths: Vec<usize> = (0..60).map(|w| at(w).chars().count()).collect();
        for pair in widths.windows(2) {
            assert!(pair[0] <= pair[1], "a wider terminal produced a shorter line: {widths:?}");
        }
    }
}
