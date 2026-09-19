//! ConPTY ownership. Shape adapted from herdr's `src/pty/actor.rs` and
//! `src/pty/backend.rs` (Apache-2.0), Windows branch only.
//!
//! `portable-pty`'s Windows reader BLOCKS, so a dedicated OS thread per pane is
//! forced. tokio is not used for PTY I/O (DESIGN.md §3).

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

/// Default Git Bash location. Fingerprinting per DESIGN.md §2.4.
pub const GIT_BASH_DEFAULT: &str = r"C:\Program Files\Git\bin\bash.exe";

/// herdr's fingerprint (`windows.rs:1947`): do not trust the name. Require
/// `bash.exe`, absolute, real file, parent dir `bin`; then from the root
/// (stepping past `usr`) require the MSYS runtime AND git.exe.
pub fn is_git_bash(path: &std::path::Path) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else { return false };
    if !name.eq_ignore_ascii_case("bash.exe") || !path.is_absolute() || !path.is_file() {
        return false;
    }
    let Some(bin) = path.parent() else { return false };
    if !bin.file_name().and_then(|s| s.to_str()).is_some_and(|n| n.eq_ignore_ascii_case("bin")) {
        return false;
    }
    let Some(mut root) = bin.parent() else { return false };
    if root.file_name().and_then(|s| s.to_str()).is_some_and(|n| n.eq_ignore_ascii_case("usr")) {
        let Some(p) = root.parent() else { return false };
        root = p;
    }
    root.join("usr").join("bin").join("msys-2.0.dll").is_file()
        && root.join("cmd").join("git.exe").is_file()
}

pub fn find_git_bash() -> Option<std::path::PathBuf> {
    let default = std::path::PathBuf::from(GIT_BASH_DEFAULT);
    if is_git_bash(&default) {
        return Some(default);
    }
    // TODO (DESIGN.md §2.4): per-user / non-default installs are still open.
    for root in ["C:\\Program Files (x86)\\Git", "C:\\Git"] {
        let p = std::path::Path::new(root).join("bin").join("bash.exe");
        if is_git_bash(&p) {
            return Some(p);
        }
    }
    None
}

/// Write the shell-integration rcfile once per daemon and return its path.
fn ensure_rcfile() -> Result<std::path::PathBuf> {
    let dir = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("zmux");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("zmux.bashrc");
    // Rewrite every time: a stale copy from an older build would silently
    // change shell behaviour in ways that are painful to diagnose.
    std::fs::write(&path, crate::cwd::RCFILE.as_bytes())?;
    Ok(path)
}

pub enum PtyEvent {
    Output(Vec<u8>),
    Exited(i32),
}

/// Bounded: a pane producing output faster than the pump consumes it must not
/// grow the daemon's memory.
const OUTPUT_QUEUE: usize = 256;

