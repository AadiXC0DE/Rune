//! Replays captured terminal bytes into a grid and prints the resulting screen.
//!
//! Driving the real binary in a pty and feeding the capture through this is how
//! a rendering claim is checked against what a terminal would actually show,
//! rather than against the bytes a program intended to emit. The two differ
//! whenever a frame lands in the wrong place, which is exactly the defect this
//! exists to catch.
//!
//! Usage: `replay [cols] [rows] < capture.bin`

use std::io::Read;

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let cols: u16 = args.next().and_then(|v| v.parse().ok()).unwrap_or(80);
    let rows: u16 = args.next().and_then(|v| v.parse().ok()).unwrap_or(24);
    let mut bytes = Vec::new();
    std::io::stdin().read_to_end(&mut bytes)?;

    let mut grid = match rune_term::Grid::new(cols, rows) {
        Ok(grid) => grid,
        Err(err) => {
            eprintln!("could not build a {cols}x{rows} grid: {err}");
            return Ok(());
        }
    };
    match grid.feed(&bytes) {
        Ok(stats) => eprintln!(
            "rows touched={} scrolled={} off={} bytes={}",
            stats.max_row_touched, stats.scrolled, stats.scroll_rows, stats.bytes_consumed
        ),
        Err(err) => eprintln!("feed stopped: {err}"),
    }
    println!("{}", grid.text());
    Ok(())
}
