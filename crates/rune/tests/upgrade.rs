//! Release upgrade regressions through the actual command-line dispatch path.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use flate2::{Compression, write::GzEncoder};
use sha2::{Digest, Sha256};
use std::io::Write as _;
use std::process::{Command, Output};

const MEMBER: &str = if cfg!(windows) { "rune.exe" } else { "rune" };

fn checksum(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn seal_header(header: &mut [u8]) {
    header[148..156].fill(b' ');
    let sum: u32 = header[..512].iter().map(|byte| u32::from(*byte)).sum();
    header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
}

// Mirrors the single-member USTAR format published by `cargo xtask release`.
fn tar(name: &str, payload: &[u8]) -> Vec<u8> {
    let mut header = [0_u8; 512];
    header[..name.len()].copy_from_slice(name.as_bytes());
    header[100..108].copy_from_slice(b"0000755\0");
    header[108..116].copy_from_slice(b"0000000\0");
    header[116..124].copy_from_slice(b"0000000\0");
    header[124..136].copy_from_slice(format!("{:011o}\0", payload.len()).as_bytes());
    header[136..148].copy_from_slice(b"00000000000\0");
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    seal_header(&mut header);

    let mut bytes = header.to_vec();
    bytes.extend_from_slice(payload);
    let padding = 512_usize.saturating_sub(payload.len() % 512) % 512;
    bytes.resize(bytes.len().saturating_add(padding).saturating_add(1024), 0);
    bytes
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(bytes).expect("compress fixture");
    encoder.finish().expect("finish fixture")
}

fn upgrade(root: &std::path::Path, bytes: &[u8], expected: &str) -> Output {
    // No filename suffix is needed to recognize the archive format.
    let source = root.join("release.artifact");
    std::fs::write(&source, bytes).expect("write release");
    Command::new(env!("CARGO_BIN_EXE_rune"))
        .args(["upgrade", "--from"])
        .arg(source)
        .args(["--checksum", expected, "--target"])
        .arg(root.join(MEMBER))
        .arg("--json")
        .env("RUNE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .output()
        .expect("run upgrade")
}

fn assert_refused(bytes: &[u8], expected: &str, code: &str) {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join(MEMBER);
    std::fs::write(&target, b"original").expect("seed target");

    let output = upgrade(dir.path(), bytes, expected);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.starts_with(&format!("rune: {code}:")), "{error}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert_eq!(std::fs::read(target).expect("read target"), b"original");
    assert!(!dir.path().join(".rune-install-staged").exists());
}

#[test]
fn a_verified_release_installs_the_executable_member() {
    for kind in [0, b'0'] {
        for size in [1, 511, 512, 513] {
            let dir = tempfile::tempdir().expect("tempdir");
            let payload = vec![b'x'; size];
            let mut archive = tar(MEMBER, &payload);
            archive[156] = kind;
            seal_header(&mut archive);
            let bytes = gzip(&archive);
            let digest = checksum(&bytes);
            std::fs::write(dir.path().join(MEMBER), b"original").expect("seed target");

            let output = upgrade(dir.path(), &bytes, &digest);
            assert!(output.status.success(), "{output:?}");
            assert!(output.stderr.is_empty(), "{output:?}");
            assert_eq!(std::fs::read(dir.path().join(MEMBER)).unwrap(), payload);
            let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(result["checksum"], digest);
            assert_eq!(result["bytes"], bytes.len());
            assert!(!dir.path().join(".rune-install-staged").exists());
        }
    }
}

#[test]
#[cfg(unix)]
fn upgrading_from_a_release_archive_runs_its_version_command() {
    let dir = tempfile::tempdir().expect("tempdir");
    let payload = b"#!/bin/sh\n[ \"$1\" = version ] || exit 1\nprintf 'rune release-fixture\\n'\n";
    let archive = gzip(&tar(MEMBER, payload));
    std::fs::write(dir.path().join(MEMBER), b"original").expect("seed target");

    let output = upgrade(dir.path(), &archive, &checksum(&archive));
    assert!(output.status.success(), "{output:?}");
    let version = Command::new(dir.path().join(MEMBER))
        .arg("version")
        .output()
        .expect("execute installed member");
    assert!(version.status.success(), "{version:?}");
    assert_eq!(version.stdout, b"rune release-fixture\n");
    assert!(version.stderr.is_empty());
}

#[test]
fn an_archive_requires_the_archive_checksum_before_extraction() {
    let archive = gzip(&tar(MEMBER, b"new binary"));
    assert_refused(&archive, &checksum(b"new binary"), "invalid_state");
    // Verification also precedes decoding a corrupt archive.
    assert_refused(&[0x1f, 0x8b, 0], &checksum(&archive), "invalid_state");
}

#[test]
fn archive_members_cannot_name_other_files_or_links() {
    for name in ["../rune", "/rune", "bin/rune", "other", "./rune"] {
        let bytes = gzip(&tar(name, b"new"));
        assert_refused(&bytes, &checksum(&bytes), "invalid_field");
    }
    for kind in *b"125xL" {
        let mut archive = tar(MEMBER, b"new");
        archive[156] = kind;
        seal_header(&mut archive);
        let bytes = gzip(&archive);
        assert_refused(&bytes, &checksum(&bytes), "invalid_field");
    }
    for offset in [157, 345] {
        let mut archive = tar(MEMBER, b"new");
        archive[offset] = b'x';
        seal_header(&mut archive);
        let bytes = gzip(&archive);
        assert_refused(&bytes, &checksum(&bytes), "invalid_field");
    }
}

#[test]
fn an_archive_must_hold_exactly_one_executable() {
    let mut duplicate = tar(MEMBER, b"new");
    duplicate.truncate(1024);
    duplicate.extend_from_slice(&tar(MEMBER, b"duplicate"));
    for archive in [vec![0; 1024], tar(MEMBER, b""), duplicate] {
        let bytes = gzip(&archive);
        assert_refused(&bytes, &checksum(&bytes), "invalid_field");
    }
}

#[test]
fn corrupt_or_truncated_archives_leave_the_target_untouched() {
    let valid = tar(MEMBER, b"new");
    let mut bad_checksum = valid.clone();
    bad_checksum[100] = b'1';
    let mut bad_size = valid.clone();
    bad_size[124] = b'9';
    seal_header(&mut bad_size);
    let mut bad_padding = valid.clone();
    bad_padding[515] = b'x';
    let mut bad_end = valid.clone();
    bad_end[1536] = b'x';
    let mut excessive_padding = valid.clone();
    excessive_padding.resize(20 * 512 + 1024 + 512, 0);
    for archive in [
        bad_checksum,
        bad_size,
        bad_padding,
        bad_end,
        excessive_padding,
        valid[..511].to_vec(),
        valid[..514].to_vec(),
        valid[..515].to_vec(),
        valid[..1024].to_vec(),
        valid[..1536].to_vec(),
        valid[..2047].to_vec(),
    ] {
        let bytes = gzip(&archive);
        assert_refused(&bytes, &checksum(&bytes), "invalid_field");
    }
    let bytes = gzip(&valid);
    let mut bad_crc = bytes.clone();
    let crc_offset = bad_crc.len().saturating_sub(8);
    bad_crc[crc_offset] ^= 1;
    let mut concatenated = bytes.clone();
    concatenated.extend_from_slice(&bytes);
    let mut trailing_bytes = bytes.clone();
    trailing_bytes.push(0);
    for archive in [
        bad_crc,
        concatenated,
        trailing_bytes,
        bytes[..bytes.len().saturating_sub(1)].to_vec(),
    ] {
        assert_refused(&archive, &checksum(&archive), "invalid_field");
    }
}

#[test]
fn an_oversized_member_is_refused_before_reading_its_payload() {
    let mut archive = tar(MEMBER, b"new");
    let size = (512_u64 * 1024 * 1024).saturating_add(1);
    archive[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
    seal_header(&mut archive);
    let bytes = gzip(&archive);
    assert_refused(&bytes, &checksum(&bytes), "too_large");
}

#[test]
fn conventional_tar_record_padding_is_accepted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut archive = tar(MEMBER, b"new");
    archive.resize(20 * 512, 0);
    let bytes = gzip(&archive);
    let output = upgrade(dir.path(), &bytes, &checksum(&bytes));
    assert!(output.status.success(), "{output:?}");
    assert_eq!(std::fs::read(dir.path().join(MEMBER)).unwrap(), b"new");
}
