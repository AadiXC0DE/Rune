//! Installing and replacing the binary.
//!
//! An artifact is only installed when its checksum matches a value supplied with
//! it, so a tampered or truncated download is refused before it can replace
//! anything. Replacement is staged beside the target and renamed over it, so the
//! old binary is either wholly replaced or wholly kept, never half of each.
//!
//! A local artifact installs without any network access, which is what makes the
//! same code path usable on a machine that has none.

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::error::{ErrorCode, Result, RuneError};
use sha2::{Digest, Sha256};

/// Largest artifact accepted, in bytes.
pub const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;

/// Where an artifact comes from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Source {
    /// A file already on this machine.
    Local(Utf8PathBuf),
    /// A URL to fetch.
    Remote(String),
}

impl Source {
    /// Parses a source from its written form.
    ///
    /// A value with a scheme is remote, and anything else is a path, so an
    /// air-gapped install is spelled by naming a file rather than by a flag that
    /// could be forgotten.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        let trimmed = raw.trim();
        if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
            Self::Remote(trimmed.to_owned())
        } else {
            Self::Local(Utf8PathBuf::from(trimmed))
        }
    }

    /// Returns true when this source needs the network.
    #[must_use]
    pub const fn is_remote(&self) -> bool {
        matches!(self, Self::Remote(_))
    }
}

/// A verified artifact, held in memory until it is installed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Artifact {
    /// The bytes.
    pub bytes: Vec<u8>,
    /// Lowercase hexadecimal digest of those bytes.
    pub digest: String,
}

impl Artifact {
    /// Builds an artifact from bytes.
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        let digest = digest_of(&bytes);
        Self { bytes, digest }
    }

    /// Reads an artifact from a file.
    pub fn from_file(path: &Utf8Path) -> Result<Self> {
        let metadata = std::fs::metadata(path).map_err(|err| {
            RuneError::new(
                ErrorCode::NotFound,
                format!("`{path}` could not be read: {err}"),
            )
            .with_hint("check the path to the artifact")
        })?;
        if metadata.len() > MAX_ARTIFACT_BYTES {
            return Err(RuneError::too_large(
                "artifact",
                usize::try_from(metadata.len()).unwrap_or(usize::MAX),
                usize::try_from(MAX_ARTIFACT_BYTES).unwrap_or(usize::MAX),
            ));
        }
        let bytes = std::fs::read(path).map_err(RuneError::from)?;
        Ok(Self::new(bytes))
    }

    /// Checks the artifact against an expected digest.
    ///
    /// The comparison is case-insensitive, because a published checksum is often
    /// uppercase and a user pasting one should not have to care.
    pub fn verify(&self, expected: &str) -> Result<()> {
        let expected = expected.trim().to_ascii_lowercase();
        if expected.is_empty() {
            return Err(
                RuneError::invalid_field("checksum", "a checksum is required to install")
                    .with_hint("pass the published checksum for this artifact"),
            );
        }
        if !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(RuneError::invalid_field(
                "checksum",
                format!("`{expected}` is not a hexadecimal digest"),
            ));
        }
        if self.digest != expected {
            return Err(RuneError::new(
                ErrorCode::InvalidState,
                format!(
                    "the artifact does not match its checksum: computed {}, expected {expected}",
                    self.digest
                ),
            )
            .with_hint("download the artifact again from the published location"));
        }
        Ok(())
    }
}

/// Returns the path a local source reads from.
///
/// Present so the local path is one obvious function rather than a match arm at
/// each call site, which is what keeps an air-gapped install from growing a
/// network call by accident.
#[must_use]
pub fn local_path(source: &Source) -> &str {
    match source {
        Source::Local(path) => path.as_str(),
        Source::Remote(url) => url.as_str(),
    }
}

