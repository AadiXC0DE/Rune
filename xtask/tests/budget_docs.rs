#![allow(clippy::expect_used)]

use std::process::Command;

fn assert_size_claims(text: &str) {
    let text = text.replace('`', "");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(text.contains("audited stripped release build"), "{text}");
    assert!(text.contains("measured 5.46 MiB"), "{text}");
    assert!(text.contains("x86_64-unknown-linux-gnu"), "{text}");
    assert!(text.contains("8 MiB ceiling"), "{text}");
    assert!(!text.contains("About 3 MiB"), "{text}");
}

fn assert_startup_claims(text: &str) {
    let text = text.replace('`', "");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(text.contains("raw process latency"), "{text}");
    assert!(text.contains("minimum-of-31"), "{text}");
    assert!(text.contains("median and p95"), "{text}");
    assert!(text.contains("nearest rank"), "{text}");
    assert!(text.contains("30th sorted sample"), "{text}");
    assert!(text.contains("baseline minimum-of-31"), "{text}");
    assert!(text.contains("clamped to zero"), "{text}");
    assert!(text.contains("no baseline is available"), "{text}");
    assert!(!text.contains("about 2 ms to start"), "{text}");
}

fn assert_prompt_claims(text: &str) {
    let text = text.replace('`', "");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(text.contains("assembled system prompt"), "{text}");
    assert!(text.contains("6144 byte ceiling"), "{text}");
    assert!(text.contains("4 bytes per token"), "{text}");
    assert!(text.contains("not a tokenizer count"), "{text}");
}

#[test]
fn readme_distinguishes_the_measured_gnu_build_from_the_ceiling() {
    assert_size_claims(include_str!("../../README.md"));
    assert_startup_claims(include_str!("../../README.md"));
    let readme = include_str!("../../README.md")
        .replace('`', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        readme.contains("6 KiB ceiling on the assembled per-turn prompt"),
        "{readme}"
    );
}

#[test]
fn landing_page_size_claims_and_work_ceilings_match_the_audit() {
    let page = include_str!("../../site/index.html").replace("&nbsp;", " ");
    for surface in [
        "<meta name=\"description\"",
        "<meta property=\"og:description\"",
        "<meta property=\"og:image:alt\"",
        "<meta name=\"twitter:description\"",
        "<h1 ",
        "<p class=\"meta ",
    ] {
        let claim = page
            .lines()
            .find(|line| line.trim_start().starts_with(surface))
            .expect("landing page claim");
        assert!(claim.contains("5.46 MiB"), "{claim}");
    }
    assert!(page.contains("8 MiB binary size ceiling"));
    assert!(page.contains("5 ms for <code>--version</code>"));
    assert!(page.contains("8 ms for <code>--help</code>"));
    assert!(page.contains("baseline-subtracted minimum-of-31"));
    assert!(page.contains("raw process latency minimum, median and p95"));
    assert!(page.contains("checks it against a 6 KiB per-turn ceiling"));
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
        let help = String::from_utf8(output.stdout).expect("UTF-8 help");
        assert_size_claims(&help);
        assert_startup_claims(&help);
        assert_prompt_claims(&help);
    }
}
