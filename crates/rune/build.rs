//! Records the commit this binary was produced from, and on Windows defers
//! loading two system libraries until they are used.
//!
//! The ref that HEAD points at is what changes on a commit, not HEAD itself, so
//! that is what is watched. Watching `.git/HEAD` alone leaves a binary reporting
//! the commit before the one it was built from, because the file it watches
//! keeps saying `ref: refs/heads/main` however many commits are made.

use std::path::Path;
use std::process::Command;

/// Windows libraries the binary loads only when it first calls into them.
const DELAY_LOADED: [&str; 2] = ["user32.dll", "ws2_32.dll"];

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
    // The target triple names this build's release archive.
    if let Ok(target) = std::env::var("TARGET") {
        println!("cargo:rustc-env=RUNE_TARGET={target}");
    }
    // Windows maps user32 (keyboard layout calls) and ws2_32 (sockets) on first
    // use instead of at start, so a command that needs neither, such as
    // `rune --version`, does not pay to load them.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        for library in DELAY_LOADED {
            println!("cargo:rustc-link-arg-bins=/DELAYLOAD:{library}");
        }
        println!("cargo:rustc-link-arg-bins=delayimp.lib");
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=RUNE_CHANNEL");

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
