//! Windows ZIP replacement through the real CLI, with portable parser coverage.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::{Command, Output};

fn python() -> &'static str {
    if cfg!(windows) { "python" } else { "python3" }
}

// An independent writer supplies the fixtures, rather than mirroring Rune's ZIP
// implementation. ZIP metadata is fixed so fixture mutations have known offsets.
fn zip(root: &Path, name: &str, payload: &[u8]) -> Vec<u8> {
    let input = root.join("payload");
    let archive = root.join("fixture.zip");
    std::fs::write(&input, payload).expect("payload");
    let output = Command::new(python())
        .args(["-c", "import pathlib,sys,zipfile; i=zipfile.ZipInfo(sys.argv[3]); i.compress_type=8; z=zipfile.ZipFile(sys.argv[2],'w'); z.writestr(i,pathlib.Path(sys.argv[1]).read_bytes()); z.close()"])
        .arg(input).arg(&archive).arg(name)
        .output().expect("Python ZIP writer");
    assert!(output.status.success(), "{output:?}");
    std::fs::read(archive).expect("ZIP bytes")
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn upgrade(root: &Path, bytes: &[u8], checksum: &str) -> Output {
    let source = root.join("release.artifact");
    std::fs::write(&source, bytes).expect("release");
    Command::new(env!("CARGO_BIN_EXE_rune"))
        .args(["upgrade", "--from"])
        .arg(source)
        .args(["--checksum", checksum, "--target"])
        .arg(root.join("rune.exe"))
        .env("RUNE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .output()
        .expect("upgrade")
}

fn assert_refused(root: &Path, bytes: &[u8], checksum: &str, code: &str) {
    let target = root.join("rune.exe");
    std::fs::write(&target, b"keep original").expect("seed target");
    let output = upgrade(root, bytes, checksum);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(code),
        "{output:?}"
    );
    assert_eq!(std::fs::read(target).unwrap(), b"keep original");
    assert!(std::fs::read_dir(root).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".rune-install-staged")
    }));
}

#[test]
fn zip_upgrade_verifies_the_archive_before_extracting_and_replaces_the_member() {
    let scratch = tempfile::tempdir().expect("scratch");
    let root = scratch.path();
    let payload = b"new executable".repeat(64);
    let bytes = zip(root, "rune.exe", &payload);
    assert_refused(root, &bytes, &digest(&payload), "invalid_state");
    let output = upgrade(root, &bytes, &digest(&bytes));
    assert!(output.status.success(), "{output:?}");
    assert_eq!(std::fs::read(root.join("rune.exe")).unwrap(), payload);
}