/// Returns the lowercase hexadecimal SHA-256 digest of some bytes.
#[must_use]
pub fn digest_of(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Replaces the binary at `target` with a verified artifact.
///
/// The staged file is written beside the target and renamed over it, so a crash
/// or a failed write leaves the original in place. The executable bit is set on
/// the staged file before the rename, because a binary that cannot be executed
/// is not an installed binary.
pub fn install(target: &Utf8Path, artifact: &Artifact, expected: &str) -> Result<()> {
    artifact.verify(expected)?;

    let parent = target.parent().ok_or_else(|| {
        RuneError::invalid_field("target", format!("`{target}` has no parent directory"))
    })?;
    let staged = parent.join(".rune-install-staged");

    // Written first, then checked, then renamed. A failure at any point before
    // the rename leaves the original untouched.
    let written = write_staged(&staged, &artifact.bytes);
    if let Err(err) = written {
        let _ = std::fs::remove_file(&staged);
        return Err(err);
    }

    if let Err(err) = set_executable(&staged) {
        let _ = std::fs::remove_file(&staged);
        return Err(err);
    }

    std::fs::rename(&staged, target).map_err(|err| {
        let _ = std::fs::remove_file(&staged);
        RuneError::new(
            ErrorCode::PermissionDenied,
            format!("`{target}` could not be replaced: {err}"),
        )
        .with_hint("check that the directory is writable and the binary is not running")
    })
}

/// Writes the staged file, refusing to leave a partial one behind.
fn write_staged(path: &Utf8Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::File::create(path).map_err(|err| {
        RuneError::new(
            ErrorCode::PermissionDenied,
            format!("`{path}` could not be created: {err}"),
        )
    })?;
    file.write_all(bytes).map_err(|err| {
        RuneError::new(
            ErrorCode::TransportFailure,
            format!("`{path}` could not be written: {err}"),
        )
    })?;
    file.sync_all().map_err(RuneError::from)
}

/// Marks a file executable.
#[cfg(unix)]
fn set_executable(path: &Utf8Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).map_err(RuneError::from)
}

/// Marks a file executable.
#[cfg(not(unix))]
fn set_executable(_path: &Utf8Path) -> Result<()> {
    // The platform decides executability from the file name.
    Ok(())
}

/// Removes the binary at `target`.
///
/// Reports whether a file was there, so a caller can distinguish a removal from
/// a path that was already empty.
pub fn remove_binary(target: &Utf8Path) -> Result<bool> {
    match std::fs::remove_file(target) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(RuneError::new(
            ErrorCode::PermissionDenied,
            format!("`{target}` could not be removed: {err}"),
        )
        .with_hint("check that the directory is writable and the binary is not running")),
    }
}

/// Removes a directory and everything under it.
///
/// Reports what was removed, so a caller can say what happened rather than
/// claiming a removal that did not occur.
pub fn remove_state(root: &Utf8Path) -> Result<bool> {
    match std::fs::remove_dir_all(root) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(RuneError::new(
            ErrorCode::PermissionDenied,
            format!("`{root}` could not be removed: {err}"),
        )),
    }
}

