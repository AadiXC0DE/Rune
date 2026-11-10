use matrix_fixture::{cols, rows, Matrix};

#[test]
fn transposes_one_row_and_one_column() {
    let row = Matrix::from_rows(vec![vec![1, 2, 3]]);
    let t = row.transpose();
    assert_eq!((t.rows(), t.cols()), (3, 1));
    assert_eq!(t, Matrix::from_rows(vec![vec![1], vec![2], vec![3]]));
    assert_eq!(t.transpose(), row);
}

#[test]
fn transposes_a_wide_matrix() {
    let m = Matrix::from_rows(vec![vec![1, 2, 3, 4], vec![5, 6, 7, 8]]);
    let t = m.transpose();
    assert_eq!((t.rows(), t.cols()), (4, 2));
    assert_eq!(t.get(3, 0), Some(4));
    assert_eq!(t.get(3, 1), Some(8));
    assert_eq!(t.get(0, 1), Some(5));
}

#[test]
fn identity_is_its_own_transpose() {
    for n in [1usize, 2, 5] {
        let i = Matrix::identity(n);
        assert_eq!(i.transpose(), i);
    }
}

#[test]
fn empty_matrices() {
    let none = Matrix::from_rows(vec![]);
    assert_eq!((none.rows(), none.cols()), (0, 0));
    assert_eq!(none.transpose(), none);
    let flat = Matrix::new(0, 3);
    let t = flat.transpose();
    assert_eq!((t.rows(), t.cols()), (3, 0));
    assert_eq!(rows::sum(&t, 1), Some(0));
    assert_eq!(rows::max(&t, 1), None);
    assert_eq!(cols::sum(&t, 0), None);
}

#[test]
fn line_operations_on_a_non_square_matrix() {
    let mut m = Matrix::from_rows(vec![vec![3, -1, 4, 1], vec![5, 9, -2, 6]]);
    assert_eq!(rows::sum(&m, 0), Some(7));
    assert_eq!(rows::max(&m, 1), Some(9));
    assert_eq!(cols::sum(&m, 2), Some(2));
    assert_eq!(cols::max(&m, 1), Some(9));
    assert_eq!(cols::sum(&m, 4), None);
    assert_eq!(rows::sum(&m, 2), None);

    assert!(cols::swap(&mut m, 0, 3));
    assert_eq!(m, Matrix::from_rows(vec![vec![1, -1, 4, 3], vec![6, 9, -2, 5]]));
    assert!(rows::scale(&mut m, 1, -2));
    assert_eq!(m.get(1, 1), Some(-18));
    assert!(cols::scale(&mut m, 2, 10));
    assert_eq!(m.get(0, 2), Some(40));
    assert!(rows::swap(&mut m, 1, 1));
    assert!(!rows::swap(&mut m, 0, 2));
    assert!(!cols::swap(&mut m, 4, 0));
    assert!(!cols::scale(&mut m, 4, 2));
}
