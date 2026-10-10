#![allow(clippy::expect_used)]

use std::process::Command;

fn assert_size_claims(text: &str) {
    let text = text.replace('`', "");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(text.contains("audited stripped release build"), "{text}");
    assert!(text.contains("measured 4.50 MiB"), "{text}");
    assert!(text.contains("x86_64-unknown-linux-gnu"), "{text}");
    assert!(text.contains("8 MiB ceiling"), "{text}");
    assert!(!text.contains("About 3 MiB"), "{text}");
}

#[test]
fn readme_distinguishes_the_measured_gnu_build_from_the_ceiling() {
    assert_size_claims(include_str!("../../README.md"));
}

#[test]
fn help_distinguishes_the_measured_gnu_build_from_the_ceiling_without_building() {
    for args in [&["--help"][..], &["budget", "--help"], &["budget", "-h"]] {
        // Help must work outside the repository without invoking a Cargo build.
        let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
            .args(args)
            .current_dir(std::env::temp_dir())
            .output()
            .expect("run xtask help");
        assert!(output.status.success(), "{args:?}: {output:?}");
        assert!(output.stderr.is_empty(), "{args:?}: {output:?}");
        assert_size_claims(&String::from_utf8(output.stdout).expect("UTF-8 help"));
    }
}
