//! ConPTY ownership. Shape adapted from herdr's `src/pty/actor.rs` and
//! `src/pty/backend.rs` (Apache-2.0), Windows branch only.
//!
//! `portable-pty`'s Windows reader BLOCKS, so a dedicated OS thread per pane is
//! forced. tokio is not used for PTY I/O (DESIGN.md §3).

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// Write the shell-integration rcfile and return its path, or `None` to launch
/// a plain login shell instead.
fn ensure_rcfile(cfg: &crate::config::Config) -> Option<std::path::PathBuf> {
    if !cfg.shell_integration {
        return None;
    }
    let dir = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("zmux");
    let write = || -> std::io::Result<std::path::PathBuf> {
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("zmux.bashrc");
        // Rewrite every time: a stale copy from an older build would silently
        // change shell behaviour in ways that are painful to diagnose.
        std::fs::write(&path, crate::cwd::RCFILE.as_bytes())?;
        Ok(path)
    };
    match write() {
        Ok(p) => Some(p),
        Err(e) => {
            // Losing cwd tracking beats losing the user's profile.
            eprintln!("[pty] no shell integration ({e}); using a login shell");
            None
        }
    }
}

/// Build the pane's command with an environment we can vouch for.
///
/// `CommandBuilder::new` seeds itself from `std::env::vars_os()` and then
/// MERGES the two Windows `Environment` registry keys over the result
/// (`portable-pty-0.9.0/src/cmdbuilder.rs:104`). That merge is unsound:
///
/// - Anything that is not `REG_SZ`/`REG_EXPAND_SZ` goes through
///   `OsString::from_reg_value`, so a `REG_MULTI_SZ` arrives with its
///   separator NULs intact. `environment_block()` (`cmdbuilder.rs:638`) writes
///   `NAME=`, the value, then a NUL - so one embedded NUL ends the block
///   early. `envs` is a `BTreeMap`, so EVERY variable sorting after that name
///   silently disappears; a shell with no `PATH` is a likely outcome, and
///   `CreateProcessW` rejecting the block outright with
///   `ERROR_INVALID_PARAMETER` (87) is the other.
/// - `REG_EXPAND_SZ` is read through `value.bytes.as_ptr() as *const u16`, an
///   unaligned load out of a `Vec<u8>`.
///
/// Group policy writes into exactly those keys, so the managed laptop this is
/// built for is where it is most likely to fire, and the symptom would look
/// like our fault. herdr carries a vendored patch for it
/// (`vendor/portable-pty.patches.md` 0003, upstream `wezterm#4364`); we do not
/// need to fork the crate, because we do not want the merge at all. The
/// daemon's own environment is what should reach a pane - that is what carries
/// `ZMUX` and `ZMUX_SESSION` - and a process environment block cannot
/// contain an embedded NUL by construction.
fn pane_command(shell: &std::path::Path) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(shell);
    #[cfg(windows)]
    {
        // Drop what the registry contributed, then put this process's own
        // environment back. On unix `get_base_env` also supplies SHELL, so the
        // scrub is deliberately Windows-only.
        cmd.env_clear();
        for (key, value) in std::env::vars_os() {
            cmd.env(key, value);
        }
    }
    cmd
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
    /// The child, kept so `close()` can terminate it.
    ///
    /// Closing the pseudoconsole is supposed to take the shell with it, but in
    /// practice shells survived and accumulated as orphaned bash.exe processes
    /// across runs. Teardown now kills explicitly rather than trusting that.
    ///
    /// SHARED with the waiter thread, which polls it for the shell's exit.
    /// This used to be a separate slot that the constructor filled by TAKING
    /// the child out of the waiter's handle - so the waiter found `None` on
    /// its first tick and returned, and `PtyEvent::Exited` could never fire
    /// for any pane. Typing `exit` left a dead pane on screen and a session
    /// whose last shell had gone never ended. One handle, two owners.
    child: Arc<Mutex<Option<Box<dyn portable_pty::Child + Send + Sync>>>>,
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
    /// When this pane last produced a byte, on the process clock.
    ///
    /// A MEASUREMENT, not a guess about what the pane is doing. zmux does
    /// not read the screen to decide whether a program has finished
    /// (DESIGN.md §12), but "it has said nothing for eleven seconds" is a
    /// fact, and it is the best signal available for a program that reports
    /// nothing about itself.
    ///
    /// Stamped by the reader thread and read by anyone: an atomic on the pane
    /// itself rather than a map the pump would have to lock hundreds of times
    /// a second.
    last_output: Arc<AtomicU64>,
    /// When this pane was created, on the same process clock.
    ///
    /// Only one thing asks: whether a shell that has just died had been alive
    /// long enough to have run anything. A pane that dies in its first moments
    /// never got a prompt, so what is on its screen is its own startup error -
    /// which is the one case where quoting a pane's screen into a log says
    /// something about zmux rather than about the person using it.
    born: u64,
}

