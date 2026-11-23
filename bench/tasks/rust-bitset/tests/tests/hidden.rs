use bitset_fixture::BitSet;

fn set(size: usize, values: &[usize]) -> BitSet {
    let mut s = BitSet::new(size);
    for &v in values {
        s.insert(v);
    }
    s
}

#[test]
fn iteration_reaches_the_highest_bit_at_every_boundary_size() {
    for size in [63usize, 64, 65, 128] {
        let s = set(size, &[0, size - 1]);
        assert_eq!(s.iter().collect::<Vec<_>>(), vec![0, size - 1], "size {size}");
        let full: Vec<usize> = (0..size).collect();
        assert_eq!(set(size, &full).iter().collect::<Vec<_>>(), full, "size {size}");
    }
}

#[test]
fn union_keeps_bits_of_the_larger_set() {
    for (small, large) in [(10usize, 65usize), (64, 65), (63, 128), (64, 128)] {
        let a = set(small, &[1, small - 1]);
        let b = set(large, &[2, large - 1]);
        let want = vec![1, 2, small - 1, large - 1];
        let mut want_sorted = want.clone();
        want_sorted.sort();
        want_sorted.dedup();
        assert_eq!(a.union(&b).iter().collect::<Vec<_>>(), want_sorted, "{small} | {large}");
        assert_eq!(b.union(&a).iter().collect::<Vec<_>>(), want_sorted, "{large} | {small}");
        assert_eq!(a.union(&b).size(), large);
    }
}

#[test]
fn difference_with_a_shorter_set_keeps_the_tail() {
    let a = set(130, &[1, 64, 100, 129]);
    let b = set(10, &[1]);
    let d = a.difference(&b);
    assert_eq!(d.iter().collect::<Vec<_>>(), vec![64, 100, 129]);
    assert_eq!(d.size(), 130);
}

#[test]
fn difference_with_a_longer_set() {
    let a = set(10, &[1, 2, 9]);
    let b = set(200, &[2, 150]);
    assert_eq!(a.difference(&b).iter().collect::<Vec<_>>(), vec![1, 9]);
}

#[test]
fn intersection_across_sizes_and_counts() {
    let a = set(65, &[0, 63, 64]);
    let b = set(128, &[63, 64, 100]);
    let i = a.intersection(&b);
    assert_eq!(i.iter().collect::<Vec<_>>(), vec![63, 64]);
    assert_eq!(i.len(), 2);
    assert_eq!(a.union(&b).len(), 4);
}
