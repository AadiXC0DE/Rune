//! Records the commit this binary was produced from.
//!
//! The ref that HEAD points at is what changes on a commit, not HEAD itself, so
//! that is what is watched. Watching `.git/HEAD` alone leaves a binary reporting
//! the commit before the one it was built from, because the file it watches
//! keeps saying `ref: refs/heads/main` however many commits are made.

use std::path::Path;
use std::process::Command;

fn main() {
    let commit = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());

    println!("cargo:rustc-env=RUNE_COMMIT={commit}");
    println!("cargo:rerun-if-changed=build.rs");

    // Watch the branch, not the pointer to it. A detached checkout has no ref to
    // watch, so HEAD itself is the fallback.
    watch_git_ref();
}

/// Tells cargo to rebuild when the current branch moves.
fn watch_git_ref() {
    let root = Path::new("../../.git");
    let head = root.join("HEAD");
    let Ok(text) = std::fs::read_to_string(&head) else {
        return;
    };
    let Some(reference) = text.trim().strip_prefix("ref: ") else {
        // A detached HEAD holds the commit directly, so it does change.
        println!("cargo:rerun-if-changed=../../.git/HEAD");
        return;
    };
    let path = root.join(reference);
    if path.exists() {
        println!("cargo:rerun-if-changed=../../.git/{reference}");
    } else {
        // A packed ref lives in one file, so watch that instead.
        println!("cargo:rerun-if-changed=../../.git/packed-refs");
    }
}
