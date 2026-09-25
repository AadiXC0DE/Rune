//! Reports where the terminal cursor sits after each prefix of a capture.
//!
//! Usage: `cursorprobe [cols] [rows] < capture.bin`

use std::io::Read;

fn main() {
    let mut args = std::env::args().skip(1);
    let cols: u16 = args.next().and_then(|v| v.parse().ok()).unwrap_or(80);
    let rows: u16 = args.next().and_then(|v| v.parse().ok()).unwrap_or(24);
    let mut bytes = Vec::new();
    if let Err(err) = std::io::stdin().read_to_end(&mut bytes) {
        eprintln!("read: {err}");
        return;
    }
    let mut grid = match rune_term::Grid::new(cols, rows) {
        Ok(grid) => grid,
        Err(err) => {
            eprintln!("grid: {err}");
            return;
        }
    };
    if let Err(err) = grid.feed(&bytes) {
        eprintln!("feed stopped: {err}");
    }
    let cursor = grid.cursor();
    eprintln!(
        "cursor at row {} col {} (rows go 0..{})",
        cursor.row,
        cursor.col,
        rows.saturating_sub(1)
    );
    for (index, line) in grid.text().lines().enumerate() {
        if !line.trim().is_empty() {
            let mark = if index as u16 == cursor.row {
                "<= cursor"
            } else {
                ""
            };
            println!("{index:3} |{line}{mark}");
        }
    }
}
