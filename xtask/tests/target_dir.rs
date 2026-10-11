#![allow(clippy::expect_used)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

struct ScratchWorkspace(PathBuf);

impl ScratchWorkspace {
    fn new() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "rune-xtask-target-dir-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).expect("create scratch workspace");
        std::fs::create_dir(path.join("src")).expect("create source directory");
        std::fs::write(
            path.join("Cargo.toml"),
            "[package]\nname = \"rune\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n\n[workspace.package]\nversion = \"0.0.0\"\n",
        )
        .expect("write scratch manifest");
        std::fs::write(
            path.join("Cargo.lock"),
            "version = 4\n\n[[package]]\nname = \"rune\"\nversion = \"0.0.0\"\n",
        )
        .expect("write scratch lockfile");
        std::fs::write(
            path.join("src/main.rs"),
            "fn main() { std::fs::write(\"ran-fixture\", b\"fixture\").unwrap(); }\n",
        )
        .expect("write scratch source");
        // A stale default artifact must never be measured or packaged when
        // Cargo writes its actual build into a different directory.
        std::fs::create_dir_all(path.join("target/release")).expect("create decoy directory");
        std::fs::write(
            path.join("target/release")
                .join(format!("rune{}", std::env::consts::EXE_SUFFIX)),
            b"stale default artifact",
        )
        .expect("write decoy artifact");
        Self(path)
    }

    fn task(&self, args: &[&str], target_dir: Option<&Path>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_xtask"));
        command
            .args(args)
            .current_dir(&self.0)
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("CARGO_BUILD_TARGET")
            .env("CARGO_NET_OFFLINE", "true");
        if let Some(directory) = target_dir {
            command.env("CARGO_TARGET_DIR", directory);
        }
        command.output().expect("run xtask")
    }

    fn binary(&self, target_dir: Option<&Path>) -> PathBuf {
        self.0
            .join(target_dir.unwrap_or(Path::new("target")))
            .join("release")
            .join(format!("rune{}", std::env::consts::EXE_SUFFIX))
    }

    fn target_dirs(&self) -> [Option<PathBuf>; 3] {
        [
            Some(self.0.join("absolute build output")),
            Some(PathBuf::from("relative build output")),
            None,
        ]
    }
}

impl Drop for ScratchWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn budget_measures_and_runs_the_binary_in_cargos_target_directory() {
    for index in 0..3 {
        let workspace = ScratchWorkspace::new();
        let dirs = workspace.target_dirs();
        let directory = dirs[index].as_deref();
        let output = workspace.task(&["budget"], directory);
        assert!(output.status.success(), "{directory:?}: {output:?}");
        let size = std::fs::metadata(workspace.binary(directory))
            .expect("read built binary metadata")
            .len();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(&format!("binary size  {size} bytes")),
            "{output:?}"
        );
        assert!(stdout.contains("startup --version"), "{output:?}");
        assert!(stdout.contains("startup --help"), "{output:?}");
        assert!(workspace.0.join("ran-fixture").exists(), "{output:?}");
    }
}

#[test]
fn release_packages_the_binary_in_cargos_target_directory() {
    for index in 0..3 {
        let workspace = ScratchWorkspace::new();
        let dirs = workspace.target_dirs();
        let directory = dirs[index].as_deref();
        let output = workspace.task(&["release"], directory);
        assert!(output.status.success(), "{directory:?}: {output:?}");
        let binary = std::fs::read(workspace.binary(directory)).expect("read built binary");
        let archive = workspace.0.join(format!(
            "target/dist/rune-{0}/rune-0.0.0-{0}.tar.gz",
            env!("TARGET")
        ));
        let compressed = std::fs::read(archive).expect("read staged archive");
        let mut tar = Vec::new();
        flate2::read::GzDecoder::new(compressed.as_slice())
            .read_to_end(&mut tar)
            .expect("decompress staged archive");
        let end = 512_usize.saturating_add(binary.len());
        assert!(tar.len() >= end, "the fresh binary must fit in the archive");
        assert_eq!(
            &tar[512..end],
            binary,
            "the archive must contain the fresh build, not the stale default artifact"
        );
    }
}

