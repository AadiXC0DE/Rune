//! Operations on one column.

use crate::Matrix;

/// Sum of the column, or `None` when the column does not exist.
pub fn sum(m: &Matrix, col: usize) -> Option<i64> {
    if col >= m.cols() {
        return None;
    }
    let mut total = 0;
    for r in 0..m.rows() {
        total += m.get(r, col)?;
    }
    Some(total)
}

/// Largest value in the column, or `None` when the column does not exist or is empty.
pub fn max(m: &Matrix, col: usize) -> Option<i64> {
    if col >= m.cols() {
        return None;
    }
    let mut best: Option<i64> = None;
    for r in 0..m.rows() {
        if let Some(v) = m.get(r, col) {
            best = Some(best.map_or(v, |b| b.max(v)));
        }
    }
    best
}

/// Multiplies the column by `k`. Returns false when the column does not exist.
pub fn scale(m: &mut Matrix, col: usize, k: i64) -> bool {
    if col >= m.cols() {
        return false;
    }
    for r in 0..m.rows() {
        let v = m.get(r, col).unwrap_or(0);
        m.set(r, col, v * k);
    }
    true
}

/// Exchanges two columns. Returns false when either does not exist.
pub fn swap(m: &mut Matrix, a: usize, b: usize) -> bool {
    if a >= m.cols() || b >= m.cols() {
        return false;
    }
    for r in 0..m.rows() {
        let (x, y) = (m.get(r, a).unwrap_or(0), m.get(r, b).unwrap_or(0));
        m.set(r, a, y);
        m.set(r, b, x);
    }
    true
}
