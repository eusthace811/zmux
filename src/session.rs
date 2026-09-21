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
use crate::protocol::{
    clamp_size, Frame, MouseIn, MouseKind, PaneInfo, PaneState, ServerMsg, SessionInfo, Source,
    Target, MAX_INPUT_BYTES, MAX_PANES, RECENT_LINES_DEFAULT,
};

use crate::pty::{Pane, PtyEvent};
use crate::persist::{self, SessionSnapshot};
use crate::render::{composite, frame_from_screen, pane_area_with, status_spans, BarState, Span};
use crate::term::Term;

use crate::config::Config;

/// How long a `Wait` may actually block.
///
/// A wait holds one of the daemon's connection threads, so the CALLER does
/// not get the last word: a script that asks to wait forever and then dies
/// would leak one until the daemon exits. At the ceiling the caller is told
/// `Timeout`, which it has to handle anyway, and can ask again.
pub fn capped_wait(asked_ms: u32, max_ms: u64) -> u64 {
    u64::from(asked_ms).min(max_ms)
}

/// One pane's answer to "what is happening in there", cheaply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneStatus {
    pub state: PaneState,
    pub busy: bool,
    pub bells: u32,
    pub quiet_ms: u64,
}

/// A live session plus the channel its panes report on.
pub type SessionHandle = (Arc<Session>, Receiver<(PaneId, PtyEvent)>);

/// A pane's live halves: what it has drawn, and what is drawing it.
///
/// Named for what it holds rather than `PaneState`, which it used to be -
/// `protocol::PaneState` is the state a pane REPORTS, a completely different
/// thing, and every mention of either in this file had to be written out in
/// full to say which one was meant.
struct LivePane {
    term: Arc<Mutex<Term>>,
    pty: Arc<Pane>,
}

/// One pane's pending resize, lifted out of the pane map so that the pty
/// write inside `Pane::resize` happens with the map lock released. See
/// `reflow_locked`.
struct PendingResize {
    id: PaneId,
    rect: Rect,
    pty: Arc<Pane>,
    term: Arc<Mutex<Term>>,
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
    panes: Mutex<HashMap<PaneId, LivePane>>,
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
    /// Serialises persistence end to end.
    ///
    /// Write-then-rename stops a torn file, not a stale one: two savers could
    /// each capture state, and the one that captured FIRST could rename LAST,
    /// leaving the older layout on disk. The lock is taken before the capture,
    /// not just around the write.
    save: Mutex<()>,
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
            save: Mutex::new(()),
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
    ///
    /// Skipped once the session is shutting down. `shutdown` saves the
    /// arrangement it means to restore and THEN drains the pane map, so a
    /// save arriving after that would capture no panes, write a layout with
    /// no directories in it, and every restored shell would come back in the
    /// wrong place. `remove_pane` has refused to run during teardown since a
    /// restarted session came back with one window instead of two; putting
    /// the check here covers every caller, including a split that is still
    /// retrying a shell when the session is told to stop.
    fn save_state(&self) {
        let _s = lock(&self.save);
        // Checked UNDER the lock, not before it. `shutdown` clears `running`
        // and then takes this same lock to write the arrangement it means to
        // restore; a save that read `true` a moment earlier would otherwise
        // queue up behind it and overwrite it with one taken after the pane
        // map was drained.
        if !self.running.load(Ordering::Relaxed) {
            return;
        }
        self.save_locked();
    }

    /// The save `shutdown` performs, which must happen although `running` is
    /// already false by then.
    fn save_state_now(&self) {
        let _s = lock(&self.save);
        self.save_locked();
    }

