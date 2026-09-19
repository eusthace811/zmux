//! Phase 0 / Probe B - ConPTY + Git Bash, with emphasis on CLEAN TEARDOWN.
//!
//! Spawning is the easy half. Per DESIGN.md 2.5, portable-pty creates the
//! pseudoconsole with PSEUDOCONSOLE_INHERIT_CURSOR; closing it without
//! draining output can hang, notably before Win11 24H2. The dev PC is build
//! 26200 (25H2) and will very likely NOT reproduce that - which is exactly
//! why this probe reports timings instead of just "it worked".
//!
//!   probe-b            # spawn Git Bash, echo output, forward stdin
//!   probe-b --auto     # non-interactive: run a command, exit, time teardown

use std::io::{Read, Write};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

const GIT_BASH: &str = r"C:\Program Files\Git\bin\bash.exe";
const TEARDOWN_WARN: Duration = Duration::from_secs(2);

/// Naive substring scan. A real implementation parses properly - a query can
/// straddle a read boundary, which this deliberately does not handle.
fn find(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn main() {
    let auto = std::env::args().any(|a| a == "--auto");

    if !std::path::Path::new(GIT_BASH).is_file() {
        eprintln!("not found: {GIT_BASH}");
        eprintln!("(DESIGN.md 2.4: discovering non-default installs is still an open item)");
        std::process::exit(1);
    }

    let pty = native_pty_system();
    let pair = match pty.openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }) {
        Ok(p) => p,
        Err(e) => { eprintln!("openpty failed: {e}"); std::process::exit(1); }
    };
    println!("[probe-b] ConPTY created 80x24");

    // -l only. Bash is already interactive on a pty, and -l -i changes which
    // startup files run. herdr's windows_login_shell_args returns exactly this.
    let mut cmd = CommandBuilder::new(GIT_BASH);
    cmd.arg("-l");
    if let Ok(cwd) = std::env::current_dir() { cmd.cwd(cwd); }

    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => { eprintln!("spawn failed: {e}"); std::process::exit(1); }
    };
    drop(pair.slave); // release our handle or the reader never sees EOF
    println!("[probe-b] spawned bash -l");

    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => { eprintln!("reader failed: {e}"); std::process::exit(1); }
    };
    let writer = match pair.master.take_writer() {
        Ok(w) => Arc::new(Mutex::new(w)),
        Err(e) => { eprintln!("writer failed: {e}"); std::process::exit(1); }
    };

    // portable-pty's Windows reader BLOCKS - hence a dedicated thread, which is
    // the shape herdr's pty/actor.rs uses and what we inherit (DESIGN.md 3).
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let reply_w = Arc::clone(&writer);
    let reader_thread = std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut total = 0usize;
        let mut replies = 0usize;
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    total += n;
                    // MANDATORY, not optional: ConPTY is created with
                    // PSEUDOCONSOLE_INHERIT_CURSOR and emits DSR-CPR (ESC[6n)
                    // during startup. If nobody answers, initialization never
                    // completes and teardown deadlocks. DESIGN.md 2.5.
                    if find(&buf[..n], b"\x1b[6n") {
                        if let Ok(mut w) = reply_w.lock() {
                            let _ = w.write_all(b"\x1b[1;1R");
                            let _ = w.flush();
                        }
                        replies += 1;
                        eprintln!("[probe-b] answered DSR-CPR #{replies}");
                    }
                    if tx.send(buf[..n].to_vec()).is_err() { break; }
                }
                Err(_) => break,
            }
        }
        (total, replies)
    });

    let pump = std::thread::spawn(move || {
        let mut out = std::io::stdout();
        while let Ok(chunk) = rx.recv() {
            let _ = out.write_all(&chunk);
            let _ = out.flush();
        }
    });

    if auto {
        std::thread::sleep(Duration::from_millis(1200));
        println!("\n[probe-b] --- resize to 100x30 ---");
        let _ = pair.master.resize(PtySize { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 });
        if let Ok(mut w) = writer.lock() {
            let _ = w.write_all(b"echo PROBE_B_OK $BASH_VERSION; pwd; exit\n");
            let _ = w.flush();
        }
    } else {
        println!("[probe-b] type into the shell; 'exit' ends it\n");
        let in_w = Arc::clone(&writer);
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 1024];
            while let Ok(n) = stdin.read(&mut buf) {
                if n == 0 { break; }
                let Ok(mut w) = in_w.lock() else { break };
                if w.write_all(&buf[..n]).is_err() { break; }
                let _ = w.flush();
            }
        });
    }

    // ---- teardown: the part this probe actually exists to measure ----
    let status = child.wait();
    let t0 = Instant::now();
    println!("\n[probe-b] child exited: {status:?}");

    drop(pair.master);                       // ClosePseudoConsole happens here
    let closed = t0.elapsed();
    println!("[probe-b] ClosePseudoConsole returned in {closed:?}");

    let (total, replies) = reader_thread.join().unwrap_or((0, 0));
    let drained = t0.elapsed();
    let _ = pump.join();

    println!("[probe-b] reader thread joined in {drained:?} ({total} bytes, {replies} CPR replies)");
    println!();
    if closed > TEARDOWN_WARN || drained > TEARDOWN_WARN {
        println!(">> SLOW TEARDOWN. This is the PSEUDOCONSOLE_INHERIT_CURSOR");
        println!(">> hazard in DESIGN.md 2.5. Investigate before Phase 1.");
    } else {
        println!(">> Teardown clean on THIS machine. Note the Windows build:");
        println!(">> a 24H2+ box will not reproduce the pre-24H2 deadlock.");
        println!(">> Re-run on the work laptop before trusting this result.");
    }
}
