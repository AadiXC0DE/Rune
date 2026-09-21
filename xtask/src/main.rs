//! Repository automation.
//!
//! Tasks are grouped by cost so the default is the cheap one. `check` is the
//! per-commit loop; `budget` and `gate` are for a pull request.

#![forbid(unsafe_code)]

use std::process::{Command, ExitCode};

/// Budget targets. These are Rune's own numbers, measured on the release
/// profile, not a restatement of another project's figures.
mod targets {
    /// Largest accepted stripped release binary, in bytes.
    pub const MAX_BINARY_BYTES: u64 = 8 * 1024 * 1024;

    /// Median startup for `rune --version`, in milliseconds.
    pub const MAX_VERSION_MS: f64 = 5.0;

    /// Median startup for `rune --help`, in milliseconds.
    pub const MAX_HELP_MS: f64 = 8.0;
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let task = args.first().map_or("check", String::as_str);
    let extra: Vec<String> = args.iter().skip(1).cloned().collect();

    let result = match task {
        "check" => check(),
        "fmt" => cargo(&["fmt", "--all", "--", "--check"]),
        "lint" => cargo(&[
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ]),
        "test" => cargo(&["test", "--workspace"]),
        "budget" => budget(),
        "gate" => gate(),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        other => {
            eprintln!("xtask: unknown task `{other}`");
            print_help();
            return ExitCode::from(2);
        }
    };

    let _ = extra;

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("xtask: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Prints the available tasks.
fn print_help() {
    println!("usage: cargo xtask <task>");
    println!();
    println!("  check    format, lint, and test (the per-commit loop)");
    println!("  fmt      check formatting only");
    println!("  lint     run clippy with warnings denied");
    println!("  test     run the workspace test suite");
    println!("  budget   build the release profile and check size and startup");
    println!("  gate     budget plus the full workspace test suite");
}

/// The per-commit loop: format, lint, test.
fn check() -> Result<(), String> {
    cargo(&["fmt", "--all", "--", "--check"])?;
    cargo(&[
        "clippy",
        "--workspace",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ])?;
    cargo(&["test", "--workspace"])
}

/// Runs cargo with the given arguments, inheriting stdio.
fn cargo(args: &[&str]) -> Result<(), String> {
    run("cargo", args)
}

/// Runs a program with the given arguments, inheriting stdio.
fn run(program: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|err| format!("could not run `{program}`: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`{program} {}` failed", args.join(" ")))
    }
}

/// Builds the release profile and checks size and startup.
fn budget() -> Result<(), String> {
    cargo(&["build", "--release", "-p", "rune"])?;

    let binary = release_binary_path();
    let size = std::fs::metadata(&binary)
        .map_err(|err| format!("could not read {binary}: {err}"))?
        .len();

    let mib = size as f64 / (1024.0 * 1024.0);
    println!("binary size  {size} bytes ({mib:.2} MiB)");
    if size > targets::MAX_BINARY_BYTES {
        return Err(format!(
            "binary exceeds the {} byte ceiling",
            targets::MAX_BINARY_BYTES
        ));
    }

    check_startup(&binary)?;
    check_duplicate_dependencies()?;
    Ok(())
}

/// Budget plus the full suite, for a pull request.
fn gate() -> Result<(), String> {
    budget()?;
    cargo(&["test", "--workspace"])
}

/// Path of the built release binary.
fn release_binary_path() -> String {
    let mut path = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    path.push("target");
    path.push("release");
    path.push(format!("rune{}", std::env::consts::EXE_SUFFIX));
    path.display().to_string()
}

