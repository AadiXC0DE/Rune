#![allow(clippy::expect_used)]

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

struct ScratchWorkspace(PathBuf);

impl ScratchWorkspace {
    fn new(source: &str) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "rune-xtask-gate-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).expect("create scratch workspace");
        std::fs::create_dir(path.join("src")).expect("create source directory");
        std::fs::write(
            path.join("Cargo.toml"),
            "[package]\nname = \"gate-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
        )
        .expect("write scratch manifest");
        std::fs::write(path.join("src/main.rs"), source).expect("write scratch source");
        Self(path)
    }

    fn gate(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_xtask"))
            .arg("gate")
            .current_dir(&self.0)
            .env("CARGO_TARGET_DIR", self.0.join("target"))
            .env("CARGO_NET_OFFLINE", "true")
            .output()
            .expect("run xtask gate")
    }
}

impl Drop for ScratchWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn formatting_violation_stops_gate_before_lint_budget_and_tests() {
    let source = "fn main(){println!(\"format me\");}\n";
    let workspace = ScratchWorkspace::new(source);
    let output = workspace.gate();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        stderr.contains("xtask: `cargo fmt --all -- --check` failed"),
        "{output:?}"
    );
    assert!(
        !workspace.0.join("target").exists(),
        "formatting failure must stop before compiling lint, budget, or tests"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.0.join("src/main.rs")).expect("read scratch source"),
        source,
        "gate must check formatting without rewriting source"
    );
}

#[test]
fn lint_violation_stops_gate_before_budget_and_tests() {
    let workspace = ScratchWorkspace::new("fn main() {\n    let _ = 1 == 1;\n}\n");
    let output = workspace.gate();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(stderr.contains("clippy::eq_op"), "{output:?}");
    assert!(
        stderr.contains("xtask: `cargo clippy --workspace --all-targets -- -D warnings` failed"),
        "{output:?}"
    );
    assert!(!workspace.0.join("target/release").exists());
    assert!(!stderr.contains("Compiling"), "{output:?}");
    assert!(!stderr.contains("Running"), "{output:?}");
}
