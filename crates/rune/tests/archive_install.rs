//! The README archive path and its CI smoke check, using a real Rune executable.

#![cfg(unix)]
#![allow(clippy::expect_used)]

use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::process::{Command, Output};

struct Release {
    scratch: tempfile::TempDir,
    archive: PathBuf,
}

impl Release {
    fn new(payload: Option<&[u8]>, version: &str) -> Self {
        let scratch = tempfile::tempdir().expect("scratch release");
        let directory = scratch.path().join("staged release");
        std::fs::create_dir(&directory).expect("release directory");
        let binary = directory.join("rune");
        if let Some(bytes) = payload {
            std::fs::write(&binary, bytes).expect("fixture executable");
        } else {
            std::fs::copy(env!("CARGO_BIN_EXE_rune"), &binary).expect("real Rune executable");
        }
        let archive = directory.join("rune-fixture.tar.gz");
        let output = Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&directory)
            .arg("rune")
            .output()
            .expect("package fixture archive");
        assert!(output.status.success(), "{output:?}");
        let digest = format!(
            "{:x}",
            Sha256::digest(std::fs::read(&archive).expect("archive bytes"))
        );
        std::fs::write(
            directory.join("rune-fixture.tar.gz.sha256"),
            format!("{digest}  rune-fixture.tar.gz\n"),
        )
        .expect("checksum sidecar");
        std::fs::write(
            directory.join("manifest.json"),
            serde_json::json!({"archive": "rune-fixture.tar.gz", "version": version}).to_string(),
        )
        .expect("release manifest");
        Self { scratch, archive }
    }

    fn smoke(&self) -> Output {
        Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../.github/scripts/test-archive-install.py"
            ))
            .arg(&self.archive)
            .env("RUNE_HOME", self.scratch.path().join("caller state"))
            .env("RUNE_CONFIG", self.scratch.path().join("caller config"))
            .env("RUNE_BENCH", "1")
            .output()
            .expect("run archive install smoke check with Python 3")
    }
}

#[test]
fn documented_archive_install_runs_real_rune_and_preserves_user_state() {
    assert!(
        include_str!("../../../.github/workflows/ci.yml").contains(
            "run: python3 .github/scripts/test-archive-install.py target/dist/*/*.tar.gz"
        ),
        "CI must smoke-test the staged release"
    );
    let release = Release::new(None, env!("CARGO_PKG_VERSION"));
    let output = release.smoke();
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("version matched, binary removed, user state unchanged."),
        "{output:?}"
    );
    assert!(!release.scratch.path().join("caller state").exists());
    assert!(!release.scratch.path().join("caller config").exists());
}

#[test]
fn smoke_rejects_an_archive_with_a_mismatched_checksum() {
    let release = Release::new(None, env!("CARGO_PKG_VERSION"));
    std::fs::write(&release.archive, b"tampered archive").expect("tamper with release");
    let output = release.smoke();
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("FAILED"),
        "{output:?}"
    );
}

#[test]
fn smoke_rejects_an_installed_version_that_disagrees_with_the_manifest() {
    let output = Release::new(None, "0.0.0-wrong").smoke();
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("installed version does not match the staged release manifest"),
        "{output:?}"
    );
}

#[test]
fn smoke_rejects_an_uninstall_that_changes_user_state() {
    let payload = br#"#!/bin/sh
case "$1" in
    --version) printf 'rune 0.0.0 (fixture)\n' ;;
    uninstall) rm -rf "$XDG_STATE_HOME/rune"; rm "$0" ;;
    *) exit 1 ;;
esac
"#;
    let output = Release::new(Some(payload), "0.0.0").smoke();
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("uninstall changed user state"),
        "{output:?}"
    );
}

#[test]
fn smoke_rejects_an_uninstall_that_leaves_the_binary_installed() {
    let payload = b"#!/bin/sh\n[ \"$1\" != --version ] || printf 'rune 0.0.0 (fixture)\\n'\n";
    let output = Release::new(Some(payload), "0.0.0").smoke();
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("uninstall left the installed binary behind"),
        "{output:?}"
    );
}
