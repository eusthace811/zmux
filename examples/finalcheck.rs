//! Drive a live session through splits and print the composited result.
//! Frames are read on a thread, because a blocking read means a deadline is
//! only checked between messages - which is no deadline at all when idle.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use zmux::ipc;
use zmux::protocol::{ClientMsg, Cmd, Frame, ServerMsg, PROTOCOL_VERSION};

fn main() {
    let session = std::env::args().nth(1).unwrap_or("final".into());
    let mut s = ipc::connect(&session).expect("connect");
    ipc::write_msg(&mut s, &ClientMsg::Hello { version: PROTOCOL_VERSION, cols: 110, rows: 28 })
        .unwrap();
    let _ = ipc::read_msg::<_, ServerMsg>(&mut s);

    let mut r = { use interprocess::TryClone as _; s.try_clone().unwrap() };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(Some(m)) = ipc::read_msg::<_, ServerMsg>(&mut r) {
            if tx.send(m).is_err() { return; }
        }
    });

    let mut frame: Option<Frame> = None;
    let settle = |rx: &mpsc::Receiver<ServerMsg>, f: &mut Option<Frame>, ms: u64| {
        let dl = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < dl {
            match rx.recv_timeout(Duration::from_millis(150)) {
                Ok(ServerMsg::Frame(x)) => *f = Some(x),
                Ok(ServerMsg::Delta(d)) => { if let Some(ff) = f.as_mut() { ff.apply_delta(&d); } }
                _ => {}
            }
        }
    };

    settle(&rx, &mut frame, 2500);
    for (cmd, text) in [(Cmd::SplitHorizontal, "echo RIGHT\r"), (Cmd::SplitVertical, "echo BOTTOM\r")] {
        ipc::write_msg(&mut s, &ClientMsg::Command(cmd)).unwrap();
        settle(&rx, &mut frame, 1200);
        ipc::write_msg(&mut s, &ClientMsg::Input { data: text.as_bytes().to_vec() }).unwrap();
        settle(&rx, &mut frame, 1800);
    }

    let f = frame.expect("no frame");
    for r in 0..f.rows {
        let row: String = (0..f.cols)
            .map(|c| f.cells[r as usize * f.cols as usize + c as usize].ch)
            .collect();
        println!("{}", row.trim_end());
    }
    println!("[revision {} | app_cursor {}]", f.revision, f.app_cursor);
}
