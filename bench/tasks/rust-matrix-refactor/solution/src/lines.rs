//! One implementation of the per-line operations, shared by rows and columns.

use crate::Matrix;

#[derive(Debug, Clone, Copy)]
pub(crate) enum Axis {
    Row,
    Col,
}

impl Axis {
    fn count(self, m: &Matrix) -> usize {
        match self {
            Axis::Row => m.rows(),
            Axis::Col => m.cols(),
        }
    }

    fn length(self, m: &Matrix) -> usize {
        match self {
            Axis::Row => m.cols(),
            Axis::Col => m.rows(),
        }
    }

    fn at(self, line: usize, i: usize) -> (usize, usize) {
        match self {
            Axis::Row => (line, i),
            Axis::Col => (i, line),
        }
    }
}

pub(crate) fn values(m: &Matrix, axis: Axis, line: usize) -> Option<Vec<i64>> {
    if line >= axis.count(m) {
        return None;
    }
    (0..axis.length(m))
        .map(|i| {
            let (r, c) = axis.at(line, i);
            m.get(r, c)
        })
        .collect()
}

pub(crate) fn sum(m: &Matrix, axis: Axis, line: usize) -> Option<i64> {
    values(m, axis, line).map(|v| v.iter().sum())
}

pub(crate) fn max(m: &Matrix, axis: Axis, line: usize) -> Option<i64> {
    values(m, axis, line)?.into_iter().max()
}

pub(crate) fn scale(m: &mut Matrix, axis: Axis, line: usize, k: i64) -> bool {
    if line >= axis.count(m) {
        return false;
    }
    for i in 0..axis.length(m) {
        let (r, c) = axis.at(line, i);
        let v = m.get(r, c).unwrap_or(0);
        m.set(r, c, v * k);
    }
    true
}

pub(crate) fn swap(m: &mut Matrix, axis: Axis, a: usize, b: usize) -> bool {
    if a >= axis.count(m) || b >= axis.count(m) {
        return false;
    }
    for i in 0..axis.length(m) {
        let (ra, ca) = axis.at(a, i);
        let (rb, cb) = axis.at(b, i);
        let (x, y) = (m.get(ra, ca).unwrap_or(0), m.get(rb, cb).unwrap_or(0));
        m.set(ra, ca, y);
        m.set(rb, cb, x);
    }
    true
}