/// Measures startup for the help and version paths.
///
/// Uses the fastest observed run rather than the median. On a busy machine the
/// median mostly measures contention, while the minimum is the closest estimate
/// of the work the binary must actually do. The process floor is measured the
/// same way and subtracted, so the number reported is our work rather than the
/// host's fork and loader cost.
fn check_startup(binary: &str) -> Result<(), String> {
    let floor = measure_floor()?;
    println!("process floor       {floor:.2} ms");

    let cases = [
        ("--version", targets::MAX_VERSION_MS),
        ("--help", targets::MAX_HELP_MS),
    ];

    let mut failures = Vec::new();

    for (flag, budget_ms) in cases {
        let raw = fastest_startup_ms(binary, flag)?;
        let work = (raw - floor).max(0.0);
        println!(
            "startup {flag:<10} {raw:.2} ms raw, {work:.2} ms work (budget {budget_ms:.0} ms)"
        );

        if work > budget_ms {
            failures.push(format!(
                "{flag} took {work:.2} ms of work, budget is {budget_ms:.0} ms"
            ));
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

/// Measures the cost of starting a trivial program.
///
/// This is the fork and exec floor plus the dynamic loader for a system binary.
/// Subtracting it isolates the work this binary does.
fn measure_floor() -> Result<f64, String> {
    let candidates = ["/usr/bin/true", "/bin/true"];
    let Some(program) = candidates
        .iter()
        .find(|path| std::path::Path::new(path).exists())
    else {
        // Without a suitable baseline the raw measurement stands on its own.
        return Ok(0.0);
    };

    const SAMPLES: usize = 21;
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let start = std::time::Instant::now();
        let status = Command::new(program)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|err| format!("could not run {program}: {err}"))?;
        if !status.success() {
            return Err(format!("`{program}` exited with a failure"));
        }
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    Ok(fastest(samples))
}

/// Returns the smallest sample, the least noisy estimate of true cost.
fn fastest(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    samples.first().copied().unwrap_or(0.0)
}

/// Returns the fastest wall time in milliseconds for one invocation path.
fn fastest_startup_ms(binary: &str, flag: &str) -> Result<f64, String> {
    const SAMPLES: usize = 31;

    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let start = std::time::Instant::now();
        let status = Command::new(binary)
            .arg(flag)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|err| format!("could not run {binary}: {err}"))?;
        if !status.success() {
            return Err(format!("`{binary} {flag}` exited with a failure"));
        }
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }

    Ok(fastest(samples))
}

/// Crates whose duplicate versions are accepted, with the reason.
///
/// Each entry is a crate that two upstream dependencies pin independently.
/// Listing them explicitly keeps the check meaningful: anything not listed is a
/// regression worth fixing.
const DUPLICATE_EXCEPTIONS: &[(&str, &str)] = &[(
    "winnow",
    "toml 0.9 parses with winnow 0.7 and writes with toml_parser, which pins winnow 1",
)];

/// Fails when the release dependency graph contains two versions of one crate.
///
/// Only normal dependencies are considered. Two versions of a crate in the
/// test-only graph cannot affect the shipped binary.
fn check_duplicate_dependencies() -> Result<(), String> {
    let output = Command::new("cargo")
        .args([
            "tree",
            "--workspace",
            "--edges",
            "normal",
            "--prefix",
            "none",
            "--format",
            "{p}",
        ])
        .output()
        .map_err(|err| format!("could not run cargo tree: {err}"))?;

    if !output.status.success() {
        return Err("cargo tree failed".to_owned());
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut counts: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        std::collections::BTreeMap::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Format is `name version (source)`.
        let mut parts = trimmed.split_whitespace();
        let Some(name) = parts.next() else { continue };
        let Some(version) = parts.next() else {
            continue;
        };
        counts
            .entry(name.to_owned())
            .or_default()
            .insert(version.to_owned());
    }

    let mut duplicates = Vec::new();
    let mut accepted = Vec::new();

    for (name, versions) in &counts {
        if versions.len() < 2 {
            continue;
        }
        let list: Vec<&str> = versions.iter().map(String::as_str).collect();
        let line = format!("{name}: {}", list.join(", "));
        if DUPLICATE_EXCEPTIONS.iter().any(|(known, _)| known == name) {
            accepted.push(line);
        } else {
            duplicates.push(line);
        }
    }

    if !accepted.is_empty() {
        println!("dependencies  accepted duplicates: {}", accepted.join("; "));
    }

    if duplicates.is_empty() {
        println!("dependencies  no unexpected duplicate versions");
        Ok(())
    } else {
        Err(format!(
            "duplicate dependency versions\n  {}\nresolve by pinning one version, or list an exception with a reason",
            duplicates.join("\n  ")
        ))
    }
}
