use semver_fixture::Version;
use std::cmp::Ordering;

fn v(text: &str) -> Version {
    Version::parse(text).expect(text)
}

#[test]
fn numeric_identifiers_rank_below_alphanumeric_ones() {
    assert!(v("1.0.0-99") < v("1.0.0-0a"));
    assert!(v("1.0.0-alpha.1") < v("1.0.0-alpha.beta"));
    assert!(v("1.0.0-alpha.2") < v("1.0.0-alpha.10"));
    assert!(v("1.0.0-a2") > v("1.0.0-a10"));
}

#[test]
fn a_longer_identifier_list_wins_when_the_prefix_is_equal() {
    assert!(v("1.0.0-alpha") < v("1.0.0-alpha.0"));
    assert!(v("1.0.0-alpha.1") < v("1.0.0-alpha.1.0"));
}

#[test]
fn build_metadata_is_ignored_in_ordering() {
    assert_eq!(v("1.2.3-rc.1+x").cmp(&v("1.2.3-rc.1+y.z")), Ordering::Equal);
    assert!(v("1.2.3-rc.1+zzz") < v("1.2.3+aaa"));
    assert!(v("1.2.3+aaa") <= v("1.2.3+zzz"));
}

#[test]
fn core_fields_compare_numerically() {
    assert!(v("1.10.0") > v("1.9.0"));
    assert!(v("2.0.0") > v("1.99.99"));
}
