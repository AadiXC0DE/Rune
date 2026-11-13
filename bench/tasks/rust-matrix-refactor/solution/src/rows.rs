//! Operations on one row.

use crate::lines::{self, Axis};
use crate::Matrix;

/// Sum of the row, or `None` when it does not exist.
pub fn sum(m: &Matrix, row: usize) -> Option<i64> {
    lines::sum(m, Axis::Row, row)
}

/// Largest value in the row, or `None` when it does not exist or is empty.
pub fn max(m: &Matrix, row: usize) -> Option<i64> {
    lines::max(m, Axis::Row, row)
}

/// Multiplies the row by `k`. Returns false when it does not exist.
pub fn scale(m: &mut Matrix, row: usize, k: i64) -> bool {
    lines::scale(m, Axis::Row, row, k)
}

/// Exchanges two rows. Returns false when either does not exist.
pub fn swap(m: &mut Matrix, a: usize, b: usize) -> bool {
    lines::swap(m, Axis::Row, a, b)
}
