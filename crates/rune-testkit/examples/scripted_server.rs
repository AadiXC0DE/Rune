//! Serves a scenario's scripted provider for a binary run somewhere else,
//! such as inside a container, and checks the workspace it left.
//!
//! ```text
//! scripted_server <scenario.json> --workspace DIR
//!     writes the scenario workspace into DIR, prints {"port": N, "prompt": "..."}
//!     and serves until standard input closes
//! scripted_server --check <scenario.json> --workspace DIR
//!     prints `ok` and exits 0 when DIR holds the expected files, else the
//!     first difference and exits 1
//! ```
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rune_testkit::scenario;
use rune_testkit::scripted::ScriptedProvider;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (check, rest) = match args.split_first() {
        Some((first, rest)) if first == "--check" => (true, rest),
        _ => (false, args.as_slice()),
    };
    let [path, flag, workspace] = rest else {
        return usage();
    };
    if flag != "--workspace" {
        return usage();
    }
    let loaded = match scenario::load(Path::new(path)) {
        Ok(loaded) => loaded,
        Err(err) => {
            eprintln!("scripted_server: {err}");
            return ExitCode::from(2);
        }
    };
    let workspace = PathBuf::from(workspace);
    if check {
        return compare(&loaded, &workspace);
    }
    for (relative, contents) in &loaded.workspace {
        let target = workspace.join(relative);
        if let Some(parent) = target.parent()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            eprintln!("scripted_server: {}: {err}", parent.display());
            return ExitCode::from(2);
        }
        if let Err(err) = std::fs::write(&target, contents) {
            eprintln!("scripted_server: {}: {err}", target.display());
            return ExitCode::from(2);
        }
    }
    let provider = ScriptedProvider::start(loaded.turns.clone());
    let port = provider
        .base_url()
        .rsplit(':')
        .next()
        .and_then(|tail| tail.split('/').next())
        .and_then(|port| port.parse::<u16>().ok())
        .unwrap_or_default();
    println!(
        "{}",
        serde_json::json!({"port": port, "prompt": loaded.prompt})
    );
    let _ = std::io::stdout().flush();
    // Serve until whoever started this closes its standard input.
    let mut sink = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut sink);
    drop(provider);
    ExitCode::SUCCESS
}

/// Compares the workspace with the scenario's expected files.
fn compare(loaded: &scenario::Scenario, workspace: &Path) -> ExitCode {
    for (relative, want) in &loaded.expect.files {
        let found = std::fs::read_to_string(workspace.join(relative)).ok();
        if found.as_deref() != want.as_deref() {
            println!("{relative}: expected {want:?}, found {found:?}");
            return ExitCode::from(1);
        }
    }
    println!("ok");
    ExitCode::SUCCESS
}

fn usage() -> ExitCode {
    eprintln!("usage: scripted_server [--check] <scenario.json> --workspace DIR");
    ExitCode::from(2)
}
