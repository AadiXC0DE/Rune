use bitset_fixture::BitSet;

fn set(size: usize, values: &[usize]) -> BitSet {
    let mut s = BitSet::new(size);
    for &v in values {
        s.insert(v);
    }
    s
}

#[test]
fn insert_and_contains() {
    let s = set(10, &[1, 4, 9]);
    assert!(s.contains(4));
    assert!(!s.contains(5));
    assert!(!s.contains(100));
    assert_eq!(s.len(), 3);
}

#[test]
fn union_of_equal_sizes() {
    let a = set(32, &[1, 2]);
    let b = set(32, &[2, 31]);
    assert_eq!(a.union(&b).iter().collect::<Vec<_>>(), vec![1, 2, 31]);
}

#[test]
fn intersection_and_difference_of_equal_sizes() {
    let a = set(20, &[1, 2, 3]);
    let b = set(20, &[2, 3, 4]);
    assert_eq!(a.intersection(&b).iter().collect::<Vec<_>>(), vec![2, 3]);
    assert_eq!(a.difference(&b).iter().collect::<Vec<_>>(), vec![1]);
}

#[test]
fn iterates_a_single_word_in_order() {
    let s = set(64, &[63, 0, 7]);
    assert_eq!(s.iter().collect::<Vec<_>>(), vec![0, 7, 63]);
}