#[test]
fn malformed_zip_members_are_refused_without_replacing_the_target() {
    let scratch = tempfile::tempdir().expect("scratch");
    let root = scratch.path();
    for name in [
        "../rune.exe",
        "bin/rune.exe",
        "other.exe",
        "rune",
        "/rune.exe",
        "rune.exe/",
    ] {
        let bytes = zip(root, name, b"new");
        assert_refused(root, &bytes, &digest(&bytes), "invalid_field");
    }
    let empty = zip(root, "rune.exe", b"");
    assert_refused(root, &empty, &digest(&empty), "invalid_field");
    let bytes = zip(root, "rune.exe", b"new binary");
    let central = bytes.len().saturating_sub(76);
    let end = bytes.len().saturating_sub(22);
    // Corrupt CRC in both copies; flags; method; filename; directory metadata;
    // symlink attributes; duplicate entries; offsets; empty and oversize members.
    for changes in [
        vec![(14, 1), (central + 16, 1)],
        vec![(6, 1)],
        vec![(8, 1)],
        vec![(central + 46, 1)],
        vec![(central + 30, 1)],
        vec![(central + 41, 0xa0)],
        vec![(end + 10, 2)],
        vec![(end + 16, 1)],
        vec![(22, 1)],
    ] {
        let mut corrupt = bytes.clone();
        for (offset, value) in changes {
            corrupt[offset] ^= value;
        }
        assert_refused(root, &corrupt, &digest(&corrupt), "invalid_field");
    }
    for length in [2, 29, 37, central, bytes.len().saturating_sub(1)] {
        let truncated = &bytes[..length];
        assert_refused(root, truncated, &digest(truncated), "invalid_field");
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert_refused(root, &trailing, &digest(&trailing), "invalid_field");

    // Even when both size fields agree, decoding must not exceed the declared
    // size, accept extra compressed data, or accept a stream missing its end.
    let mut too_small = bytes.clone();
    too_small[22..26].copy_from_slice(&1_u32.to_le_bytes());
    too_small[central + 24..central + 28].copy_from_slice(&1_u32.to_le_bytes());
    assert_refused(root, &too_small, &digest(&too_small), "invalid_field");
    for removed in [false, true] {
        let mut corrupt = bytes.clone();
        let packed = central.saturating_sub(38);
        let new_packed = if removed {
            corrupt.remove(central.saturating_sub(1));
            packed.saturating_sub(1)
        } else {
            corrupt.insert(central, 0);
            packed.saturating_add(1)
        };
        let new_central = 38_usize.saturating_add(new_packed);
        let new_end = new_central.saturating_add(54);
        corrupt[18..22].copy_from_slice(&(new_packed as u32).to_le_bytes());
        corrupt[new_central + 20..new_central + 24]
            .copy_from_slice(&(new_packed as u32).to_le_bytes());
        corrupt[new_end + 16..new_end + 20].copy_from_slice(&(new_central as u32).to_le_bytes());
        assert_refused(root, &corrupt, &digest(&corrupt), "invalid_field");
    }
    let mut oversize = bytes;
    oversize[22..26].copy_from_slice(&(512_u32 * 1024 * 1024 + 1).to_le_bytes());
    assert_refused(root, &oversize, &digest(&oversize), "too_large");
}

fn smoke(root: &Path, archive: &Path) -> Output {
    Command::new(python())
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../.github/scripts/test-windows-release.py"
        ))
        .arg(archive)
        .env("RUNE_HOME", root.join("caller state"))
        .env("RUNE_CONFIG", root.join("caller config"))
        .env("RUNE_BENCH", "1")
        .output()
        .expect("Windows release smoke")
}

#[test]
fn windows_release_smoke_extracts_runs_and_upgrades_a_real_executable() {
    let scratch = tempfile::tempdir().expect("scratch");
    let root = scratch.path();
    let executable = std::fs::read(env!("CARGO_BIN_EXE_rune")).expect("native executable");
    // On Windows these are native PE binaries. On other hosts the same ZIP and
    // smoke harness are exercised using native executables named rune.exe.
    let bytes = zip(root, "rune.exe", &executable);
    let archive = root.join("fixture.zip");
    let manifest = serde_json::json!({
        "archive": "fixture.zip", "version": env!("CARGO_PKG_VERSION"),
        "target": "x86_64-pc-windows-msvc", "bytes": bytes.len(), "sha256": digest(&bytes),
        "binary": {"name": "rune.exe", "bytes": executable.len(), "sha256": digest(&executable)},
    });
    std::fs::write(root.join("manifest.json"), manifest.to_string()).expect("manifest");
    std::fs::write(
        root.join("fixture.zip.sha256"),
        format!("{}  fixture.zip\n", digest(&bytes)),
    )
    .expect("sidecar");
    let output = smoke(root, &archive);
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("old executable replaced, new version ran."),
        "{output:?}"
    );
    assert!(!root.join("caller state").exists());
    assert!(!root.join("caller config").exists());
    let mut wrong_version = manifest;
    wrong_version["version"] = serde_json::json!("0.0.0-wrong");
    std::fs::write(root.join("manifest.json"), wrong_version.to_string()).expect("wrong version");
    let output = smoke(root, &archive);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("extracted version does not match"),
        "{output:?}"
    );
    std::fs::write(&archive, b"tampered").expect("tamper");
    let output = smoke(root, &archive);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("archive checksum mismatch"),
        "{output:?}"
    );
}