/// Send that applies backpressure but gives up once the pane is closing.
///
/// A plain blocking `send` on a bounded channel deadlocks teardown: nobody
/// drains during shutdown, the reader blocks forever, and `close()` joins it.
/// Output is worth waiting for; a pane being torn down is not.
fn send_or_abort<T>(tx: &SyncSender<T>, mut msg: T, closing: &AtomicBool) -> bool {
    loop {
        if closing.load(Ordering::Relaxed) {
            return false;
        }
        match tx.try_send(msg) {
            Ok(()) => return true,
            Err(TrySendError::Full(m)) => {
                msg = m;
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
}

/// Owns one ConPTY. `Send + Sync` so every attached connection can write to it
/// without taking ownership - the session outlives any single client.
pub struct Pane {
    /// `Option` so `close()` can TAKE it and drop the pseudoconsole.
    ///
    /// This is load-bearing: the reader thread blocks in `read()` until the
    /// pseudoconsole closes, so joining it without dropping the master first
    /// deadlocks. Closing then joining is also the order Microsoft documents
    /// for `ClosePseudoConsole` (DESIGN.md §2.5).
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    closing: Arc<AtomicBool>,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl Pane {
    /// Spawns Git Bash under a new ConPTY.
    ///
    /// `on_output` runs on the reader thread for every chunk and returns bytes
    /// to write BACK into the pty - terminal query replies. This is not
    /// optional: ConPTY is created with `PSEUDOCONSOLE_INHERIT_CURSOR` and
    /// emits `ESC[6n` during startup. Leave it unanswered and the shell never
    /// finishes initialising and teardown deadlocks. Reproduced on Windows 11
    /// build 26200 (DESIGN.md §2.5).
    pub fn spawn<F>(cols: u16, rows: u16, on_output: F) -> Result<(Self, Receiver<PtyEvent>)>
    where
        F: FnMut(&[u8]) -> Vec<u8> + Send + 'static,
    {
        Self::spawn_in(cols, rows, None, on_output)
    }

    /// Spawn with an explicit starting directory.
    pub fn spawn_in<F>(
        cols: u16,
        rows: u16,
        cwd: Option<&std::path::Path>,
        mut on_output: F,
    ) -> Result<(Self, Receiver<PtyEvent>)>
    where
        F: FnMut(&[u8]) -> Vec<u8> + Send + 'static,
    {
        let shell = find_git_bash().context(
            "Git Bash not found; looked for bash.exe with msys-2.0.dll and cmd/git.exe alongside",
        )?;

        let pair = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| anyhow::anyhow!("openpty: {e}"))?;

        // `-l` only. Bash is already interactive on a pty, and `-l -i` changes
        // which startup files run. herdr's windows_login_shell_args does the
        // same (`pane.rs:1856`).
        let mut cmd = CommandBuilder::new(&shell);
        // Shell integration for OSC 7 cwd reporting.
        //
        // NOTE: deliberately NOT `-l`. A login shell ignores --rcfile, so the
        // two are mutually exclusive; cwd::RCFILE does the login shell's work
        // instead. Long options must also precede short ones - `bash -l
        // --rcfile X` is rejected outright with "--: invalid option".
        //
        // If the rcfile cannot be written we fall back to a plain login shell:
        // losing cwd tracking is much better than losing the user's profile.
        match ensure_rcfile() {
            Ok(rc) => {
                cmd.arg("--rcfile");
                cmd.arg(&rc);
                cmd.arg("-i");
            }
            Err(e) => {
                eprintln!("[pty] no shell integration ({e}); falling back to a login shell");
                cmd.arg("-l");
            }
        }
        match cwd {
            Some(d) if d.is_dir() => cmd.cwd(d),
            _ => {
                if let Ok(d) = std::env::current_dir() {
                    cmd.cwd(d);
                }
            }
        }

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| anyhow::anyhow!("spawn {}: {e}", shell.display()))?;
        drop(pair.slave); // release our copy or the reader never sees EOF

        let mut reader = pair.master.try_clone_reader().map_err(|e| anyhow::anyhow!("{e}"))?;
        let writer: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(pair.master.take_writer().map_err(|e| anyhow::anyhow!("{e}"))?));

        let (tx, events): (SyncSender<PtyEvent>, Receiver<PtyEvent>) =
            mpsc::sync_channel(OUTPUT_QUEUE);
        let reply_w = Arc::clone(&writer);
        let exit_tx = tx.clone();
        let closing = Arc::new(AtomicBool::new(false));
        let reader_closing = Arc::clone(&closing);

        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        // Replies FIRST and unconditionally. An unanswered
                        // ESC[6n wedges the shell, so this must not queue
                        // behind anything (DESIGN.md §2.5).
                        let replies = on_output(&buf[..n]);
                        if !replies.is_empty() {
                            if let Ok(mut w) = reply_w.lock() {
                                let _ = w.write_all(&replies);
                                let _ = w.flush();
                            }
                        }
                        if !send_or_abort(&tx, PtyEvent::Output(buf[..n].to_vec()), &reader_closing) {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let waiter_closing = Arc::clone(&closing);
        let waiter = std::thread::spawn(move || {
            let code = child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1);
            send_or_abort(&exit_tx, PtyEvent::Exited(code), &waiter_closing);
        });

        Ok((
            Self {
                master: Mutex::new(Some(pair.master)),
                writer,
                closing,
                threads: Mutex::new(vec![reader, waiter]),
            },
            events,
        ))
    }

    /// Tear the ConPTY down deliberately.
    ///
    /// Relying on drop order is not enough on Windows: closing a pseudoconsole
    /// without draining it can hang, notably before Win11 24H2 (DESIGN.md
    /// §2.5). Signal the reader, drop the master to close the pseudoconsole,
    /// then join so no thread outlives the pane.
    pub fn close(&self) {
        self.closing.store(true, Ordering::Relaxed);
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.flush();
        }
        // Drop the master FIRST. That closes the pseudoconsole, which is what
        // makes the blocked reader return so the join below can complete.
        {
            let mut m = match self.master.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            m.take();
        }
        let handles: Vec<_> = match self.threads.lock() {
            Ok(mut t) => t.drain(..).collect(),
            Err(p) => p.into_inner().drain(..).collect(),
        };
        // Bounded patience. Joining is how we know teardown finished, but a
        // wedged ConPTY must not take the whole daemon with it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        for h in handles {
            while !h.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            if h.is_finished() {
                let _ = h.join();
            } else {
                eprintln!("[pty] thread did not finish within 3s; abandoning it");
            }
        }
    }

    /// Write user input.
    ///
    /// Chunked so a large paste cannot hold the writer mutex for long: the
    /// reader thread needs that same mutex to answer terminal queries, and a
    /// blocked query reply wedges the shell.
    pub fn write_input(&self, data: &[u8]) -> Result<()> {
        const CHUNK: usize = 4096;
        for part in data.chunks(CHUNK) {
            let mut w = self.writer.lock().map_err(|_| anyhow::anyhow!("writer poisoned"))?;
            w.write_all(part)?;
            w.flush()?;
            drop(w); // yield between chunks so replies can interleave
        }
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let m = self.master.lock().map_err(|_| anyhow::anyhow!("master poisoned"))?;
        let Some(m) = m.as_ref() else { return Ok(()) }; // already closed
        m.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| anyhow::anyhow!("resize: {e}"))
    }
}
