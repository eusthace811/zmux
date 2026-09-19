//! Phase 0 / Probe C - does a detached daemon survive closing its terminal?
//!
//! This is the go/no-go for persistent sessions. Windows console hosts put
//! children in a job object that may carry JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
//! DETACHED_PROCESS severs the console but does NOT leave the job.
//!
//! Zero dependencies on purpose: this must be runnable before crates.io /
//! Artifactory access is settled.
//!
//! Usage:
//!   probe-c spawn detached     # DETACHED_PROCESS only
//!   probe-c spawn breakaway    # DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB
//!   probe-c daemon <tag>       # internal: heartbeat loop
//!   probe-c report             # show what survived

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// --- job diagnostics (Codex C2): observing survival is not the same as
// knowing WHY. Report whether we are in a job and what limits it carries.
type H = *mut core::ffi::c_void;
extern "system" {
    fn GetCurrentProcess() -> H;
    fn IsProcessInJob(p: H, job: H, result: *mut i32) -> i32;
    fn QueryInformationJobObject(job: H, class: u32, info: *mut u8, len: u32, ret: *mut u32) -> i32;
}
const JOB_EXTENDED_LIMIT: u32 = 9;
const LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
const LIMIT_BREAKAWAY_OK: u32 = 0x0800;
const LIMIT_SILENT_BREAKAWAY: u32 = 0x1000;

fn job_report() -> String {
    let mut in_job = 0i32;
    let ok = unsafe { IsProcessInJob(GetCurrentProcess(), core::ptr::null_mut(), &mut in_job) } != 0;
    if !ok { return "job: IsProcessInJob failed".into(); }
    if in_job == 0 { return "job: NOT in any job".into(); }
    // JOBOBJECT_EXTENDED_LIMIT_INFORMATION; LimitFlags is at offset 16 of the
    // BASIC_LIMIT_INFORMATION that starts the struct.
    let mut buf = [0u8; 144];
    let mut ret = 0u32;
    let got = unsafe {
        QueryInformationJobObject(core::ptr::null_mut(), JOB_EXTENDED_LIMIT,
            buf.as_mut_ptr(), buf.len() as u32, &mut ret)
    } != 0;
    if !got { return "job: IN a job, QueryInformationJobObject failed".into(); }
    let flags = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);
    format!(
        "job: IN a job flags=0x{flags:08x} KILL_ON_CLOSE={} BREAKAWAY_OK={} SILENT_BREAKAWAY={} (IMMEDIATE job only - an ancestor may differ)",
        flags & LIMIT_KILL_ON_JOB_CLOSE != 0,
        flags & LIMIT_BREAKAWAY_OK != 0,
        flags & LIMIT_SILENT_BREAKAWAY != 0)
}

const HEARTBEAT_SECS: u64 = 5;
const RUN_FOR_SECS: u64 = 600; // 10 minutes is plenty to close a window

fn log_path() -> PathBuf {
    let dir = std::env::var_os("TEMP")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    dir.join("zmux-probe-c.log")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn append(line: &str) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path()) {
        let _ = writeln!(f, "{line}");
    }
}

fn spawn(mode: &str) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;

    let flags = match mode {
        "detached" => DETACHED_PROCESS | CREATE_NO_WINDOW,
        "breakaway" => DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB | CREATE_NO_WINDOW,
        other => {
            eprintln!("unknown mode {other:?}; use 'detached' or 'breakaway'");
            std::process::exit(2);
        }
    };

    println!("[spawner] {}", job_report());
    append(&format!("{} SPAWNER {}", now(), job_report()));
    let exe = std::env::current_exe()?;
    let child = Command::new(exe)
        .args(["daemon", mode])
        .creation_flags(flags)
        .spawn();

    match child {
        Ok(c) => {
            println!("spawned mode={mode} pid={} flags=0x{flags:08x}", c.id());
            append(&format!("{} SPAWN mode={} pid={}", now(), mode, c.id()));
            println!("log: {}", log_path().display());
            println!();
            println!("NOW: close this terminal (try a tab first, then the whole window).");
            println!("Then reopen and run:  probe-c report");
            Ok(())
        }
        Err(e) => {
            // CREATE_BREAKAWAY_FROM_JOB fails with ACCESS_DENIED (5) when the
            // job forbids breakaway. That is a RESULT, not a crash.
            println!("spawn mode={mode} FAILED: {e}");
            append(&format!("{} SPAWN-FAILED mode={} err={}", now(), mode, e));
            if mode == "breakaway" {
                println!();
                println!(">> Breakaway refused by the job. Under the no-WMI policy");
                println!(">> (DESIGN.md 0.2) persistent sessions are NOT available");
                println!(">> from this host terminal. This is a go/no-go answer.");
            }
            Ok(())
        }
    }
}

fn daemon(tag: &str) {
    let start = now();
    append(&format!("{start} START mode={tag} pid={}", std::process::id()));
    append(&format!("{start} DAEMON {}", job_report()));
    let deadline = start + RUN_FOR_SECS;
    loop {
        std::thread::sleep(Duration::from_secs(HEARTBEAT_SECS));
        let t = now();
        append(&format!("{t} ALIVE mode={tag} pid={}", std::process::id()));
        if t >= deadline {
            append(&format!("{t} DONE mode={tag}"));
            return;
        }
    }
}

fn report() {
    let p = log_path();
    match std::fs::read_to_string(&p) {
        Ok(s) => {
            println!("--- {} ---", p.display());
            print!("{s}");
            println!("---");
            println!();
            println!("READ IT LIKE THIS:");
            println!("  ALIVE lines with timestamps AFTER you closed the window");
            println!("  => that mode SURVIVES. Persistence works.");
            println!("  Heartbeats stop at the moment you closed the window");
            println!("  => the job killed it. That mode does NOT survive.");
        }
        Err(e) => println!("no log at {} ({e})", p.display()),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("spawn") => {
            let mode = args.get(1).map(String::as_str).unwrap_or("detached");
            if let Err(e) = spawn(mode) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Some("daemon") => daemon(args.get(1).map(String::as_str).unwrap_or("?")),
        Some("report") => report(),
        _ => {
            println!("probe-c spawn detached | probe-c spawn breakaway | probe-c report");
            std::process::exit(2);
        }
    }
}