/// Returns the path the running binary was started from.
///
/// Used as the default install target, so an upgrade replaces the binary the
/// user actually ran rather than a copy somewhere else.
pub fn current_binary() -> Result<Utf8PathBuf> {
    let path = std::env::current_exe().map_err(|err| {
        RuneError::new(
            ErrorCode::NotFound,
            format!("the running binary could not be located: {err}"),
        )
    })?;
    Utf8PathBuf::from_path_buf(path).map_err(|_| {
        RuneError::new(
            ErrorCode::UnsafePath,
            "the running binary is not at a UTF-8 path",
        )
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;

    fn artifact_dir() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        (dir, root)
    }

    #[test]
    fn a_source_names_a_file_or_a_url() {
        assert_eq!(
            Source::parse("/tmp/rune.tar.gz"),
            Source::Local(Utf8PathBuf::from("/tmp/rune.tar.gz"))
        );
        assert_eq!(
            Source::parse("https://example.test/rune.tar.gz"),
            Source::Remote(String::from("https://example.test/rune.tar.gz"))
        );
        assert!(Source::parse("https://example.test/x").is_remote());
        assert!(!Source::parse("./rune.tar.gz").is_remote());
    }

    #[test]
    fn a_checksum_round_trips_through_its_written_form() {
        let artifact = Artifact::new(b"payload".to_vec());
        artifact.verify(&artifact.digest).expect("verified");
        // A published checksum is often uppercase.
        artifact
            .verify(&artifact.digest.to_ascii_uppercase())
            .expect("verified");
    }

    #[test]
    fn a_tampered_artifact_is_refused() {
        let artifact = Artifact::new(b"payload".to_vec());
        let err = artifact
            .verify(&digest_of(b"different"))
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidState);
        assert!(err.hint().is_some());
    }

    #[test]
    fn a_missing_checksum_is_refused() {
        // Installing without one would make verification optional, which is the
        // same as not having it.
        let artifact = Artifact::new(b"payload".to_vec());
        let err = artifact.verify("").expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
    }

    #[test]
    fn a_checksum_that_is_not_hexadecimal_is_refused() {
        let artifact = Artifact::new(b"payload".to_vec());
        let err = artifact.verify("not a digest").expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
    }

    #[test]
    fn a_verified_artifact_replaces_the_target() {
        let (_dir, root) = artifact_dir();
        let target = root.join("rune");
        std::fs::write(&target, b"old").expect("seeded");

        let artifact = Artifact::new(b"new binary".to_vec());
        install(&target, &artifact, &artifact.digest).expect("installed");

        assert_eq!(std::fs::read(&target).expect("read"), b"new binary");
        assert!(
            !root.join(".rune-install-staged").exists(),
            "the staged file was left behind"
        );
    }

    #[test]
    fn a_refused_install_leaves_the_original_untouched() {
        // A checksum failure must not have replaced anything, even partially.
        let (_dir, root) = artifact_dir();
        let target = root.join("rune");
        std::fs::write(&target, b"original").expect("seeded");

        let artifact = Artifact::new(b"tampered".to_vec());
        let err = install(&target, &artifact, &digest_of(b"expected")).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidState);
        assert_eq!(std::fs::read(&target).expect("read"), b"original");
        assert!(
            !root.join(".rune-install-staged").exists(),
            "a stage file was left behind"
        );
    }

    #[test]
    fn an_installed_binary_is_executable() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let (_dir, root) = artifact_dir();
            let target = root.join("rune");
            let artifact = Artifact::new(b"binary".to_vec());
            install(&target, &artifact, &artifact.digest).expect("installed");

            let mode = std::fs::metadata(&target)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o755, "the installed binary is not executable");
        }
    }

    #[test]
    fn a_replacement_never_leaves_a_half_written_file() {
        // The target always holds either the old bytes or the new ones, which is
        // what the staged rename buys.
        let (_dir, root) = artifact_dir();
        let target = root.join("rune");
        std::fs::write(&target, b"v1").expect("seeded");

        for index in 0..5 {
            let payload = format!("v{}", index + 2).into_bytes();
            let artifact = Artifact::new(payload.clone());
            install(&target, &artifact, &artifact.digest).expect("installed");
            assert_eq!(std::fs::read(&target).expect("read"), payload);
        }
    }

    #[test]
    fn installing_twice_over_the_same_target_succeeds() {
        let (_dir, root) = artifact_dir();
        let target = root.join("rune");
        for payload in [b"one".to_vec(), b"two".to_vec()] {
            let artifact = Artifact::new(payload.clone());
            install(&target, &artifact, &artifact.digest).expect("installed");
            assert_eq!(std::fs::read(&target).expect("read"), payload);
        }
    }

    #[test]
    fn reading_an_artifact_from_a_file_matches_its_bytes() {
        let (_dir, root) = artifact_dir();
        let path = root.join("artifact.bin");
        std::fs::write(&path, b"contents").expect("write");

        let artifact = Artifact::from_file(&path).expect("read");
        assert_eq!(artifact.bytes, b"contents");
        assert_eq!(artifact.digest, digest_of(b"contents"));
    }

    #[test]
    fn reading_a_missing_artifact_names_the_path() {
        let (_dir, root) = artifact_dir();
        let err = Artifact::from_file(&root.join("absent")).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::NotFound);
        assert!(err.message().contains("absent"), "{}", err.message());
    }

    #[test]
    fn removing_a_binary_reports_whether_one_was_there() {
        let (_dir, root) = artifact_dir();
        let target = root.join("rune");
        assert!(!remove_binary(&target).expect("checked"));
        std::fs::write(&target, b"binary").expect("write");
        assert!(remove_binary(&target).expect("removed"));
        assert!(!target.exists());
    }

    #[test]
    fn removing_state_removes_what_is_there_and_reports_a_miss() {
        let (_dir, root) = artifact_dir();
        let state = root.join("state/sessions");
        std::fs::create_dir_all(&state).expect("mkdir");
        std::fs::write(state.join("a.log"), b"x").expect("write");

        assert!(remove_state(&root.join("state")).expect("removed"));
        assert!(!root.join("state").exists());
        assert!(!remove_state(&root.join("state")).expect("checked"));
    }

    #[test]
    fn a_local_source_needs_no_network() {
        // An air-gapped install is spelled by naming a file, so the path that
        // reads one must not consult a remote anything. The check is that the
        // local source carries no URL at all, which is what makes the install
        // possible with no route out.
        let local = Source::parse("/srv/artifacts/rune.tar.gz");
        assert!(!local.is_remote());
        assert_eq!(
            local_path(&local),
            "/srv/artifacts/rune.tar.gz",
            "a local source was resolved through something other than its path"
        );
    }

    #[test]
    fn a_remote_source_is_recognized_rather_than_treated_as_a_path() {
        // Reading a URL as a filename would produce a confusing not-found.
        let remote = Source::parse("https://example.test/rune");
        assert!(remote.is_remote());
    }

    #[test]
    fn the_digest_of_known_bytes_is_stable() {
        // A published checksum is only useful if the algorithm does not change.
        assert_eq!(
            digest_of(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
