//! Attach to a session and print its composited screen as text, then exit.
//!
//! Needs no console, so it works from WSL, CI, or a script - which makes it
//! the tool of choice for checking what a live session actually looks like
//! without a human staring at a terminal.
//!
//!     cargo run --example dump -- [session] [--watch]

use std::time::{Duration, Instant};

use zmux::ipc;
use zmux::protocol::{ClientMsg, ServerMsg, PROTOCOL_VERSION};

fn render(f: &zmux::protocol::Frame) -> String {
    (0..f.rows)
        .map(|r| {
            let row: String = (0..f.cols)
                .map(|c| f.cells[r as usize * f.cols as usize + c as usize].ch)
                .collect();
            format!("{}\n", row.trim_end())
        })
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let session = args.first().filter(|a| !a.starts_with("--")).cloned().unwrap_or("default".into());
    let watch = args.iter().any(|a| a == "--watch");

    if !ipc::is_running(&session) {
        eprintln!("no such session: {session}");
        std::process::exit(1);
    }
    let mut s = ipc::connect(&session).expect("connect");
    ipc::write_msg(&mut s, &ClientMsg::Hello { version: PROTOCOL_VERSION, cols: 120, rows: 32 })
        .expect("hello");

    let deadline = Instant::now() + Duration::from_secs(if watch { 3600 } else { 10 });
    while Instant::now() < deadline {
        match ipc::read_msg::<_, ServerMsg>(&mut s) {
            Ok(Some(ServerMsg::Frame(f))) => {
                println!("{}", render(&f));
                println!("-- revision {} --", f.revision);
                if !watch {
                    return;
                }
            }
            Ok(Some(ServerMsg::Refused { reason })) => {
                eprintln!("refused: {reason}");
                return;
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return,
        }
    }
}
