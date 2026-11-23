//! Operations on one column.

use crate::lines::{self, Axis};
use crate::Matrix;

/// Sum of the column, or `None` when it does not exist.
pub fn sum(m: &Matrix, col: usize) -> Option<i64> {
    lines::sum(m, Axis::Col, col)
}

/// Largest value in the column, or `None` when it does not exist or is empty.
pub fn max(m: &Matrix, col: usize) -> Option<i64> {
    lines::max(m, Axis::Col, col)
}

/// Multiplies the column by `k`. Returns false when it does not exist.
pub fn scale(m: &mut Matrix, col: usize, k: i64) -> bool {
    lines::scale(m, Axis::Col, col, k)
}

/// Exchanges two columns. Returns false when either does not exist.
pub fn swap(m: &mut Matrix, a: usize, b: usize) -> bool {
    lines::swap(m, Axis::Col, a, b)
}
