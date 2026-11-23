use pathnorm_fixture::normalize;

#[test]
fn never_walks_above_the_root() {
    assert_eq!(normalize("/../a"), "/a");
    assert_eq!(normalize("/.."), "/");
    assert_eq!(normalize("/a/../../.."), "/");
    assert_eq!(normalize("/../.."), "/");
}

#[test]
fn collapses_repeated_slashes_and_trailing_parts() {
    assert_eq!(normalize("a/./b//c/"), "a/b/c");
    assert_eq!(normalize("//a///b"), "/a/b");
    assert_eq!(normalize("/a/b/"), "/a/b");
    assert_eq!(normalize("a/b/."), "a/b");
    assert_eq!(normalize("/a/b/.."), "/a");
    assert_eq!(normalize("./"), ".");
    assert_eq!(normalize("//"), "/");
}

#[test]
fn relative_parent_segments_are_preserved() {
    assert_eq!(normalize(".."), "..");
    assert_eq!(normalize("../a"), "../a");
    assert_eq!(normalize("../../a"), "../../a");
    assert_eq!(normalize("a/../.."), "..");
    assert_eq!(normalize("a/../../b"), "../b");
    assert_eq!(normalize("./../a/"), "../a");
}

#[test]
fn dotted_names_are_ordinary_names() {
    assert_eq!(normalize("a/..."), "a/...");
    assert_eq!(normalize("/.hidden/../.cfg"), "/.cfg");
    assert_eq!(normalize("a../b"), "a../b");
}

#[test]
fn normalizing_twice_changes_nothing() {
    for p in ["/../a//b/./c/..", "../x/./y/", "a//", "/", ""] {
        let once = normalize(p);
        assert_eq!(normalize(&once), once, "input {p:?}");
    }
}
