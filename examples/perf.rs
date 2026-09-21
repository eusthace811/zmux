//! The two paths that run on every published frame, measured.
//!
//! `./x` intercepts `run`, so this is driven by cargo directly:
//!   cargo run --release --example perf
//!
//! Kept as an example rather than a throwaway script because the numbers in
//! DESIGN.md §12 are only worth anything if somebody can reproduce them on
//! the machine that matters.
use zmux::layout::{Dir, Layout, PaneId};
use zmux::protocol::Cell;
use zmux::render::{composite, frame_from_screen, pane_area_with};

fn time<T>(rounds: u32, mut f: impl FnMut() -> T) -> f64 {
    let start = std::time::Instant::now();
    for _ in 0..rounds {
        std::hint::black_box(f());
    }
    start.elapsed().as_nanos() as f64 / f64::from(rounds)
}

/// A screen with every cell occupied - the expensive case for both paths.
fn dense(cols: u16, rows: u16) -> vt100::Parser {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    for r in 0..rows {
        parser.process(format!("\x1b[{};1H\x1b[31;44m", r + 1).as_bytes());
        parser.process("x".repeat(cols as usize - 1).as_bytes());
    }
    parser
}

fn main() {
    for (cols, rows) in [(80u16, 24u16), (160, 44), (240, 60)] {
        let parser = dense(cols, rows);
        let src = frame_from_screen(parser.screen());
        let w = cols as usize;

        // --- the delta, derived once per attached client per frame ---
        let base = { let mut f = src.clone(); f.revision = 1; f };
        let same = base.clone();
        let mut one = base.clone();
        one.revision = 2;
        one.cells[w] = Cell { ch: 'y', ..one.cells[w] };
        let mut all = base.clone();
        all.revision = 2;
        for c in all.cells.iter_mut() {
            *c = Cell { ch: 'z', ..*c };
        }

        // --- the composite, built once per frame ---
        let area = pane_area_with(cols, rows, false);
        let mut layout = Layout::new(PaneId::from_raw(1));
        for (i, dir) in [Dir::Horizontal, Dir::Vertical, Dir::Horizontal].iter().enumerate() {
            layout.split(*dir, area, PaneId::from_raw(i as u32 + 2)).expect("wide enough");
        }
        let panes = layout.compute(area).0.len();

        println!("--- {cols}x{rows}, {panes} panes, dense ---");
        println!("  delta_from, nothing moved   {:>9.0} ns", time(20_000, || same.delta_from(&base)));
        println!("  delta_from, one row moved   {:>9.0} ns", time(20_000, || one.delta_from(&base)));
        println!("  delta_from, full redraw     {:>9.0} ns", time(5_000, || all.delta_from(&base)));
        println!("  frame_from_screen (1 pane)  {:>9.0} ns",
                 time(5_000, || frame_from_screen(parser.screen())));
        println!("  composite, frames given     {:>9.0} ns",
                 time(5_000, || composite(cols, rows, &layout, |_| Some(src.clone()), None))
                     - time(20_000, || src.clone()) * panes as f64);
    }
    println!();
    println!("frame_from_screen dominates, and it is bounded by vt100's per-cell");
    println!("API: `contents()` validates UTF-8 per cell and there is no slice");
    println!("accessor to go round it. See DESIGN.md §12 for what was tried.");
}
