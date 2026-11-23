use pathnorm_fixture::normalize;

#[test]
fn resolves_dot_and_dotdot_inside_a_path() {
    assert_eq!(normalize("/a/b/../c"), "/a/c");
    assert_eq!(normalize("a/./b"), "a/b");
    assert_eq!(normalize("a/b/c/../.."), "a");
}

#[test]
fn keeps_plain_paths() {
    assert_eq!(normalize("/usr/local/bin"), "/usr/local/bin");
    assert_eq!(normalize("src/lib.rs"), "src/lib.rs");
}

#[test]
fn root_and_empty() {
    assert_eq!(normalize("/"), "/");
    assert_eq!(normalize(""), ".");
    assert_eq!(normalize("a/.."), ".");
}
