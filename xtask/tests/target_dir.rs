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

    fn task(&self, task: &str, target_dir: Option<&Path>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_xtask"));
        command
            .arg(task)
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
        let output = workspace.task("budget", directory);
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
        let output = workspace.task("release", directory);
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