    /// Capture and write. The caller holds `save`.
    fn save_locked(&self) {
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
        let (pty, events) = Pane::spawn_with(cols.max(1), rows.max(1), cwd.as_deref(), &self.cfg, Some(id.0), move |chunk| {
            let mut t = lock(&feed);
            t.parser.process(chunk);
            let cur = t.cursor();
            t.responder.scan(chunk, cur)
        })?;

        lock(&self.panes).insert(id, LivePane { term, pty: Arc::new(pty) });

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

    /// Write to the focused pane. False when it went away first.
    ///
    /// The distinction matters to a control caller: a human losing a
    /// keystroke as a shell exits is nothing, but a script told its `run`
    /// succeeded when not a byte was written would go on to poll for output
    /// that can never appear.
    pub fn write_focused(&self, data: &[u8]) -> Result<bool> {
        if data.len() > MAX_INPUT_BYTES {
            bail!("input of {} bytes exceeds MAX_INPUT_BYTES", data.len());
        }
        self.follow_focused();
        let id = self.focus();
        let pty = {
            let panes = lock(&self.panes);
            match panes.get(&id) {
                Some(p) => Arc::clone(&p.pty),
                // The pane exited between reading focus and looking it up.
                // That is a stale keystroke, not a protocol violation - and
                // the daemon ends the connection on an input error, so
                // returning one here threw the user out of a live session
                // merely for typing as a shell exited.
                None => return Ok(false),
            }
        };
        // Lock released before writing: a blocked write must not stall the
        // whole session.
        pty.write_input(data)?;
        Ok(true)
    }

    /// Write to a pane by 1-based position in the current window, leaving
    /// focus alone.
    ///
    /// False when there is no such pane. Unlike `write_focused` this does not
    /// snap the pane back to the live screen: a control caller typing into a
    /// background pane has no business yanking the user's scrollback view.
    pub fn write_pane(&self, position: usize, data: &[u8]) -> Result<bool> {
        let Some(id) = self.pane_at_position(position) else { return Ok(false) };
        self.write_to(id, data, false, false)
    }

    /// Resolve a 1-based position in the current window to a pane.
    fn pane_at_position(&self, position: usize) -> Option<PaneId> {
        let i = position.checked_sub(1)?;
        lock(&self.windows).current().panes().get(i).copied()
    }

    /// Resolve a control caller's `Target` to a pane, or `None` if there is
    /// no such pane.
    ///
    /// `None` is ALWAYS refused by the caller rather than redirected to
    /// something nearby (DESIGN.md §11). A target that no longer resolves
    /// usually means the pane exited, and quietly picking a different one
    /// types somebody's command into the wrong shell.
    pub fn resolve(&self, target: &Target) -> Option<PaneId> {
        match target {
            Target::Focused => Some(self.focus()),
            Target::Position(p) => self.pane_at_position(*p as usize),
            // Any window, not just the visible one - and checked against the
            // live tree rather than the pane map, so an id belonging to a
            // pane that has exited does not resolve.
            Target::Id(id) => {
                let id = PaneId::from_raw(*id);
                lock(&self.windows).panes().contains(&id).then_some(id)
            }
            // A name that two panes claim addresses NEITHER. Taking the
            // first match would put a prompt into whichever pane happened to
            // sort earlier, which is the silent wrong answer every other
            // kind of unresolvable target is refused to avoid.
            //
            // Nothing prevents the duplicate: `record` is per-pane, and a
            // report arrives on the pty reader path holding that pane's
            // `Term` lock - it cannot go looking at its neighbours without
            // inverting the lock order. So the check belongs here, where the
            // locks are already taken in the right order, and it is the
            // right place anyway: an ambiguous name is TEMPORARY. When one
            // of the two claimants exits, `133;D` clears its claim and the
            // name starts working again.
            Target::Name(want) => match self.named(want).as_slice() {
                [only] => Some(*only),
                _ => None,
            },
        }
    }

    /// Every live pane currently claiming `want`, in window-then-position
    /// order.
    ///
    /// Public so the daemon can say how many there were: "no pane named
    /// 'reviewer'" and "two panes are called 'reviewer'" are different
    /// problems with different fixes, and a caller told the first when the
    /// second is true goes looking for a pane that is right there.
    pub fn named(&self, want: &str) -> Vec<PaneId> {
        let ids = lock(&self.windows).panes();
        let panes = lock(&self.panes);
        ids.into_iter()
            .filter(|id| {
                panes.get(id).is_some_and(|p| lock(&p.term).name().as_deref() == Some(want))
            })
            .collect()
    }

    /// Write to a pane by id, leaving focus and the pane's scroll position
    /// alone. False when the pane went away first.
    ///
    /// `paste` wraps the payload in a bracketed paste, but only when the
    /// program in that pane has actually asked for one - which only this
    /// side knows, because only this side holds the vt100 state. `enter`
    /// appends the newline IN THE SAME WRITE, so a program reading a line
    /// cannot observe the text without the byte that submits it.
    pub fn write_to(&self, id: PaneId, data: &[u8], paste: bool, enter: bool) -> Result<bool> {
        if data.len() > MAX_INPUT_BYTES {
            bail!("input of {} bytes exceeds MAX_INPUT_BYTES", data.len());
        }
        let (pty, bracketed) = {
            let panes = lock(&self.panes);
            match panes.get(&id) {
                Some(p) => {
                    let bracketed = lock(&p.term).parser.screen().bracketed_paste();
                    (Arc::clone(&p.pty), bracketed)
                }
                None => return Ok(false),
            }
        };
        let payload = crate::paste::wrap(data, paste && bracketed, enter);
        if payload.len() > MAX_INPUT_BYTES {
            bail!("input of {} bytes exceeds MAX_INPUT_BYTES once wrapped", payload.len());
        }
        // Lock released before writing: a blocked write must not stall the
        // whole session.
        pty.write_input(&payload)?;
        Ok(true)
    }

    /// Everything a wait condition needs from one pane, in one lock.
    ///
    /// `info()` would answer the same questions, but it walks every pane in
    /// every window and takes each of their `Term` locks. A wait asks twenty
    /// times a second, so it asks about ONE pane. `None` when the pane has
    /// gone, which is how a wait learns to stop.
    pub fn pane_status(&self, id: PaneId) -> Option<PaneStatus> {
        let panes = lock(&self.panes);
        let p = panes.get(&id)?;
        let activity = lock(&p.term).activity();
        Some(PaneStatus {
            state: activity.state,
            busy: activity.busy,
            bells: activity.bells,
            quiet_ms: p.pty.quiet_ms(),
        })
    }

    /// Why a pane that has just died could not start, if that is what
    /// happened to it.
    ///
    /// A shell has one job before its first prompt, and when it cannot do it
    /// the reason goes to the pane's screen and nowhere else - then the pane
    /// is removed and the screen with it. The daemon was left saying only
    /// that a pane had exited, which is the symptom; every other symptom a
    /// caller sees (a target that stops resolving, a window that closed
    /// itself, a focus that moved) is downstream of this one line.
    ///
    /// Bounded by AGE, not by the exit status alone. A pane still in its first
    /// few seconds has not run anything, so its last line is its own startup
    /// error - `console device allocation failure - too many consoles in use`
    /// is the one that prompted this. An older pane's screen is the user's
    /// work, and none of it belongs in a log.
    fn starting_failure(&self, id: PaneId) -> Option<String> {
        /// Long enough to cover a slow ConPTY under load, short enough that
        /// nothing a person typed can be on screen yet.
        const NEVER_STARTED_MS: u64 = 5_000;
        let panes = lock(&self.panes);
        let p = panes.get(&id)?;
        if p.pty.age_ms() > NEVER_STARTED_MS {
            return None;
        }
        let text = lock(&p.term).parser.screen().contents();
        text.lines()
            .rev()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(|l| l.chars().take(crate::protocol::MAX_COMMAND_CHARS).collect())
    }

    /// Does this pane still exist in the layout?
    ///
    /// Asked of the PINNED id rather than the original target: a wait on
    /// position 2 must not be satisfied by whatever moves into position 2
    /// when the pane it was watching exits.
    pub fn pane_lives(&self, id: PaneId) -> bool {
        lock(&self.windows).panes().contains(&id)
    }

    /// Record what a pane says about itself, arriving over the control
    /// connection rather than on the pane's own pty.
    ///
    /// False when the pane is gone or the report changed nothing. A report
    /// does not publish: it moves no pixels, and waking every attached client
    /// for a state change nobody can see is pure cost.
    pub fn report(&self, id: PaneId, r: &crate::protocol::Report) -> bool {
        let panes = lock(&self.panes);
        panes.get(&id).is_some_and(|p| lock(&p.term).record(r))
    }

    /// Read one pane by id. See `capture` for the position-based version.
    ///
    /// `Source::Screen` is what the user can see. The other two reach into
    /// scrollback, which means moving the pane's scroll offset and putting it
    /// back - done entirely under the pane's own `Term` lock, so nobody can
    /// observe the intermediate state and nobody's terminal jumps.
    pub fn capture_from(&self, id: PaneId, lines: u16, source: Source) -> Option<String> {
        let panes = lock(&self.panes);
        let mut term = lock(&panes.get(&id)?.term);
        if source != Source::Screen {
            // 0 means "as much as you have" for the screen; for history it
            // has to mean something bounded, or a caller that omitted the
            // count gets the whole million-row buffer.
            let want = if lines == 0 { RECENT_LINES_DEFAULT } else { lines as usize };
            return Some(term.read_recent(want, source == Source::RecentUnwrapped));
        }
        let text = term.parser.screen().contents();
        drop(term);
        if lines == 0 {
            return Some(text);
        }
        let rows: Vec<&str> = text.lines().collect();
        let keep = rows.len().saturating_sub(lines as usize);
        Some(rows[keep..].join("\n"))
    }

    /// Everything a control caller needs to decide what to do next.
    ///
    /// EVERY window's panes, not just the visible one. A background window's
    /// shells keep running (§9), so a caller that cannot see them cannot work
    /// with them - and switching windows to find out what is in one moves the
    /// user's screen, which is exactly what a background caller must not do.
    /// Each pane says which window it is in; `position` stays within that
    /// window, so it still means what the status bar shows.
    pub fn info(&self) -> SessionInfo {
        let (cols, rows) = self.size();
        let area = self.area(cols, rows);
        let (window, layouts) = {
            let w = lock(&self.windows);
            (w.index(), w.all().to_vec())
        };
        let windows = layouts.len();
        let focused = layouts.get(window).map(Layout::focus);
        let mut focus = 0;
        let panes = lock(&self.panes);
        let mut list = Vec::new();
        for (w, layout) in layouts.iter().enumerate() {
            // Every window composites into the same area - only one is on
            // screen, but a hidden one's panes are sized as though it were,
            // because that is the size its ptys are actually at.
            let (rects, _) = layout.compute(area);
            for (i, id) in layout.panes().into_iter().enumerate() {
                let position = i + 1;
                if w == window && Some(id) == focused {
                    focus = position;
                }
                let rect = rects.iter().find(|(p, _)| *p == id).map(|(_, r)| *r);
                // One lock per pane, taken once for every fact it holds.
                // `quiet_ms` comes off an atomic on the pty instead, so it
                // costs nothing and cannot be blocked by a busy parser.
                let (cwd, activity, quiet_ms) = match panes.get(&id) {
                    Some(p) => {
                        let t = lock(&p.term);
                        (t.cwd_windows(), t.activity(), p.pty.quiet_ms())
                    }
                    None => (None, crate::term::Activity::default(), 0),
                };
                list.push(PaneInfo {
                    position,
                    window: w,
                    id: id.0,
                    cols: rect.map_or(0, |r| r.w),
                    rows: rect.map_or(0, |r| r.h),
                    cwd,
                    command: activity.command,
                    busy: activity.busy,
                    last_exit: activity.last_exit,
                    state: activity.state,
                    agent: activity.agent,
                    name: activity.name,
                    quiet_ms,
                    bells: activity.bells,
                    title: activity.raw_title,
                });
            }
        }
        SessionInfo {
            session: self.name.clone(),
            cols,
            rows,
            window,
            windows,
            focus,
            focus_id: focused.map_or(0, |id| id.0),
            panes: list,
            // Filled in by whoever ran a creating command; `info` on its own
            // created nothing.
            created: None,
        }
    }

    /// The visible screen of one pane, as text.
    ///
    /// `position` is 1-based, `None` meaning the focused pane; `lines` keeps
    /// that many lines from the bottom, 0 meaning all of them. None when
    /// there is no such pane.
    ///
    /// Visible rows only. Reading scrollback would mean moving the pane's
    /// scroll position, which is user-visible state that a background caller
    /// must not touch.
    pub fn capture(&self, position: Option<usize>, lines: u16) -> Option<String> {
        let id = match position {
            Some(p) => self.pane_at_position(p)?,
            None => self.focus(),
        };
        self.capture_from(id, lines, Source::Screen)
    }

    /// Split the focused pane and move the focus into the new one, as
    /// `C-b %` does.
    pub fn split(&self, dir: Dir) -> Result<Option<PaneId>> {
        self.split_with(dir, false)
    }

    /// Split WITHOUT moving the focus, for a caller working in the
    /// background while somebody else uses the session.
    ///
    /// This is what makes an agent's pane non-disruptive. A split that takes
    /// the focus moves the user's cursor mid-sentence, so a helper that needs
    /// a pane of its own could only get one by interrupting them.
    pub fn split_background(&self, dir: Dir) -> Result<Option<PaneId>> {
        self.split_with(dir, true)
    }

    fn split_with(&self, dir: Dir, keep_focus: bool) -> Result<Option<PaneId>> {
        let _g = lock(&self.geometry);
        // shutdown() drains the pane map under this same lock. A command that
        // arrives after that drain would create a ConPTY nothing owns and
        // nothing tears down - an orphaned shell. Reachable with two clients:
        // one sends Shutdown while the other has a split queued.
        if !self.running.load(Ordering::Relaxed) {
            return Ok(None);
        }
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
        // Whose focus to put back. Read before the split, because
        // `Layout::split` moves it to the new pane.
        let was_focused = keep_focus.then(|| self.focus());
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
            // Then reflow, exactly as an accepted split does. The survivors
            // were sized against a layout that included this pane, and if any
            // output arrived while its shell was being retried the pump will
            // have published that layout already. Without this the correction
            // carries the revision the client ALREADY has, and a frame at an
            // equal revision is dropped - leaving a pane on screen that does
            // not exist. `reflow_locked` moves the revision on and clears the
            // delta base, so what follows is a full frame that lands.
            self.reflow_locked();
            drop(_g);
            // Save, as both the accepted path and `remove_pane` do. Another
            // thread's `save_state` can capture the tree while this split is
            // still retrying its shell, and would otherwise leave a pane on
            // disk that never existed - to be restored, with a shell, on the
            // next start.
            self.save_state();
            return Err(e);
        }
        // Inside the geometry lock and BEFORE the reflow, so no frame is
        // ever composited with the focus on the new pane. Publishing that
        // intermediate state would flash the user's cursor across the screen
        // and back, which is the whole thing this avoids.
        if let Some(prev) = was_focused {
            lock(&self.windows).current_mut().set_focus(prev);
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
    pub fn pane_exited(&self, id: PaneId, code: i32) -> bool {
        // Before the pane goes, because its explanation goes with it.
        if code != 0 {
            if let Some(why) = self.starting_failure(id) {
                eprintln!("[session] pane {} never started: {why}", id.0);
            }
        }
        self.remove_pane(id) == Closed::LastPane
    }

    /// Create a window with one pane and switch to it.
    ///
    /// `Ok(None)` means the session is already at `MAX_PANES`.
    /// Create a window and switch to it. `Some((index, first pane))`.
    pub fn new_window(&self) -> Result<Option<(usize, PaneId)>> {
        let _g = lock(&self.geometry);
        // See split(): no pane may be created after shutdown's drain.
        if !self.running.load(Ordering::Relaxed) {
            return Ok(None);
        }
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
        Ok(Some((index, id)))
    }

    pub fn next_window(&self) -> bool {
        self.switch_window(|w| w.select_next())
    }

    pub fn prev_window(&self) -> bool {
        self.switch_window(|w| w.select_previous())
    }

    /// Jump straight to a window by index, as `C-b <digit>` does.
    pub fn select_window(&self, index: usize) -> bool {
        self.switch_window(move |w| w.select(index))
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
            if !self.running.load(Ordering::Relaxed) {
                return false; // tearing down; do not persist a new current window
            }
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
    ///
    /// Bumps the revision when it actually moved the view, because that is a
    /// change to the composited screen and a frame at an unchanged revision
    /// is dropped by the client. Usually the keystroke's own echo advances it
    /// a moment later - but an input that produces no output at all, which
    /// `zmux keys` can send, would otherwise leave the pane showing history
    /// it has already left.
    fn follow_focused(&self) {
        let id = self.focus();
        let moved = {
            let panes = lock(&self.panes);
            match panes.get(&id) {
                Some(p) => {
                    let mut t = lock(&p.term);
                    let was_scrolled = t.scroll_offset() > 0;
                    t.follow_output();
                    was_scrolled
                }
                None => false,
            }
        };
        if moved {
            self.revision.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Deliver a mouse event.
    ///
    /// The pane under the POINTER decides, not the focused one. If the
    /// program running there asked for the mouse it gets the event, in its
    /// own coordinates, and zmux does nothing else with it - a click inside
    /// vim is vim's business, including the click that would otherwise have
    /// moved focus. Otherwise zmux falls back to what it has always done
    /// with a click and a wheel.
    ///
    /// True when the composited screen changed as a result. A program that
    /// took the event will redraw itself, so that path reports false and the
    /// frame follows from its output.
    pub fn mouse(&self, ev: MouseIn) -> bool {
        if self.forward_mouse(ev) {
            return true;
        }
        match ev.kind {
            MouseKind::Press(0) => self.focus_at(ev.x, ev.y),
            MouseKind::WheelUp => {
                self.scroll_focused(Some(1));
                true
            }
            MouseKind::WheelDown => {
                self.scroll_focused(Some(-1));
                true
            }
            _ => false,
        }
    }

    /// Hand the event to the program under the pointer, if it wants it.
    ///
    /// False means nobody took it. The pty write happens with every lock
    /// released, like `write_focused`: a pane whose reader has stopped
    /// draining must not stall the session for everyone else.
    fn forward_mouse(&self, ev: MouseIn) -> bool {
        let (cols, rows) = self.size();
        let area = self.area(cols, rows);
        let Some((id, rect)) = lock(&self.windows).current().pane_rect_at(area, ev.x, ev.y)
        else {
            return false;
        };
        let out = {
            let panes = lock(&self.panes);
            let Some(p) = panes.get(&id) else { return false };
            let pty = Arc::clone(&p.pty);
            let t = lock(&p.term);
            let screen = t.parser.screen();
            crate::mouse::encode(
                screen.mouse_protocol_mode(),
                screen.mouse_protocol_encoding(),
                ev,
                ev.x - rect.x,
                ev.y - rect.y,
            )
            .map(|bytes| (pty, bytes))
        };
        match out {
            Some((pty, bytes)) => {
                let _ = pty.write_input(&bytes);
                true
            }
            None => false,
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
        // DECIDE under the pane-map lock, APPLY outside it.
        //
        // `Pane::resize` ends in a one-byte write to the pty - the NUL ConPTY
        // eats - and a pty write BLOCKS when the shell has stopped reading its
        // input. Holding the map lock across it would stall every other pane's
        // keystrokes behind one wedged shell; `write_focused` drops the lock
        // before writing for exactly this reason. `geometry` is still held,
        // deliberately: two reflows interleaving and leaving the pty at one
        // size and the parser at another is what it exists to prevent.
        let mut todo: Vec<PendingResize> = Vec::new();
        {
            let panes = lock(&self.panes);
            for (id, r) in rects {
                if r.w == 0 || r.h == 0 {
                    continue;
                }
                let Some(p) = panes.get(&id) else { continue };
                // Skip panes already at this size. `ResizePseudoConsole` is not
                // free and makes the shell redraw, and reflow runs on every
                // window switch - where nothing has actually moved - so
                // resizing unconditionally would churn every pane in every
                // window on each C-b n. The parser's size is the pty's: they
                // are only ever set together, below.
                let (cur_rows, cur_cols) = lock(&p.term).parser.screen().size();
                if (cur_cols, cur_rows) == (r.w, r.h) {
                    continue;
                }
                todo.push(PendingResize {
                    id,
                    rect: r,
                    pty: Arc::clone(&p.pty),
                    term: Arc::clone(&p.term),
                });
            }
        }
        for r in todo {
            // Only resize the parser if the pty accepted the new size.
            // Resizing it regardless left vt100 and ConPTY disagreeing about
            // the grid, which renders as misplaced output.
            match r.pty.resize(r.rect.w, r.rect.h) {
                Ok(()) => lock(&r.term).resize(r.rect.w, r.rect.h),
                Err(e) => eprintln!("[session] pane {} resize failed: {e}", r.id.0),
            }
        }
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
        // Nobody is looking. Compositing a whole screen for zero clients is
        // pure waste - the pump already skips it for exactly this reason, and
        // the control connection now publishes too, in sessions that often
        // have no viewport at all. `last_published` is only the delta BASE,
        // and it only means anything to an attached client: one that attaches
        // later is sent a full frame and flagged `needs_full`, and the first
        // publish after that refreshes the base.
        if self.client_count() == 0 {
            return;
        }
        // Hold `last_published` across COMPOSE-derive-send-update.
        //
        // Composing outside the lock let two publishers snapshot concurrently
        // and commit in the opposite order - reachable by switching windows
        // while a pane is producing output. The loser then derived a
        // BACKWARDS delta (base N+1 -> revision N) and reset the base to N.
        // The client's `revision < base_revision` guard refuses that delta,
        // but nothing repaired the base, so every later delta was derived
        // from N against a client already past it and refused too: the
        // display froze until some unrelated full frame arrived.
        let mut last = lock(&self.last_published);
        let frame = self.snapshot();
        let msg_delta = last.as_ref().and_then(|prev| frame.delta_from(prev)).map(ServerMsg::Delta);
        // Built ONLY if somebody actually needs one. The steady state is a
        // settled client taking deltas, and a full frame is the whole grid -
        // cols x rows x a 16-byte cell, so 230KB at 240x60 - copied once here
        // and again into the client's slot. Composing it unconditionally
        // meant paying for the larger of those on every tick of a session
        // where nothing ever asked for it.
        let mut full: Option<ServerMsg> = None;

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
                        full.get_or_insert_with(|| ServerMsg::Frame(frame.clone()))
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

    /// What to call a window that is not running anything: the shell's own
    /// name, as tmux does. `C:\\Program Files\\Git\\bin\\bash.exe` -> `bash`.
    fn idle_name(&self) -> String {
        let path = if self.cfg.shell.is_empty() {
            crate::pty::GIT_BASH_DEFAULT
        } else {
            &self.cfg.shell
        };
        std::path::Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("shell")
            .to_string()
    }

    fn status_line(&self) -> Vec<Span> {
        // Under the windows lock, work out WHICH panes the bar needs; under
        // the pane lock, ask them. Never the other way round - `geometry ->
        // save -> windows -> panes -> Term` is the order the whole daemon
        // takes these in.
        let (focus_ids, current_ids, focus_pos, current) = {
            let w = lock(&self.windows);
            let focus_ids: Vec<PaneId> = w.all().iter().map(|l| l.focus()).collect();
            let l = w.current();
            let ids = l.panes();
            let pos = ids.iter().position(|id| *id == l.focus()).map_or(0, |i| i + 1);
            (focus_ids, ids, pos, w.index())
        };
        let (labels, busy) = {
            let panes = lock(&self.panes);
            let activity = |id: &PaneId| {
                panes.get(id).map(|p| lock(&p.term).activity()).unwrap_or_default()
            };
            (
                focus_ids.iter().map(|id| activity(id).label()).collect::<Vec<_>>(),
                current_ids.iter().map(|id| activity(id).busy).collect::<Vec<_>>(),
            )
        };
        status_spans(
            &BarState {
                name: &self.name,
                windows: &labels,
                current,
                busy: &busy,
                focus: focus_pos,
                idle: &self.idle_name(),
                scrolled: self.focused_is_scrolled(),
            },
            self.size().0 as usize,
        )
    }

    /// Compose the current screen.
    ///
    /// The revision is read LAST, after the state it describes. Neither order
    /// is exact - the two are not captured atomically, and `serve` calls this
    /// on attach outside the `last_published` lock that serialises everything
    /// else - so the question is only which way it should be wrong.
    ///
    /// Reading it FIRST was tried and reverted. It produces a frame stamped
    /// OLDER than its contents, which a client that has already passed that
    /// revision rejects; `last_published` then holds that stale base, every
    /// later delta is derived from it and rejected too, and nothing asks for
    /// a resync - the frozen display DESIGN.md §4 describes. Stamped newer,
    /// the failure is one dropped frame that the next delta corrects, because
    /// the base and the client agree on the number even when the contents
    /// lagged. Degrading is better than wedging.
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

    /// Recompute viewport size if a broadcast dropped a client.
    pub fn settle_size(&self) {
        if self.size_dirty.swap(false, Ordering::Relaxed) {
            self.apply_size();
        }
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
        // Take `geometry` BEFORE saving, not just before draining. A split
        // holds it across the whole insert-spawn-rollback sequence, and
        // saving without it could capture the tree mid-split - persisting a
        // leaf whose shell then failed to start, to be restored with a shell
        // of its own next time. The split's own corrective save cannot fix
        // that, because by then `running` is false. Waiting here means the
        // tree has settled before it is written.
        //
        // Held on through the drain, so a concurrent split cannot insert a
        // pane AFTER the map is emptied and leave a ConPTY outside teardown.
        // Released before closing.
        let panes: Vec<_> = {
            let _g = lock(&self.geometry);
            // Save the final directories: cwd was otherwise only persisted on
            // structural changes, so a plain `cd` before shutdown was lost.
            self.save_state_now();
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
    use super::*;

    /// The caller does not get the last word on how long the daemon blocks.
    #[test]
    fn a_wait_cannot_outlast_the_daemons_ceiling() {
        assert_eq!(capped_wait(5_000, 600_000), 5_000);
        assert_eq!(capped_wait(u32::MAX, 600_000), 600_000);
        // A ceiling of zero means every wait returns at once, which is a
        // usable way to turn waiting off rather than a bug to clamp away.
        assert_eq!(capped_wait(5_000, 0), 0);
    }
}