/// Process start, for a clock that cannot go backwards.
///
/// Wall time can: an NTP correction or a timezone change would make a pane
/// look like it produced output in the future, and `quiet_ms` would saturate
/// to zero forever. `Instant` is monotonic by contract.
fn epoch() -> &'static std::time::Instant {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    EPOCH.get_or_init(std::time::Instant::now)
}

fn now_ms() -> u64 {
    epoch().elapsed().as_millis() as u64
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

    /// Environment a pane gets on top of the daemon's own.
    ///
    /// `ZMUX` and `ZMUX_SESSION` are set on the daemon process and reach
    /// every pane by inheritance, because they are the same for all of them.
    /// `ZMUX_PANE` cannot be: it is what makes one pane distinguishable
    /// from another, which is what `-p current` and a self-report need.
    ///
    /// The pane ID, deliberately, not the position. A position is what the
    /// status bar shows, but it renumbers the moment a pane closes - so a
    /// shell that read its own position at startup would be telling a caller
    /// to type into somebody else's pane an hour later. IDs are never reused.
    ///
    /// No `ZMUX_WINDOW` for that same reason: closing a window shifts every
    /// index after it. A pane's window is a question for `zmux info`, which
    /// answers it fresh.
    pub const PANE_ENV_VAR: &'static str = "ZMUX_PANE";

    /// Spawn with an explicit starting directory.
    pub fn spawn_in<F>(
        cols: u16,
        rows: u16,
        cwd: Option<&std::path::Path>,
        on_output: F,
    ) -> Result<(Self, Receiver<PtyEvent>)>
    where
        F: FnMut(&[u8]) -> Vec<u8> + Send + 'static,
    {
        Self::spawn_with(cols, rows, cwd, &crate::config::Config::load(), None, on_output)
    }

    pub fn spawn_with<F>(
        cols: u16,
        rows: u16,
        cwd: Option<&std::path::Path>,
        cfg: &crate::config::Config,
        pane_id: Option<u32>,
        mut on_output: F,
    ) -> Result<(Self, Receiver<PtyEvent>)>
    where
        F: FnMut(&[u8]) -> Vec<u8> + Send + 'static,
    {
        let shell = if cfg.shell.is_empty() {
            find_git_bash()
        } else {
            Some(std::path::PathBuf::from(&cfg.shell)).filter(|p| p.is_file())
        };
        let shell = shell.context(
            "Git Bash not found; looked for bash.exe with msys-2.0.dll and cmd/git.exe alongside",
        )?;

        let pair = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| anyhow::anyhow!("openpty: {e}"))?;

        // `-l` only. Bash is already interactive on a pty, and `-l -i` changes
        // which startup files run. herdr's windows_login_shell_args does the
        // same (`pane.rs:1856`).
        let mut cmd = pane_command(&shell);
        if let Some(id) = pane_id {
            cmd.env(Self::PANE_ENV_VAR, id.to_string());
        }
        // Shell integration for OSC 7 cwd reporting.
        //
        // NOTE: deliberately NOT `-l`. A login shell ignores --rcfile, so the
        // two are mutually exclusive; cwd::RCFILE does the login shell's work
        // instead. Long options must also precede short ones - `bash -l
        // --rcfile X` is rejected outright with "--: invalid option".
        //
        // If the rcfile cannot be written we fall back to a plain login shell:
        // losing cwd tracking is much better than losing the user's profile.
        match ensure_rcfile(cfg) {
            Some(rc) => {
                cmd.arg("--rcfile");
                cmd.arg(&rc);
                cmd.arg("-i");
            }
            None => {
                cmd.arg("-l");
            }
        }
        for a in &cfg.shell_args {
            cmd.arg(a);
        }
        match cwd {
            Some(d) if d.is_dir() => cmd.cwd(d),
            _ => {
                if let Ok(d) = std::env::current_dir() {
                    cmd.cwd(d);
                }
            }
        }

        let child = pair
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

        let last_output = Arc::new(AtomicU64::new(now_ms()));
        let reader_stamp = Arc::clone(&last_output);
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        reader_stamp.store(now_ms(), Ordering::Relaxed);
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

        let child = Arc::new(Mutex::new(Some(child)));
        let waiter_child = Arc::clone(&child);
        let waiter_closing = Arc::clone(&closing);
        let waiter = std::thread::spawn(move || {
            // Poll rather than block in wait(): close() needs the same handle
            // to kill the child, and a blocking wait would hold it forever.
            loop {
                {
                    let mut guard = match waiter_child.lock() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    match guard.as_mut() {
                        None => return,
                        Some(c) => match c.try_wait() {
                            Ok(Some(status)) => {
                                let code = status.exit_code() as i32;
                                drop(guard);
                                send_or_abort(&exit_tx, PtyEvent::Exited(code), &waiter_closing);
                                return;
                            }
                            Ok(None) => {}
                            Err(_) => return,
                        },
                    }
                }
                if waiter_closing.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        });

        let pane = Self {
            master: Mutex::new(Some(pair.master)),
            // The waiter's own handle, not a copy of it.
            child,
            writer,
            closing,
            threads: Mutex::new(vec![reader, waiter]),
            last_output,
            born: now_ms(),
        };
        Ok((pane, events))
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
        // Kill the child FIRST. Closing the pseudoconsole is supposed to take
        // the shell with it, but shells were surviving and piling up as
        // orphaned bash.exe processes. Killing explicitly also unblocks the
        // reader promptly.
        {
            let mut c = match self.child.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(child) = c.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
            *c = None;
        }
        // Then drop the master, closing the pseudoconsole, which is what makes
        // any blocked reader return so the join below can complete.
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
    /// Milliseconds since this pane last produced any output.
    ///
    /// Saturating, so a clock that has not ticked yet reads as 0 rather than
    /// wrapping to something enormous.
    pub fn quiet_ms(&self) -> u64 {
        now_ms().saturating_sub(self.last_output.load(Ordering::Relaxed))
    }

    /// Milliseconds since this pane was created.
    pub fn age_ms(&self) -> u64 {
        now_ms().saturating_sub(self.born)
    }

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

    /// Resize the pseudoconsole.
    ///
    /// Then write one NUL, which ConPTY eats.
    ///
    /// After a resize, the FIRST byte written to the pty some time later is
    /// swallowed - `echo` arrives at the shell as `cho`. Measured at this
    /// layer with no daemon involved and no terminal-query replies in flight:
    /// 4 of 16 resizes lost it, 0 of 16 with this byte. It needs a pause
    /// after the resize, so it lands exactly on the user who splits a pane
    /// and then types - which is everyone.
    ///
    /// NUL is the conventional terminal no-op filler: readline binds it to
    /// set-mark (no output) and full-screen programs ignore it. Spending one
    /// on every resize is worth not losing a keystroke every fourth split.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        {
            let m = self.master.lock().map_err(|_| anyhow::anyhow!("master poisoned"))?;
            let Some(m) = m.as_ref() else { return Ok(()) }; // already closed
            m.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
                .map_err(|e| anyhow::anyhow!("resize: {e}"))?;
        }
        // The master lock is released first: the write below takes the
        // writer lock, and there is no reason to hold both.
        let _ = self.write_input(b"\0");
        Ok(())
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// Build a tree that looks exactly like a Git for Windows install.
    fn fake_git(root: &std::path::Path, msys: bool, git: bool) {
        std::fs::create_dir_all(root.join("bin")).expect("bin");
        std::fs::write(root.join("bin").join("bash.exe"), b"").expect("bash");
        std::fs::create_dir_all(root.join("usr").join("bin")).expect("usr/bin");
        std::fs::write(root.join("usr").join("bin").join("bash.exe"), b"").expect("usr bash");
        if msys {
            std::fs::write(root.join("usr").join("bin").join("msys-2.0.dll"), b"").expect("msys");
        }
        if git {
            std::fs::create_dir_all(root.join("cmd")).expect("cmd");
            std::fs::write(root.join("cmd").join("git.exe"), b"").expect("git");
        }
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("zmux-gitbash-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// The shell fingerprint, which nothing covered.
    ///
    /// This decides what the daemon executes, and herdr's rule - which this
    /// follows - is DO NOT TRUST THE NAME. Anything called `bash.exe` on a
    /// machine would otherwise be launched as the user's shell; the check is
    /// that the MSYS runtime and `git.exe` sit where a real Git for Windows
    /// install puts them. Every rejection below is a path that would have
    /// spawned the wrong program.
    #[test]
    fn a_shell_is_only_git_bash_when_the_whole_install_is_there() {
        let root = scratch("ok");
        fake_git(&root, true, true);

        let bash = root.join("bin").join("bash.exe");
        assert!(is_git_bash(&bash), "a complete install was rejected: {}", bash.display());
        // The `usr/bin` layout, which the function steps past on purpose.
        assert!(is_git_bash(&root.join("usr").join("bin").join("bash.exe")));

        // The name is not enough, in either direction.
        std::fs::write(root.join("bin").join("notbash.exe"), b"").expect("write");
        assert!(!is_git_bash(&root.join("bin").join("notbash.exe")), "any .exe passed as a shell");
        assert!(
            is_git_bash(&root.join("bin").join("BASH.EXE")),
            "Windows filenames are case-insensitive and so must this be"
        );

        // A directory called bash.exe is not a shell.
        std::fs::create_dir_all(root.join("bin").join("dir.exe")).expect("dir");
        assert!(!is_git_bash(&root.join("bin").join("dir.exe")));

        // Right name, wrong place: not under a `bin`.
        std::fs::write(root.join("bash.exe"), b"").expect("write");
        assert!(!is_git_bash(&root.join("bash.exe")), "a bash.exe outside bin/ was accepted");

        // A relative path can be resolved against anything, so it is refused
        // before the filesystem is touched at all.
        assert!(!is_git_bash(std::path::Path::new("bin/bash.exe")));
        assert!(!is_git_bash(std::path::Path::new("bash.exe")));

        // And something that simply is not there.
        assert!(!is_git_bash(&root.join("bin").join("absent.exe")));
        std::fs::remove_dir_all(&root).ok();
    }

    /// The two halves of the fingerprint are both load-bearing.
    #[test]
    fn a_lookalike_install_missing_either_half_is_refused() {
        for (tag, msys, git) in [("nomsys", false, true), ("nogit", true, false)] {
            let root = scratch(tag);
            fake_git(&root, msys, git);
            let bash = root.join("bin").join("bash.exe");
            assert!(
                !is_git_bash(&bash),
                "{tag}: a tree with a bash.exe but no {} was taken for Git Bash",
                if msys { "git.exe" } else { "msys-2.0.dll" }
            );
            std::fs::remove_dir_all(&root).ok();
        }
    }

    /// `portable-pty` merges the Windows `Environment` registry keys over the
    /// process environment, and a value that arrives with an embedded NUL
    /// truncates the whole `CreateProcessW` block. `pane_command` must hand a
    /// pane exactly this process's environment, which cannot contain one.
    ///
    /// The sharpest symptom is `PATH`: the registry holds the unexpanded
    /// system+user value, the process holds the merged one, so if the merge is
    /// still in the builder the two differ and a pane shell starts with a
    /// `PATH` that is not the daemon's.
    #[test]
    fn a_pane_gets_this_process_environment_and_nothing_the_registry_added() {
        let cmd = pane_command(std::path::Path::new(r"C:\Program Files\Git\bin\bash.exe"));

        let built: std::collections::BTreeMap<String, String> = cmd
            .iter_extra_env_as_str()
            .map(|(k, v)| (k.to_ascii_uppercase(), v.to_string()))
            .collect();
        let ours: std::collections::BTreeMap<String, String> = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?.to_ascii_uppercase(), v.into_string().ok()?)))
            .collect();

        assert!(!ours.is_empty(), "no environment to compare against");
        assert_eq!(
            built, ours,
            "a pane would not inherit this process's environment - `portable-pty`'s \
             registry merge is still what reaches the shell"
        );
        assert_eq!(
            built.get("PATH"),
            ours.get("PATH"),
            "a pane's PATH is not the daemon's"
        );
        assert!(
            built.iter().all(|(k, v)| !k.contains('\0') && !v.contains('\0')),
            "an embedded NUL would truncate the CreateProcessW environment block"
        );
    }
}
