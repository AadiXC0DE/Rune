//! Operations on one row.

use crate::Matrix;

/// Sum of the row, or `None` when the row does not exist.
pub fn sum(m: &Matrix, row: usize) -> Option<i64> {
    if row >= m.rows() {
        return None;
    }
    let mut total = 0;
    for c in 0..m.cols() {
        total += m.get(row, c)?;
    }
    Some(total)
}

/// Largest value in the row, or `None` when the row does not exist or is empty.
pub fn max(m: &Matrix, row: usize) -> Option<i64> {
    if row >= m.rows() {
        return None;
    }
    (0..m.cols()).filter_map(|c| m.get(row, c)).max()
}

/// Multiplies the row by `k`. Returns false when the row does not exist.
pub fn scale(m: &mut Matrix, row: usize, k: i64) -> bool {
    if row >= m.rows() {
        return false;
    }
    for c in 0..m.cols() {
        let v = m.get(row, c).unwrap_or(0);
        m.set(row, c, v * k);
    }
    true
}

/// Exchanges two rows. Returns false when either does not exist.
pub fn swap(m: &mut Matrix, a: usize, b: usize) -> bool {
    if a >= m.rows() || b >= m.rows() {
        return false;
    }
    for c in 0..m.cols() {
        let (x, y) = (m.get(a, c).unwrap_or(0), m.get(b, c).unwrap_or(0));
        m.set(a, c, y);
        m.set(b, c, x);
    }
    true
}
