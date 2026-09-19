//! ConPTY ownership. Shape adapted from herdr's `src/pty/actor.rs` and
//! `src/pty/backend.rs` (Apache-2.0), Windows branch only.
//!
//! `portable-pty`'s Windows reader BLOCKS, so a dedicated OS thread per pane is
//! forced. tokio is not used for PTY I/O (DESIGN.md §3).

use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver, Sender};
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

pub enum PtyEvent {
    Output(Vec<u8>),
    Exited(i32),
}

/// Owns one ConPTY. `Send + Sync` so every attached connection can write to it
/// without taking ownership - the session outlives any single client.
pub struct Pane {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
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
    pub fn spawn<F>(cols: u16, rows: u16, mut on_output: F) -> Result<(Self, Receiver<PtyEvent>)>
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
        cmd.arg("-l");
        if let Ok(cwd) = std::env::current_dir() {
            cmd.cwd(cwd);
        }

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| anyhow::anyhow!("spawn {}: {e}", shell.display()))?;
        drop(pair.slave); // release our copy or the reader never sees EOF

        let mut reader = pair.master.try_clone_reader().map_err(|e| anyhow::anyhow!("{e}"))?;
        let writer: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(pair.master.take_writer().map_err(|e| anyhow::anyhow!("{e}"))?));

        let (tx, events): (Sender<PtyEvent>, Receiver<PtyEvent>) = mpsc::channel();
        let reply_w = Arc::clone(&writer);
        let exit_tx = tx.clone();

        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let replies = on_output(&buf[..n]);
                        if !replies.is_empty() {
                            if let Ok(mut w) = reply_w.lock() {
                                let _ = w.write_all(&replies);
                                let _ = w.flush();
                            }
                        }
                        if tx.send(PtyEvent::Output(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        std::thread::spawn(move || {
            let code = child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1);
            let _ = exit_tx.send(PtyEvent::Exited(code));
        });

        Ok((Self { master: Mutex::new(pair.master), writer }, events))
    }

    pub fn write_input(&self, data: &[u8]) -> Result<()> {
        let mut w = self.writer.lock().map_err(|_| anyhow::anyhow!("writer poisoned"))?;
        w.write_all(data)?;
        w.flush()?;
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let m = self.master.lock().map_err(|_| anyhow::anyhow!("master poisoned"))?;
        m.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| anyhow::anyhow!("resize: {e}"))
    }
}