#[test]
fn release_manifest_preserves_channels_requiring_json_escaping() {
    use sha2::{Digest, Sha256};

    let workspace = ScratchWorkspace::new();
    let version = "0.0.0-preview";
    let target = env!("TARGET");
    let archive = format!("rune-{version}-{target}.tar.gz");
    let directory = workspace.0.join(format!("target/dist/rune-{target}"));
    for channel in [
        "dev",
        "preview\"candidate",
        "preview\\candidate\n\r\t\u{0001}",
        "\", \"injected\": true, \"channel\": \"other",
        "preview-\u{03bb}",
        "",
    ] {
        let output = workspace.task(&["release", channel, version], None);
        assert!(output.status.success(), "{channel:?}: {output:?}");
        let contents =
            std::fs::read(directory.join("manifest.json")).expect("read staged manifest");
        let manifest: serde_json::Value =
            serde_json::from_slice(&contents).expect("staged manifest must be valid JSON");
        let bytes = std::fs::read(directory.join(&archive)).expect("read staged archive");
        let checksum = format!("{:x}", Sha256::digest(&bytes));
        assert_eq!(
            manifest,
            serde_json::json!({
                "name": "rune",
                "version": version,
                "channel": channel,
                "target": target,
                "archive": archive,
                "bytes": bytes.len(),
                "sha256": checksum,
            }),
            "all manifest fields must retain their values and types"
        );
        assert_eq!(
            std::fs::read_to_string(directory.join(format!("{archive}.sha256")))
                .expect("read staged checksum"),
            format!("{checksum}  {archive}\n")
        );
    }
}

// The reproducible CI job and its SHA-256 utility run on Linux.
#[cfg(target_os = "linux")]
mod reproducible_ci {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn check(mode: &str) -> Output {
        let workspace = ScratchWorkspace::new();
        let tools = workspace.0.join("tools");
        std::fs::create_dir(&tools).expect("create tool directory");
        let wrapper = tools.join("cargo");
        std::fs::write(
            &wrapper,
            r#"#!/usr/bin/env bash
set -euo pipefail
if [[ "$1" != xtask ]]; then
    exec "$RUNE_TEST_CARGO" "$@"
fi
[[ "$*" == 'xtask release dev' ]]
[[ ! -e "$CARGO_TARGET_DIR" ]]
printf '%s\n' "$CARGO_TARGET_DIR" >> build-directories
case "$CARGO_TARGET_DIR" in
    */build-2)
        if [[ "$RUNE_TEST_MODE" == binary ]]; then
            printf '%s\n' 'fn main() { println!("different second build"); }' > src/main.rs
        fi
        ;;
esac
shift
"$RUNE_TEST_XTASK" "$@"
[[ -f "$CARGO_TARGET_DIR/release/rune" ]]
case "$CARGO_TARGET_DIR" in
    */build-2)
        if [[ "$RUNE_TEST_MODE" == archive ]]; then
            # Alter only gzip's timestamp. The extracted binary stays identical.
            for archive in target/dist/*/*.tar.gz; do
                printf '\001' | dd of="$archive" bs=1 seek=4 count=1 conv=notrunc status=none
            done
        fi
        ;;
esac
"#,
        )
        .expect("write cargo wrapper");
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
            .expect("make cargo wrapper executable");
        let path = std::env::join_paths(std::iter::once(tools).chain(std::env::split_paths(
            &std::env::var_os("PATH").expect("PATH"),
        )))
        .expect("prepend test tools to PATH");
        let output = Command::new("bash")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../.github/scripts/check-reproducible-release.sh"
            ))
            .current_dir(&workspace.0)
            .env("PATH", path)
            .env("RUNE_TEST_CARGO", env!("CARGO"))
            .env("RUNE_TEST_XTASK", env!("CARGO_BIN_EXE_xtask"))
            .env("RUNE_TEST_MODE", mode)
            .env("CARGO_NET_OFFLINE", "true")
            .env_remove("CARGO_BUILD_TARGET")
            .output()
            .expect("run the CI reproducibility check");

        let directories = std::fs::read_to_string(workspace.0.join("build-directories"))
            .expect("read actual clean build directories");
        let directories: Vec<_> = directories.lines().map(PathBuf::from).collect();
        assert_eq!(directories.len(), 2, "{output:?}");
        assert_ne!(directories[0], directories[1]);
        for directory in directories {
            assert!(directory.is_absolute());
            assert!(!directory.exists(), "scratch builds must be cleaned up");
        }
        output
    }

    #[test]
    fn ci_accepts_equal_binaries_and_archives_from_two_clean_builds() {
        assert!(
            include_str!("../../.github/workflows/ci.yml")
                .contains("run: bash .github/scripts/check-reproducible-release.sh"),
            "CI must execute the independent build check"
        );
        let output = check("equal");
        assert!(output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains("Independent clean release builds have equal binary and archive hashes."),
            "{output:?}"
        );
    }

    #[test]
    fn ci_rejects_different_independently_built_binaries() {
        let output = check("binary");
        assert!(!output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("independently built release binaries have different hashes"),
            "{output:?}"
        );
    }

    #[test]
    fn ci_rejects_different_archives_even_when_extracted_binaries_match() {
        let output = check("archive");
        assert!(!output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("independently built release archives have different hashes"),
            "{output:?}"
        );
    }
}
