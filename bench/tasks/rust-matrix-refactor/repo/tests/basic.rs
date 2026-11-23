use matrix_fixture::{cols, rows, Matrix};

fn sample() -> Matrix {
    Matrix::from_rows(vec![vec![1, 2, 3], vec![4, 5, 6], vec![7, 8, 9]])
}

#[test]
fn row_operations() {
    let mut m = sample();
    assert_eq!(rows::sum(&m, 1), Some(15));
    assert_eq!(rows::max(&m, 2), Some(9));
    assert!(rows::scale(&mut m, 0, 2));
    assert_eq!(m.get(0, 2), Some(6));
    assert!(rows::swap(&mut m, 0, 2));
    assert_eq!(m.get(0, 0), Some(7));
    assert_eq!(m.get(2, 1), Some(4));
    assert_eq!(rows::sum(&m, 3), None);
    assert!(!rows::swap(&mut m, 0, 3));
}

#[test]
fn column_operations() {
    let mut m = sample();
    assert_eq!(cols::sum(&m, 1), Some(15));
    assert_eq!(cols::max(&m, 0), Some(7));
    assert!(cols::scale(&mut m, 2, -1));
    assert_eq!(m.get(1, 2), Some(-6));
    assert!(cols::swap(&mut m, 0, 2));
    assert_eq!(m.get(0, 0), Some(-3));
    assert_eq!(m.get(2, 2), Some(7));
    assert_eq!(cols::max(&m, 3), None);
    assert!(!cols::scale(&mut m, 9, 2));
}

#[test]
fn transposes_a_square_matrix() {
    let t = sample().transpose();
    assert_eq!(t.get(0, 1), Some(4));
    assert_eq!(t.get(2, 0), Some(3));
    assert_eq!(t.transpose(), sample());
}
