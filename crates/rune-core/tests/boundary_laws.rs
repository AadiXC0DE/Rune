//! Boundary laws enforced by the dependency graph.
//!
//! The architectural laws in the design are only real if they are checked. This
//! test parses the workspace manifests and fails when a crate gains a dependency
//! that violates its layer.

// Integration tests assert by panicking. The guards that forbid panicking
// apply to shipped code, where a panic on user input is a defect.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeSet;
use std::path::Path;

/// Crates that must not appear in `rune-core`, because linking them would make
/// command dispatch and configuration resolution pay for the agent runtime.
const FORBIDDEN_IN_CORE: &[&str] = &[
    "tokio",
    "reqwest",
    "crossterm",
    "ratatui-core",
    "ratatui",
    "hyper",
    "rustls",
    "ignore",
    "globset",
    "grep-searcher",
];

/// Internal crates, by manifest name.
const INTERNAL_CRATES: &[&str] = &[
    "rune-core",
    "rune-net",
    "rune-policy",
    "rune-exec",
    "rune-tools",
    "rune-agent",
    "rune-session",
    "rune-context",
    "rune-term",
    "rune-acp",
    "rune-sdk",
    "rune-testkit",
];

/// Workspace root, resolved from this crate's manifest directory.
fn workspace_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

/// Reads a crate manifest.
fn manifest(crate_name: &str) -> String {
    let path = workspace_root()
        .join("crates")
        .join(crate_name)
        .join("Cargo.toml");
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
}

/// Returns the dependency names declared in a manifest, from every section.
fn declared_dependencies(crate_name: &str) -> BTreeSet<String> {
    let text = manifest(crate_name);
    let value: toml::Value = toml::from_str(&text).expect("parse manifest");
    let mut names = BTreeSet::new();

    let Some(table) = value.as_table() else {
        return names;
    };

    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(deps) = table.get(section).and_then(toml::Value::as_table) {
            for key in deps.keys() {
                names.insert(key.clone());
            }
        }
    }

    // Inline target-specific dependencies.
    if let Some(target) = table.get("target").and_then(toml::Value::as_table) {
        for spec in target.values() {
            if let Some(deps) = spec.get("dependencies").and_then(toml::Value::as_table) {
                for key in deps.keys() {
                    names.insert(key.clone());
                }
            }
        }
    }

    names
}

#[test]
fn core_does_not_depend_on_runtime_heavy_crates() {
    let deps = declared_dependencies("rune-core");
    for forbidden in FORBIDDEN_IN_CORE {
        assert!(
            !deps.contains(*forbidden),
            "rune-core declares `{forbidden}`, which would load it onto the startup path"
        );
    }
}

#[test]
fn core_depends_on_no_other_workspace_crate() {
    let deps = declared_dependencies("rune-core");
    for internal in INTERNAL_CRATES {
        if *internal == "rune-core" {
            continue;
        }
        assert!(
            !deps.contains(*internal),
            "rune-core declares `{internal}`; it must sit below every other crate"
        );
    }
}

#[test]
fn policy_depends_only_on_core() {
    let deps = declared_dependencies("rune-policy");
    for internal in INTERNAL_CRATES {
        if matches!(*internal, "rune-core" | "rune-policy") {
            continue;
        }
        assert!(
            !deps.contains(*internal),
            "rune-policy declares `{internal}`; the policy engine must stay pure"
        );
    }
}

#[test]
fn net_does_not_depend_on_product_state_crates() {
    let deps = declared_dependencies("rune-net");
    for forbidden in [
        "rune-tools",
        "rune-policy",
        "rune-session",
        "rune-context",
        "rune-term",
    ] {
        assert!(
            !deps.contains(forbidden),
            "rune-net declares `{forbidden}`; the transport layer must speak protocols only"
        );
    }
}

#[test]
fn tools_do_not_depend_on_renderer_or_transport() {
    let deps = declared_dependencies("rune-tools");
    for forbidden in ["rune-term", "rune-net"] {
        assert!(
            !deps.contains(forbidden),
            "rune-tools declares `{forbidden}`; tools receive a context, they do not reach out"
        );
    }
}

#[test]
fn only_the_binary_depends_on_the_renderer() {
    let crates_dir = workspace_root().join("crates");
    let mut offenders = Vec::new();

    let entries = std::fs::read_dir(&crates_dir).expect("read crates dir");
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("rune-") || name == "rune-term" || name == "rune" {
            continue;
        }
        // The testkit may render for golden-frame assertions.
        if name == "rune-testkit" {
            continue;
        }
        if declared_dependencies(&name).contains("rune-term") {
            offenders.push(name);
        }
    }

    assert!(
        offenders.is_empty(),
        "these crates depend on the renderer: {offenders:?}"
    );
}

#[test]
fn every_crate_declares_workspace_lints() {
    let entries = std::fs::read_dir(workspace_root().join("crates")).expect("read crates dir");
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let manifest_path = entry.path().join("Cargo.toml");
        if !manifest_path.exists() {
            continue;
        }
        let text = std::fs::read_to_string(&manifest_path).expect("read");
        assert!(
            text.contains("[lints]") && text.contains("workspace = true"),
            "crate `{name}` does not inherit workspace lints"
        );
    }
}

#[test]
fn every_crate_uses_workspace_package_fields() {
    let entries = std::fs::read_dir(workspace_root().join("crates")).expect("read crates dir");
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let manifest_path = entry.path().join("Cargo.toml");
        if !manifest_path.exists() {
            continue;
        }
        let text = std::fs::read_to_string(&manifest_path).expect("read");
        for field in ["version.workspace = true", "edition.workspace = true"] {
            assert!(
                text.contains(field),
                "crate `{name}` does not inherit `{field}`"
            );
        }
    }
}
