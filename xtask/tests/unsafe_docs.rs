#![allow(clippy::expect_used)]

const MANIFEST: &str = include_str!("../../Cargo.toml");
const README: &str = include_str!("../../README.md");

#[test]
fn unsafe_lint_comment_scopes_the_single_site_to_the_native_binary() {
    let lint_docs = MANIFEST
        .split_once("[workspace.lints.rust]")
        .expect("workspace Rust lints")
        .1
        .split_once("unsafe_code = \"deny\"")
        .expect("unsafe code remains denied by default")
        .0;
    let lint_docs = lint_docs.split_whitespace().collect::<Vec<_>>().join(" ");
    for required in [
        "native binary on Unix",
        "pre_exec",
        "crates/rune-exec/src/command.rs",
        "WASM",
        "FFI",
        "crates/rune-web/src/exports.rs",
    ] {
        assert!(
            lint_docs.contains(required),
            "unsafe lint documentation must mention {required}: {lint_docs}"
        );
    }
}

#[test]
fn readme_links_both_boundaries_and_describes_their_safety_contracts() {
    let lf = README.replace("\r\n", "\n");
    for readme in [&lf, &lf.replace('\n', "\r\n")] {
        assert_readme_boundary_docs(readme);
    }
}

fn assert_readme_boundary_docs(readme: &str) {
    // Keep section delimiters exact across LF and Windows CRLF checkouts.
    let readme = readme.replace("\r\n", "\n");
    let safety_docs = readme
        .split_once("## Unsafe code boundaries\n")
        .expect("document the native and WASM unsafe boundaries")
        .1
        .split("\n## ")
        .next()
        .expect("unsafe boundary documentation");
    let safety_docs = safety_docs.split_whitespace().collect::<Vec<_>>().join(" ");
    for required in [
        "native binary on Unix",
        "one unsafe call site",
        "pre_exec",
        "async-signal-safe",
        "WASM",
        "FFI",
        "rune_alloc",
        "ownership",
        "buffer capacity",
        "Native tests",
    ] {
        assert!(
            safety_docs.contains(required),
            "unsafe boundary documentation must mention {required}: {safety_docs}"
        );
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    for path in [
        "crates/rune-exec/src/command.rs",
        "crates/rune-web/src/exports.rs",
    ] {
        assert!(safety_docs.contains(&format!("]({path})")), "link {path}");
        assert!(root.join(path).is_file(), "boundary source exists: {path}");
    }
}
