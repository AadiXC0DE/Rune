use semver_fixture::Version;

fn v(text: &str) -> Version {
    Version::parse(text).expect(text)
}

#[test]
fn the_specification_example_is_in_order() {
    let ordered = [
        "1.0.0-alpha",
        "1.0.0-alpha.1",
        "1.0.0-alpha.beta",
        "1.0.0-beta",
        "1.0.0-beta.2",
        "1.0.0-beta.11",
        "1.0.0-rc.1",
        "1.0.0",
    ];
    for pair in ordered.windows(2) {
        assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
    }
}

#[test]
fn a_release_is_greater_than_its_pre_releases() {
    assert!(v("2.0.0") > v("2.0.0-rc.9"));
    assert!(v("2.0.0-rc.9") > v("1.9.9"));
}

#[test]
fn numeric_identifiers_compare_as_numbers() {
    assert!(v("1.0.0-2") < v("1.0.0-10"));
    assert!(v("1.0.0-10") < v("1.0.0-a"));
}

#[test]
fn build_metadata_does_not_affect_order() {
    assert_eq!(v("1.0.0+a").cmp(&v("1.0.0+b")), std::cmp::Ordering::Equal);
}

#[test]
fn malformed_versions_are_rejected() {
    for text in ["1.0", "1.0.0.0", "a.b.c", "1.0.0-", "01.0.0"] {
        assert!(Version::parse(text).is_none(), "{text}");
    }
}
