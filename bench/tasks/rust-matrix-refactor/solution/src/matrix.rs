//! The matrix type, stored row by row.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matrix {
    rows: usize,
    cols: usize,
    data: Vec<i64>,
}

impl Matrix {
    /// A matrix of zeros.
    pub fn new(rows: usize, cols: usize) -> Self {
        Matrix { rows, cols, data: vec![0; rows * cols] }
    }

    /// Builds a matrix from rows of equal length. No rows give a 0 by 0 matrix.
    pub fn from_rows(rows: Vec<Vec<i64>>) -> Self {
        let cols = rows.first().map_or(0, Vec::len);
        assert!(rows.iter().all(|r| r.len() == cols), "rows differ in length");
        Matrix { rows: rows.len(), cols, data: rows.into_iter().flatten().collect() }
    }

    pub fn identity(n: usize) -> Self {
        let mut m = Matrix::new(n, n);
        for i in 0..n {
            m.data[i * n + i] = 1;
        }
        m
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn get(&self, row: usize, col: usize) -> Option<i64> {
        (row < self.rows && col < self.cols).then(|| self.data[row * self.cols + col])
    }

    pub fn set(&mut self, row: usize, col: usize, value: i64) {
        assert!(row < self.rows && col < self.cols, "index out of range");
        self.data[row * self.cols + col] = value;
    }

    pub fn transpose(&self) -> Matrix {
        let mut out = Matrix::new(self.cols, self.rows);
        for r in 0..self.rows {
            for c in 0..self.cols {
                out.data[c * self.rows + r] = self.data[r * self.cols + c];
            }
        }
        out
    }
}
