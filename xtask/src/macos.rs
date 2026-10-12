//! Certificate-pinned macOS signing and verification of the packaged bytes.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

const IDENTIFIER: &str = "io.github.AadiXC0DE.Rune";

/// The optional signing flag follows the existing positional release arguments.
pub(super) fn release_args(args: &[String]) -> Result<(&[String], Option<String>), String> {
    let (positional, identity) = match args {
        [positional @ .., flag, value] if flag == "--sign-identity" => {
            (positional, Some(identity(value)?))
        }
        _ => (args, None),
    };
    if positional.len() > 3 || positional.iter().any(|arg| arg == "--sign-identity") {
        return Err("usage: cargo xtask release <channel> [version] [target] [--sign-identity <certificate SHA-1>]".to_owned());
    }
    Ok((positional, identity))
}

fn identity(value: &str) -> Result<String, String> {
    if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("macOS signing identity must be a 40-digit certificate SHA-1 fingerprint; ad-hoc signing is not accepted".to_owned());
    }
    Ok(value.to_ascii_uppercase())
}

pub(super) fn sign(binary: &Path, identity: &str) -> Result<Value, String> {
    let signature = serde_json::json!({
        "format": "apple-codesign",
        "identity": identity,
        "identifier": IDENTIFIER,
    });
    codesign(
        &[
            "--force",
            "--sign",
            identity,
            "--timestamp",
            "--options",
            "runtime",
            "--identifier",
            IDENTIFIER,
        ],
        binary,
    )?;
    verify_binary(binary, &signature)?;
    Ok(signature)
}

fn verify_binary(binary: &Path, signature: &Value) -> Result<(), String> {
    if signature["format"] != "apple-codesign" || signature["identifier"] != IDENTIFIER {
        return Err("unsupported macOS release signature format or identifier".to_owned());
    }
    let identity = identity(
        signature["identity"]
            .as_str()
            .ok_or("missing signing identity")?,
    )?;
    // The certificate fingerprint is supplied by the publisher. The code's own
    // designated requirement alone cannot establish who was expected to sign it.
    let requirement =
        format!("=certificate leaf = H\"{identity}\" and identifier \"{IDENTIFIER}\"");
    codesign(
        &[
            "--verify",
            "--strict",
            "--verbose=2",
            "--test-requirement",
            &requirement,
        ],
        binary,
    )
}

fn codesign(args: &[&str], binary: &Path) -> Result<(), String> {
    let output = Command::new("codesign")
        .args(args)
        .arg(binary)
        .output()
        .map_err(|err| format!("could not run macOS codesign: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "macOS release signature check/signing failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// Verifies integrity and the pinned signer without executing the artifact.
pub(super) fn verify_archive(
    archive: &[u8],
    signature: &Value,
    binary: &Value,
) -> Result<(), String> {
    if binary["name"] != "rune" {
        return Err("macOS release executable must be named rune".to_owned());
    }
    let size = binary["bytes"].as_u64().ok_or("missing executable size")?;
    if size > super::targets::MAX_BINARY_BYTES {
        return Err("macOS release executable exceeds the binary size ceiling".to_owned());
    }
    let mut tar = Vec::new();
    flate2::read::MultiGzDecoder::new(archive)
        .take(size.saturating_add(2048).saturating_add(1))
        .read_to_end(&mut tar)
        .map_err(|err| format!("could not decode macOS release archive: {err}"))?;
    let end = 512_usize.saturating_add(size as usize);
    let bytes = tar.get(512..end).ok_or("truncated macOS release archive")?;
    // Accept exactly the deterministic single-file format that release emits.
    // This also rejects extra entries, path traversal and altered tar metadata.
    if tar != super::tar_single("rune", bytes) {
        return Err("macOS release archive is not the expected single executable".to_owned());
    }
    if binary["sha256"].as_str() != Some(super::digest(bytes).as_str()) {
        return Err("macOS release executable checksum mismatch".to_owned());
    }
    let scratch = Scratch::new()?;
    let extracted = scratch.0.join("rune");
    std::fs::write(&extracted, bytes).map_err(|err| format!("could not extract release: {err}"))?;
    verify_binary(&extracted, signature)
}

pub(super) fn verify_release(args: &[String]) -> Result<(), String> {
    let [manifest_path, trusted_identity] = args else {
        return Err(
            "usage: cargo xtask verify-release <manifest-path> <certificate SHA-1>".to_owned(),
        );
    };
    let trusted_identity = identity(trusted_identity)?;
    let path = Path::new(manifest_path);
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(path).map_err(|err| format!("could not read release manifest: {err}"))?,
    )
    .map_err(|err| format!("could not parse release manifest: {err}"))?;
    if !manifest["target"]
        .as_str()
        .is_some_and(|target| target.ends_with("-apple-darwin"))
    {
        return Err("signature verification requires a macOS release target".to_owned());
    }
    let signature = &manifest["signature"];
    if signature["identity"].as_str() != Some(trusted_identity.as_str()) {
        return Err(
            "published macOS signing identity does not match the trusted identity".to_owned(),
        );
    }
    let archive_name = manifest["archive"]
        .as_str()
        .ok_or("missing release archive name")?;
    if archive_name.is_empty()
        || archive_name.contains(['/', '\\'])
        || archive_name == "."
        || archive_name == ".."
    {
        return Err("release archive must be a filename beside the manifest".to_owned());
    }
    let archive = std::fs::read(path.with_file_name(archive_name))
        .map_err(|err| format!("could not read release archive: {err}"))?;
    if manifest["bytes"].as_u64() != Some(archive.len() as u64)
        || manifest["sha256"].as_str() != Some(super::digest(&archive).as_str())
    {
        return Err("macOS release archive checksum or size mismatch".to_owned());
    }
    verify_archive(&archive, signature, &manifest["binary"])?;
    println!("verified macOS release signature: {trusted_identity}");
    Ok(())
}

/// Exclusive temporary extraction, with no dependency or shared staging name.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Self, String> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        for _ in 0..100 {
            let path = std::env::temp_dir().join(format!(
                "rune-signature-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            let builder = {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = builder;
                builder.mode(0o700);
                builder
            };
            match builder.create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(err) => {
                    return Err(format!(
                        "could not create signature staging directory: {err}"
                    ));
                }
            }
        }
        Err("could not reserve signature staging directory".to_owned())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
