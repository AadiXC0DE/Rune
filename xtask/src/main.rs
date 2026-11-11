//! Repository automation.
//!
//! Tasks are grouped by cost so the default is the cheap one. `check` is the
//! per-commit loop; `budget` and `gate` are for a pull request.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

mod fixture;
mod macos;
mod windows;

/// Serial runs per measurement, used for the minimum, median and p95.
const MEASUREMENT_SAMPLES: usize = 31;

/// Enforced release budget ceilings, distinct from measured build results.
mod targets {
    /// Largest accepted stripped release binary, in bytes.
    pub const MAX_BINARY_BYTES: u64 = 8 * 1024 * 1024;

    /// Baseline-subtracted minimum-of-31 work for `rune --version`, in milliseconds.
    pub const MAX_VERSION_MS: f64 = 2.0;

    /// Baseline-subtracted minimum-of-31 work for `rune --help`, in milliseconds.
    pub const MAX_HELP_MS: f64 = 2.0;

    /// Largest accepted assembled system prompt, in bytes.
    ///
    /// Shared with the crate that compiles the prompt, so the gate and the
    /// compiled-in floor cannot drift apart.
    pub const MAX_PROMPT_BYTES: usize = rune_core::budget::PROMPT_CEILING_BYTES;

    /// Largest accepted advertised tool schema set, in bytes.
    pub const MAX_TOOL_BYTES: usize = rune_core::budget::TOOL_SCHEMA_CEILING_BYTES;

    /// Divisor used to turn prompt bytes into an estimated token count.
    ///
    /// Four is the estimator's working bytes-per-token figure, not a tokenizer:
    /// the number printed beside the byte count is an estimate at this divisor.
    pub const PROMPT_BYTES_PER_TOKEN: usize = 4;

    /// Largest accepted first request of the scripted `fix_one_file` session, in bytes,
    /// with the workspace path replaced by `<workspace>`.
    pub const MAX_SCRIPTED_FIRST_REQUEST_BYTES: usize = 8966;

    /// Largest accepted sum of every request body in that session, in bytes.
    pub const MAX_SCRIPTED_SESSION_BYTES: usize = 38035;

    /// Largest accepted number of requests in that session.
    pub const MAX_SCRIPTED_REQUESTS: usize = 4;

    /// Requests in that session whose cached share falls under one half.
    pub const MAX_SCRIPTED_PREFIX_BREAKS: usize = 0;
}

/// Measured on macOS arm64; other hosts print their numbers without a gate.
mod perf_baseline {
    pub const LONG_TURN_PEAK_RSS_BYTES: u64 = 20_238_000;
    pub const LONG_TURN_CPU_SECONDS: f64 = 2.98;
    pub const FIRST_FRAME_MEDIAN_MS: f64 = 52.5;
    /// A measurement more than this fraction above its baseline fails.
    pub const ALLOWED_REGRESSION: f64 = 0.25;
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
        "budget"
            if extra
                .first()
                .is_some_and(|arg| arg == "--help" || arg == "-h") =>
        {
            print_budget_help();
            Ok(())
        }
        "budget" => budget(),
        "perf" => perf(&extra),
        "gate" => gate(),
        "release" => release(&extra),
        "verify-release" => macos::verify_release(&extra),
        "benchmark" => benchmark(&extra),
        "web" => web(),
        "verify-web" => verify_web(&extra),
        "fixture" => fixture::run(&extra),
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
    println!(
        "  perf     measure long-turn memory and CPU and time to first frame against stored baselines"
    );
    println!("  gate     format, lint, budget, and the full workspace test suite");
    println!("  release  stage a release artifact, its checksum, and a manifest");
    println!("  benchmark  print raw size and startup JSON for an existing executable");
    println!("  verify-release  check a signed macOS archive against a trusted identity");
    println!("  web      build the harness for the landing page into site/demo/rune.wasm");
    println!("  verify-web  fail unless site/demo/rune.wasm matches a fresh build");
    println!("  fixture  copy one traced provider exchange into a conformance fixture");
    println!();
    println!(
        "  release takes: cargo xtask release <channel> [version] [target] [--sign-identity <certificate SHA-1>]"
    );
    println!(
        "  verify-release takes: cargo xtask verify-release <manifest-path> <certificate SHA-1>"
    );
    println!("  benchmark takes: cargo xtask benchmark <executable-path>");
    println!("  fixture takes: cargo xtask fixture <trace-dir> <name> [--pair NNNN]");
    println!();
    print_budget_help();
}

/// Explains the measured reference build and the enforced size ceiling.
fn print_budget_help() {
    println!("usage: cargo xtask budget [--help]");
    println!();
    println!("  Builds the release profile and checks size and startup.");
    println!("  The audited stripped release build measured 5.45 MiB for");
    println!("  x86_64-unknown-linux-gnu (GNU/Linux).");
    println!(
        "  The enforced binary size budget is an {} MiB ceiling ({} bytes).",
        targets::MAX_BINARY_BYTES / (1024 * 1024),
        targets::MAX_BINARY_BYTES
    );
    println!("  Startup reports raw process latency: minimum-of-31, median and p95.");
    println!("  p95 uses nearest rank (the 30th sorted sample of 31).");
    println!("  Work is minimum-of-31 minus the process baseline minimum-of-31, clamped to zero.");
    println!("  The baseline is system true on Unix, an empty Rust executable on Windows.");
    println!(
        "  If no baseline is available, the budget fails: startup work ceilings cannot be enforced."
    );
    println!(
        "  Work budgets: --version {} ms, --help {} ms.",
        targets::MAX_VERSION_MS,
        targets::MAX_HELP_MS
    );
    println!("  The assembled system prompt is measured from this working tree and its");
    println!(
        "  fixed per-request cost is checked against a {} byte ceiling.",
        targets::MAX_PROMPT_BYTES
    );
    println!(
        "  Tokens are estimated at {} bytes per token, not a tokenizer count.",
        targets::PROMPT_BYTES_PER_TOKEN
    );
    println!(
        "  A scripted session (fix_one_file) is replayed against the release binary: its first"
    );
    println!(
        "  request, total request bytes, request count and prefix breaks are checked against ceilings."
    );
}

/// The per-commit loop: format, lint, test.
fn check() -> Result<(), String> {
    format_and_lint()?;
    cargo(&["test", "--workspace"])
}

/// Checks formatting and lint before more expensive work.
fn format_and_lint() -> Result<(), String> {
    cargo(&["fmt", "--all", "--", "--check"])?;
    cargo(&[
        "clippy",
        "--workspace",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ])
}

/// Runs cargo with the given arguments, inheriting stdio.
fn cargo(args: &[&str]) -> Result<(), String> {
    run("cargo", args)
}

/// Runs cargo with extra environment variables, inheriting stdio.
fn cargo_with_env(args: &[&str], env: &[(&str, &str)]) -> Result<(), String> {
    let status = Command::new("cargo")
        .args(args)
        .envs(env.iter().copied())
        .status()
        .map_err(|err| format!("could not run `cargo`: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`cargo {}` failed", args.join(" ")))
    }
}

/// The target the landing page's harness is built for.
const WEB_TARGET: &str = "wasm32-wasip1";

/// Builds the harness for the browser and places it where the page loads it.
///
/// The page is served as static files with no build step of its own, so the
/// module is committed beside it, and this is the one command that makes it.
fn web() -> Result<(), String> {
    let built = build_web()?;
    let placed = web_module_path()?;
    std::fs::copy(&built, &placed).map_err(|err| {
        format!(
            "could not copy {} to {}: {err}",
            built.display(),
            placed.display()
        )
    })?;
    let module = std::fs::read(&placed)
        .map_err(|err| format!("could not read {}: {err}", placed.display()))?;
    let record = WebRecord {
        source: web_source_digest()?,
        module: digest(&module),
    };
    std::fs::write(web_record_path()?, record.render())
        .map_err(|err| format!("could not write the module record: {err}"))?;
    println!(
        "site/demo/rune.wasm: {} bytes, source {}",
        module.len(),
        record.source
    );
    Ok(())
}

/// What a committed module was built from, and what it is.
///
/// The compiler does not produce the same module bytes on every host, so the
/// module cannot be checked by rebuilding it and comparing bytes on another
/// machine. The record ties it to the source it was built from instead: the
/// source digest is the same on every host, and the module digest shows the
/// committed module is the one that was recorded.
#[derive(Debug, PartialEq, Eq)]
struct WebRecord {
    source: String,
    module: String,
}

impl WebRecord {
    fn render(&self) -> String {
        format!("source {}\nmodule {}\n", self.source, self.module)
    }

    fn parse(text: &str) -> Option<Self> {
        let mut source = None;
        let mut module = None;
        for line in text.lines() {
            match line.split_once(' ') {
                Some(("source", value)) => source = Some(value.trim().to_owned()),
                Some(("module", value)) => module = Some(value.trim().to_owned()),
                _ => {}
            }
        }
        Some(Self {
            source: source?,
            module: module?,
        })
    }
}

/// Path of the record that sits beside the committed module.
fn web_record_path() -> Result<PathBuf, String> {
    Ok(web_module_path()?.with_extension("wasm.source"))
}

/// Digests every input the browser module is built from.
///
/// The inputs are the tracked files of each path package the module depends
/// on, the lockfile, the workspace manifest and the toolchain pin, each named
/// by its path relative to the root, so the digest is the same on every host.
fn web_source_digest() -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let root = workspace_root()?;
    let mut paths = vec![
        "Cargo.lock".to_owned(),
        "Cargo.toml".to_owned(),
        "rust-toolchain.toml".to_owned(),
    ];
    let packages = web_package_dirs(&root)?;
    let listed = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["ls-files", "-z", "--"])
        .args(&packages)
        .output()
        .map_err(|err| format!("could not run git: {err}"))?;
    if !listed.status.success() {
        return Err("`git ls-files` failed while digesting the module source".to_owned());
    }
    paths.extend(
        String::from_utf8_lossy(&listed.stdout)
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_owned),
    );
    paths.sort();
    paths.dedup();
    let mut hasher = Sha256::new();
    for path in &paths {
        let bytes = std::fs::read(root.join(path))
            .map_err(|err| format!("could not read {path}: {err}"))?;
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update(bytes.len().to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Returns the directories of the path packages the browser module builds,
/// relative to the workspace root.
fn web_package_dirs(root: &Path) -> Result<Vec<String>, String> {
    let output = Command::new("cargo")
        .args([
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--filter-platform",
            WEB_TARGET,
        ])
        .current_dir(root)
        .output()
        .map_err(|err| format!("could not run `cargo metadata`: {err}"))?;
    if !output.status.success() {
        return Err("`cargo metadata` failed while digesting the module source".to_owned());
    }
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("`cargo metadata` printed invalid JSON: {err}"))?;
    let packages = metadata["packages"].as_array().cloned().unwrap_or_default();
    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let web = packages
        .iter()
        .find(|package| package["name"] == "rune-web")
        .and_then(|package| package["id"].as_str())
        .ok_or("`cargo metadata` does not list rune-web")?
        .to_owned();
    let mut reached = std::collections::BTreeSet::new();
    let mut pending = vec![web];
    while let Some(id) = pending.pop() {
        if !reached.insert(id.clone()) {
            continue;
        }
        let Some(node) = nodes.iter().find(|node| node["id"] == id.as_str()) else {
            continue;
        };
        for dependency in node["deps"].as_array().into_iter().flatten() {
            // Development dependencies do not reach the module.
            let normal_or_build = dependency["dep_kinds"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|kind| kind["kind"] != "dev");
            if normal_or_build && let Some(next) = dependency["pkg"].as_str() {
                pending.push(next.to_owned());
            }
        }
    }
    let mut dirs: Vec<String> = packages
        .iter()
        .filter(|package| package["source"].is_null())
        .filter(|package| {
            package["id"]
                .as_str()
                .is_some_and(|id| reached.contains(id))
        })
        .filter_map(|package| package["manifest_path"].as_str())
        .filter_map(|manifest| Path::new(manifest).parent())
        .filter_map(|dir| dir.strip_prefix(root).ok())
        .map(|dir| dir.to_string_lossy().replace('\\', "/"))
        .collect();
    dirs.sort();
    Ok(dirs)
}

/// Fails unless the committed landing-page module is what the source builds.
///
/// An optional argument names the copy to check, so a caller can point at a
/// copy outside the working tree.
fn verify_web(extra: &[String]) -> Result<(), String> {
    let placed = match extra {
        [] => web_module_path()?,
        [path] => PathBuf::from(path),
        _ => return Err("usage: cargo xtask verify-web [committed-module-path]".to_owned()),
    };
    // The module is still built, so the source compiles for the browser and a
    // caller can exercise the fresh build.
    let built = build_web()?;
    let committed = std::fs::read(&placed)
        .map_err(|err| format!("could not read {}: {err}", placed.display()))?;
    let record_path = placed.with_extension("wasm.source");
    let record = std::fs::read_to_string(&record_path)
        .ok()
        .as_deref()
        .and_then(WebRecord::parse)
        .ok_or_else(|| {
            format!(
                "{} is missing or unreadable; run `cargo xtask web` and commit the result",
                record_path.display()
            )
        })?;
    let source = web_source_digest()?;
    let regenerate = "run `cargo xtask web` and commit the result";
    if record.source != source {
        return Err(format!(
            "{} is stale: it was built from source {}, the current source is {source}; {regenerate}",
            placed.display(),
            record.source
        ));
    }
    if record.module != digest(&committed) {
        return Err(format!(
            "{} is not the module its record names ({}, found {}); {regenerate}",
            placed.display(),
            record.module,
            digest(&committed)
        ));
    }
    println!(
        "{} was built from the current source ({source}); this host built {} bytes at {}",
        placed.display(),
        std::fs::metadata(&built).map_or(0, |meta| meta.len()),
        built.display()
    );
    Ok(())
}

/// Builds the browser harness and returns the module the build produced.
///
/// Machine-specific source paths are remapped out of the module. Rust records
/// the source path of every panic site, so the build would otherwise embed the
/// builder's workspace root, `CARGO_HOME`, and toolchain directory, and two
/// machines could never produce equal bytes.
fn build_web() -> Result<PathBuf, String> {
    let root = workspace_root()?;
    let rust_src = rust_src_remap();
    cargo_with_rustflags(
        &[
            "build",
            "--release",
            "--locked",
            "-p",
            "rune-web",
            "--target",
            WEB_TARGET,
        ],
        &web_rustflags(
            &root,
            cargo_home().as_deref(),
            rust_src
                .as_ref()
                .map(|(from, to)| (from.as_path(), to.as_str())),
        ),
    )?;
    let target =
        std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);
    Ok(target
        .join(WEB_TARGET)
        .join("release")
        .join("rune_web.wasm"))
}

/// Returns the path the committed landing-page module is served from.
fn web_module_path() -> Result<PathBuf, String> {
    Ok(workspace_root()?
        .join("site")
        .join("demo")
        .join("rune.wasm"))
}

/// Returns the workspace root, the parent of the xtask crate.
fn workspace_root() -> Result<PathBuf, String> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "the xtask crate has no parent directory".to_owned())
}

/// Returns the directory Cargo keeps registry sources in, when it is known.
///
/// Cargo defaults to `$HOME/.cargo` when `CARGO_HOME` is unset, and CI sets the
/// variable explicitly.
fn cargo_home() -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".cargo")))
}

/// Returns the rebuild flags that keep machine-specific paths out of the module.
///
/// Workspace sources are recorded relative to the root, but dependency sources
/// are absolute and live under `CARGO_HOME`, which differs per machine. Standard
/// library sources are absolute too: when the `rust-src` component is installed
/// rustc records them under the toolchain, and when it is not the precompiled
/// library records them as `/rustc/<commit>/library`. The two spellings differ
/// per machine, so the local tree is mapped onto the virtual prefix the
/// precompiled library already uses.
fn web_rustflags(
    root: &Path,
    cargo_home: Option<&Path>,
    rust_src: Option<(&Path, &str)>,
) -> String {
    let mut flags = vec![format!("--remap-path-prefix={}=/rune", root.display())];
    if let Some(cargo_home) = cargo_home {
        flags.push(format!(
            "--remap-path-prefix={}=/cargo",
            cargo_home.display()
        ));
    }
    if let Some((from, to)) = rust_src {
        flags.push(format!("--remap-path-prefix={}={to}", from.display()));
    }
    flags.join(" ")
}

/// Locates the toolchain's standard-library sources and the virtual prefix the
/// precompiled library records them under.
///
/// Returns `None` when `rustc` cannot be queried, in which case the remap is
/// skipped rather than failing the build. The prefix is a no-op on a machine
/// without `rust-src`, because nothing was recorded under it to rewrite.
fn rust_src_remap() -> Option<(PathBuf, String)> {
    let sysroot = rustc_stdout(&["--print", "sysroot"])?;
    let version = rustc_stdout(&["-vV"])?;
    let hash = version
        .lines()
        .find_map(|line| line.strip_prefix("commit-hash: "))?;
    Some((
        PathBuf::from(sysroot.trim_end())
            .join("lib")
            .join("rustlib")
            .join("src")
            .join("rust"),
        format!("/rustc/{}", hash.trim_end()),
    ))
}

/// Runs `rustc` and returns its standard output when it succeeds.
///
/// The compiler Cargo will use is preferred, so a build pinned to a specific
/// `RUSTC` remaps that compiler's sources rather than whatever `rustc` resolves
/// to on `PATH`.
fn rustc_stdout(args: &[&str]) -> Option<String> {
    let program = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Runs cargo with `RUSTFLAGS` set, inheriting stdio.
///
/// An inherited `CARGO_ENCODED_RUSTFLAGS` would take precedence and drop the
/// remaps, so it is removed.
fn cargo_with_rustflags(args: &[&str], rustflags: &str) -> Result<(), String> {
    let status = Command::new("cargo")
        .args(args)
        .env("RUSTFLAGS", rustflags)
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .status()
        .map_err(|err| format!("could not run `cargo`: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`cargo {}` failed", args.join(" ")))
    }
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

/// Steps in the long-turn measurement.
const LONG_TURN_STEPS: usize = 1000;

/// Resources the long-turn run used, from the output of `time`.
#[derive(Debug, PartialEq)]
struct TimeUsage {
    peak_rss_bytes: u64,
    cpu_seconds: f64,
}

/// Reads peak memory and user plus system CPU from `/usr/bin/time -l` (macOS)
/// or `/usr/bin/time -v` (Linux) output.
fn parse_time_output(text: &str) -> Option<TimeUsage> {
    let mut rss = None;
    let mut user = None;
    let mut system = None;
    let mut mac_cpu = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_suffix("maximum resident set size") {
            rss = rest.trim().parse::<u64>().ok();
        } else if let Some(rest) = line.strip_prefix("Maximum resident set size (kbytes):") {
            rss = rest
                .trim()
                .parse::<u64>()
                .ok()
                .map(|kib| kib.saturating_mul(1024));
        } else if let Some(rest) = line.strip_prefix("User time (seconds):") {
            user = rest.trim().parse::<f64>().ok();
        } else if let Some(rest) = line.strip_prefix("System time (seconds):") {
            system = rest.trim().parse::<f64>().ok();
        } else if line.contains(" real ") && line.contains(" user ") && line.ends_with(" sys") {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let number = |label: &str| {
                let at = fields.iter().position(|field| *field == label)?;
                fields.get(at.checked_sub(1)?)?.parse::<f64>().ok()
            };
            mac_cpu = Some(number("user")? + number("sys")?);
        }
    }
    let cpu_seconds = match (user, system, mac_cpu) {
        (Some(user), Some(system), _) => user + system,
        (_, _, Some(cpu)) => cpu,
        _ => return None,
    };
    Some(TimeUsage {
        peak_rss_bytes: rss?,
        cpu_seconds,
    })
}

/// Compares measurements with the stored baselines.
fn perf_failures(rss: u64, cpu: f64, frame: f64) -> Vec<String> {
    let limit = 1.0 + perf_baseline::ALLOWED_REGRESSION;
    let percent = (perf_baseline::ALLOWED_REGRESSION * 100.0).round();
    let mut failures = Vec::new();
    if rss as f64 > perf_baseline::LONG_TURN_PEAK_RSS_BYTES as f64 * limit {
        failures.push(format!(
            "long-turn peak memory {:.1} MiB is more than {percent}% above its baseline {:.1} MiB",
            rss as f64 / 1_048_576.0,
            perf_baseline::LONG_TURN_PEAK_RSS_BYTES as f64 / 1_048_576.0
        ));
    }
    if cpu > perf_baseline::LONG_TURN_CPU_SECONDS * limit {
        failures.push(format!(
            "long-turn CPU time {cpu:.2} s is more than {percent}% above its baseline {:.2} s",
            perf_baseline::LONG_TURN_CPU_SECONDS
        ));
    }
    if frame > perf_baseline::FIRST_FRAME_MEDIAN_MS * limit {
        failures.push(format!(
            "first frame {frame:.1} ms is more than {percent}% above its baseline {:.1} ms",
            perf_baseline::FIRST_FRAME_MEDIAN_MS
        ));
    }
    failures
}

/// The scripted long run: one `read_file` call per request, then an answer.
fn long_turn_scenario(steps: usize) -> rune_testkit::scenario::Scenario {
    let lines: Vec<String> = (1..=steps.saturating_add(100))
        .map(|line| format!("line {line}"))
        .collect();
    let workspace = std::iter::once(("data.txt".to_owned(), lines.join("\n") + "\n")).collect();
    let mut turns: Vec<rune_testkit::scripted::Turn> = (1..=steps)
        .map(|step| {
            rune_testkit::scripted::Turn::call(
                &format!("call_{step}"),
                "read_file",
                &format!("{{\"path\":\"data.txt\",\"start_line\":{step},\"line_count\":1}}"),
            )
        })
        .collect();
    turns.push(rune_testkit::scripted::Turn::text("Done."));
    rune_testkit::scenario::Scenario {
        name: "long_turn".to_owned(),
        prompt: "Read the file one line at a time.".to_owned(),
        workspace,
        // No step cap: `ask` has none by default, and a cap equal to the
        // request count would make the last request the graceful final step.
        global_args: Vec::new(),
        ask_args: Vec::new(),
        env: std::collections::BTreeMap::new(),
        provider: None,
        // A window no run here can fill, so the measurement is the turn loop
        // and never a compaction (1,000 steps come close to 80% of 128k).
        user_config: Some(
            "[models.chat_completions]\nid = \"scripted\"\ncontext_window = 100000000\n".to_owned(),
        ),
        timeout_s: 600,
        turns,
        expect: rune_testkit::scenario::Expect {
            exit_code: Some(0),
            requests: Some(steps.saturating_add(1)),
            ..Default::default()
        },
    }
}

/// Runs the long turn under `time` and returns its peak memory and CPU.
fn measure_long_turn(binary: &Path, steps: usize) -> Result<TimeUsage, String> {
    let time = Path::new("/usr/bin/time");
    if !time.exists() {
        return Err("perf needs /usr/bin/time".to_owned());
    }
    let flag = if cfg!(target_os = "macos") {
        "-l"
    } else {
        "-v"
    };
    let wrapper = vec!["/usr/bin/time".to_owned(), flag.to_owned()];
    let scenario = long_turn_scenario(steps);
    let provider =
        rune_testkit::scripted::ScriptedProvider::start_without_bodies(scenario.turns.clone());
    let report = rune_testkit::scenario::run_wrapped(binary, &wrapper, &scenario, provider);
    let failures = rune_testkit::scenario::check(&scenario, &report);
    if !failures.is_empty() {
        let tail: Vec<&str> = report.stderr.lines().rev().take(25).collect();
        return Err(format!(
            "long-turn scenario did not finish: {}\n{}",
            failures.join("; "),
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        ));
    }
    parse_time_output(&report.stderr).ok_or_else(|| {
        format!(
            "could not read resource use from `time` output:\n{}",
            report.stderr
        )
    })
}

/// Median of the samples the first-frame script prints.
fn measure_first_frame(binary: &Path) -> Result<f64, String> {
    let output = Command::new("python3")
        .arg("-c")
        .arg(include_str!("first_frame.py"))
        .arg(binary)
        .output()
        .map_err(|err| format!("could not run python3: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "first frame measurement failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(text.trim())
        .map_err(|err| format!("first frame output is not JSON: {err}"))?;
    value
        .get("median_ms")
        .and_then(serde_json::Value::as_f64)
        .ok_or_else(|| "first frame output has no median_ms".to_owned())
}

/// Measures long-turn memory and CPU and the time to the first frame.
///
/// `--steps N` (not in the help) runs only the long turn at another length
/// and prints its numbers, for checking how cost grows with the turn.
fn perf(extra: &[String]) -> Result<(), String> {
    let steps = match extra {
        [] => None,
        [flag, value] if flag == "--steps" => Some(
            value
                .parse::<usize>()
                .ok()
                .filter(|steps| *steps > 0)
                .ok_or_else(|| format!("--steps needs a positive whole number, not `{value}`"))?,
        ),
        _ => return Err(format!("perf: unexpected arguments {extra:?}")),
    };
    cargo(&["build", "--release", "-p", "rune"])?;
    let binary = release_binary_path();
    if let Some(steps) = steps {
        let usage = measure_long_turn(&binary, steps)?;
        println!(
            "long turn    {steps} steps: peak resident {:.1} MiB, cpu {:.2} s",
            usage.peak_rss_bytes as f64 / 1_048_576.0,
            usage.cpu_seconds
        );
        return Ok(());
    }
    let usage = measure_long_turn(&binary, LONG_TURN_STEPS)?;
    let frame = measure_first_frame(&binary)?;
    let mib = |bytes: u64| bytes as f64 / 1_048_576.0;
    println!(
        "long turn    {LONG_TURN_STEPS} steps: peak resident {:.1} MiB (baseline {:.1} MiB), cpu {:.2} s (baseline {:.2} s)",
        mib(usage.peak_rss_bytes),
        mib(perf_baseline::LONG_TURN_PEAK_RSS_BYTES),
        usage.cpu_seconds,
        perf_baseline::LONG_TURN_CPU_SECONDS
    );
    println!(
        "first frame  median of 11 {frame:.1} ms (baseline {:.1} ms)",
        perf_baseline::FIRST_FRAME_MEDIAN_MS
    );
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        println!("no baseline for this host; numbers printed only");
        return Ok(());
    }
    let failures = perf_failures(usage.peak_rss_bytes, usage.cpu_seconds, frame);
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

/// Builds the release profile and checks size and startup.
fn budget() -> Result<(), String> {
    cargo(&["build", "--release", "-p", "rune"])?;

    let binary = release_binary_path();
    let size = std::fs::metadata(&binary)
        .map_err(|err| format!("could not read {}: {err}", binary.display()))?
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
    check_prompt(&binary)?;
    check_tools(&binary)?;
    check_scripted_session(&binary)?;
    check_duplicate_dependencies()?;
    Ok(())
}

/// Measures the assembled system prompt and enforces its ceiling.
///
/// The prompt is assembled from the working tree, so its size depends on the
/// repository contents and the effective limits. The release binary is run the
/// way a turn would run it. User configuration and user skills are excluded, so
/// the measurement reflects the checkout rather than the machine.
fn check_prompt(binary: &Path) -> Result<(), String> {
    let output = run_isolated(binary, &["prompt", "--show"])?;
    // `prompt --show` prints the prompt and then a newline when the text does
    // not end with one, so the trailing byte the CLI adds is not part of it.
    let bytes = output
        .stdout
        .strip_suffix(b"\n")
        .map_or(output.stdout.len(), <[u8]>::len);
    if !output.status.success() {
        return Err("`rune prompt --show` failed while measuring the prompt".to_owned());
    }
    let tokens = bytes / targets::PROMPT_BYTES_PER_TOKEN;
    println!(
        "prompt bytes  {bytes} bytes ({tokens} estimated tokens at {} bytes/token)",
        targets::PROMPT_BYTES_PER_TOKEN
    );
    match prompt_ceiling_failure(bytes) {
        Some(message) => Err(message),
        None => Ok(()),
    }
}

/// Measures the advertised tool schemas and enforces their ceiling.
///
/// Measured in the same isolated home as the prompt, so the count is what a
/// fresh install in this checkout advertises.
fn check_tools(binary: &Path) -> Result<(), String> {
    let output = run_isolated(binary, &["prompt", "--tools"])?;
    if !output.status.success() {
        return Err("`rune prompt --tools` failed while measuring the tool schemas".to_owned());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let count = text.lines().count();
    let bytes: usize = text.lines().map(str::len).sum();
    let tokens = bytes / targets::PROMPT_BYTES_PER_TOKEN;
    println!(
        "tool bytes    {bytes} bytes over {count} tools ({tokens} estimated tokens, budget {})",
        targets::MAX_TOOL_BYTES
    );
    if bytes > targets::MAX_TOOL_BYTES {
        return Err(format!(
            "advertised tool schemas exceed the {} byte ceiling",
            targets::MAX_TOOL_BYTES
        ));
    }
    Ok(())
}

/// Replays the `fix_one_file` scenario and enforces the request-shape ceilings.
fn check_scripted_session(binary: &Path) -> Result<(), String> {
    let path = workspace_root()?.join("crates/rune/tests/scenarios/fix_one_file.json");
    let scenario = rune_testkit::scenario::load(&path)?;
    let report = rune_testkit::scenario::run(binary, &scenario);
    let failures = rune_testkit::scenario::check(&scenario, &report);
    if !failures.is_empty() {
        return Err(format!(
            "scripted session `fix_one_file` did not pass: {}",
            failures.join("; ")
        ));
    }
    let lengths: Vec<usize> = report
        .requests
        .iter()
        .map(|request| normalized_len(&request.body, &[report.workspace_dir.as_str()]))
        .collect();
    let first = lengths.first().copied().unwrap_or(0);
    let total: usize = lengths.iter().sum();
    let count = lengths.len();
    let breaks = report.prefix_breaks;
    println!(
        "scripted session  fix_one_file: first request {first} bytes (ceiling {}), {total} bytes over {count} requests (ceiling {}, {} requests), {breaks} prefix breaks (ceiling {})",
        targets::MAX_SCRIPTED_FIRST_REQUEST_BYTES,
        targets::MAX_SCRIPTED_SESSION_BYTES,
        targets::MAX_SCRIPTED_REQUESTS,
        targets::MAX_SCRIPTED_PREFIX_BREAKS,
    );
    let failures = scripted_session_failures(first, total, count, breaks);
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

/// Returns the length of `body` with every path in `paths` replaced by `<workspace>`.
///
/// Paths are replaced as plain text: temporary directories on Unix contain no
/// character that JSON escapes.
fn normalized_len(body: &str, paths: &[&str]) -> usize {
    let mut text = body.to_owned();
    for path in paths.iter().filter(|path| !path.is_empty()) {
        text = text.replace(path, "<workspace>");
    }
    text.len()
}

/// Compares the measured session against its ceilings, one message per breach.
fn scripted_session_failures(
    first: usize,
    total: usize,
    count: usize,
    breaks: usize,
) -> Vec<String> {
    let mut failures = Vec::new();
    if first > targets::MAX_SCRIPTED_FIRST_REQUEST_BYTES {
        failures.push(format!(
            "first scripted request is {first} bytes, ceiling {}",
            targets::MAX_SCRIPTED_FIRST_REQUEST_BYTES
        ));
    }
    if total > targets::MAX_SCRIPTED_SESSION_BYTES {
        failures.push(format!(
            "scripted session sent {total} bytes, ceiling {}",
            targets::MAX_SCRIPTED_SESSION_BYTES
        ));
    }
    if count > targets::MAX_SCRIPTED_REQUESTS {
        failures.push(format!(
            "scripted session made {count} requests, ceiling {}",
            targets::MAX_SCRIPTED_REQUESTS
        ));
    }
    if breaks > targets::MAX_SCRIPTED_PREFIX_BREAKS {
        failures.push(format!(
            "scripted session had {breaks} prefix breaks, ceiling {}",
            targets::MAX_SCRIPTED_PREFIX_BREAKS
        ));
    }
    failures
}

/// Runs the binary with a throwaway home, so user configuration and user skills
/// do not reach the measurement.
fn run_isolated(binary: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    let isolated = std::env::temp_dir().join(format!("rune-budget-{}", std::process::id()));
    std::fs::create_dir_all(&isolated)
        .map_err(|err| format!("could not create {}: {err}", isolated.display()))?;
    Command::new(binary)
        .args(args)
        .env("HOME", &isolated)
        .env("RUNE_HOME", &isolated)
        .env("XDG_CONFIG_HOME", &isolated)
        .env("XDG_STATE_HOME", &isolated)
        .env("XDG_DATA_HOME", &isolated)
        .output()
        .map_err(|err| format!("could not run {}: {err}", binary.display()))
}

/// Returns the failure message for an assembled prompt over its ceiling.
fn prompt_ceiling_failure(bytes: usize) -> Option<String> {
    (bytes > targets::MAX_PROMPT_BYTES).then(|| {
        format!(
            "assembled prompt exceeds the {} byte ceiling",
            targets::MAX_PROMPT_BYTES
        )
    })
}

/// Format, lint, budget, and the full suite, for a pull request.
fn gate() -> Result<(), String> {
    format_and_lint()?;
    budget()?;
    cargo(&["test", "--workspace"])
}

/// Path of the built release binary.
fn release_binary_path() -> PathBuf {
    binary_path(env!("TARGET"))
}

/// Cargo resolves a relative target directory against the build's working directory.
fn target_directory() -> PathBuf {
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    root.join(std::env::var_os("CARGO_TARGET_DIR").unwrap_or_else(|| "target".into()))
}

/// Stages a release artifact for the host that is building it.
///
/// One target is staged at a time. Cross-compilation is supported, but startup
/// measurements require a runner that can execute the resulting artifact. The
/// artifact is named after the target, with a checksum and a measured manifest.
fn release(extra: &[String]) -> Result<(), String> {
    let (extra, signing_identity) = macos::release_args(extra)?;
    let channel = extra.first().map_or("dev", String::as_str);
    let version = match extra.get(1) {
        Some(value) => value.clone(),
        None => manifest_version()?,
    };
    // The target is taken from the caller when one is given, so a matrix can be
    // staged from one machine with the cross targets installed. Without it the
    // build target is used, which is the ordinary single-platform case.
    let target = extra
        .get(2)
        .cloned()
        .unwrap_or_else(|| env!("TARGET").to_owned());
    let directory = format!("target/dist/rune-{target}");
    if signing_identity.is_some() && !target.ends_with("-apple-darwin") {
        return Err("--sign-identity requires a macOS release target".to_owned());
    }

    // The binary reports the channel the archive manifest records.
    let channel_env = [("RUNE_CHANNEL", channel)];
    if target == env!("TARGET") {
        cargo_with_env(
            &["build", "--release", "--locked", "-p", "rune"],
            &channel_env,
        )?;
    } else {
        cargo_with_env(
            &[
                "build",
                "--release",
                "--locked",
                "-p",
                "rune",
                "--target",
                &target,
            ],
            &channel_env,
        )?;
    }

    let binary = binary_path(&target);
    let signature = signing_identity
        .as_deref()
        .map(|identity| macos::sign(&binary, identity))
        .transpose()?;
    let name = executable_name(&target);
    let extension = archive_extension(&target);
    let archive = format!("rune-{version}-{target}.{extension}");
    let archive_path = format!("{directory}/{archive}");

    // Measure the exact bytes that will be archived. A cross-built executable
    // must run on this machine; host measurements cannot stand in for it.
    let binary_bytes = std::fs::read(&binary)
        .map_err(|err| format!("could not read {}: {err}", binary.display()))?;
    let measurements = artifact_measurements(&binary, &binary_bytes)?;

    std::fs::create_dir_all(&directory)
        .map_err(|err| format!("could not create {directory}: {err}"))?;

    // The archive is what is published, so the checksum is over the archive
    // rather than over the binary inside it. A checksum naming a file that does
    // not exist verifies nothing.
    // The archive is written here rather than by an external tool, because the
    // tools disagree about how to pin metadata: one accepts a timestamp format
    // the other rejects, and an unpinned timestamp makes two runs over identical
    // input produce different archives. Writing it means the bytes depend on the
    // binary alone.
    let packed = if extension == "zip" {
        windows::archive(&binary_bytes)?
    } else {
        archive_bytes(&name, &binary_bytes)?
    };
    if let Some(signature) = &signature {
        // Check an extracted copy of the archive, not just the build output.
        macos::verify_archive(&packed, signature, &measurements["binary"])?;
    }
    std::fs::write(&archive_path, &packed)
        .map_err(|err| format!("could not write {archive_path}: {err}"))?;

    let bytes = std::fs::read(&archive_path)
        .map_err(|err| format!("could not read {archive_path}: {err}"))?;
    let checksum = digest(&bytes);

    // Read back and re-check, so a staging step that wrote something else cannot
    // publish a digest that does not describe the file.
    let reread = std::fs::read(&archive_path)
        .map_err(|err| format!("could not re-read {archive_path}: {err}"))?;
    if digest(&reread) != checksum || (signature.is_some() && reread != packed) {
        return Err(format!("{archive_path} changed while it was being staged"));
    }

    write(
        &format!("{directory}/{archive}.sha256"),
        &format!("{checksum}  {archive}\n"),
    )?;
    let mut manifest = serde_json::json!({
        "name": "rune",
        "version": version,
        "channel": channel,
        "target": target,
        "archive": archive,
        "bytes": bytes.len(),
        "sha256": checksum,
        "binary": measurements["binary"],
        "startup": measurements["startup"],
    });
    if let Some(signature) = signature {
        manifest["signature"] = signature;
    }
    let mut manifest_json = serde_json::to_string_pretty(&manifest)
        .map_err(|err| format!("could not serialize the release manifest: {err}"))?;
    manifest_json.push('\n');
    write(&format!("{directory}/manifest.json"), &manifest_json)?;

    println!("staged   {archive_path}");
    println!("bytes    {}", bytes.len());
    println!("sha256   {checksum}");
    println!("manifest {directory}/manifest.json");
    Ok(())
}

/// Repeats release measurements on an extracted executable without rebuilding it.
fn benchmark(extra: &[String]) -> Result<(), String> {
    let [path] = extra else {
        return Err("usage: cargo xtask benchmark <executable-path>".to_owned());
    };
    let binary = Path::new(path);
    let bytes = std::fs::read(binary)
        .map_err(|err| format!("could not read {}: {err}", binary.display()))?;
    let measurements = artifact_measurements(binary, &bytes)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&measurements)
            .map_err(|err| format!("could not serialize measurements: {err}"))?
    );
    Ok(())
}

/// Records raw measurements and enough method detail to repeat them.
fn artifact_measurements(binary: &Path, bytes: &[u8]) -> Result<serde_json::Value, String> {
    // Resolve relative paths, including a bare filename, rather than searching PATH.
    let executable = std::fs::canonicalize(binary)
        .map_err(|err| format!("could not resolve {}: {err}", binary.display()))?;
    let version = startup_metrics(&executable, "--version")?;
    let help = startup_metrics(&executable, "--help")?;
    let reread = std::fs::read(&executable)
        .map_err(|err| format!("could not re-read {}: {err}", executable.display()))?;
    if reread != bytes {
        return Err(format!("{} changed during benchmarking", binary.display()));
    }
    Ok(serde_json::json!({
        "binary": {
            "name": binary.file_name().ok_or("executable path has no filename")?.to_string_lossy(),
            "bytes": bytes.len(),
            "sha256": digest(bytes),
        },
        "startup": {
            "host": {
                "target": env!("TARGET"),
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
            },
            "method": {
                "reproduce": ["cargo", "xtask", "benchmark", "<extracted executable path>"],
                "sample_count": MEASUREMENT_SAMPLES,
                "commands": [["--version"], ["--help"]],
                "clock": "std::time::Instant",
                "timing": "wall time from before process spawn through successful exit",
                "execution": "serial, --version samples followed by --help samples",
                "warmup_runs": 0,
                "cache": "uncontrolled; no cache flushing or cold-start guarantee",
                "stdin": "inherited",
                "stdout": "discarded",
                "stderr": "discarded",
                "environment": "inherited from benchmark process",
                "working_directory": "inherited from benchmark process",
                "baseline_subtracted": false,
                "percentile": "nearest rank: p95 is the 30th sorted sample of 31",
            },
            "measurements": {
                "--version": version.raw_json(),
                "--help": help.raw_json(),
            },
        },
    }))
}

/// Builds a tar archive holding one file.
///
/// Every header field is fixed, so the same input always produces the same
/// bytes. The modification time is zero, the owner and group are zero, and the
/// mode is fixed rather than read from the filesystem, because a build machine's
/// umask is not part of a release.
#[must_use]
pub fn tar_single(name: &str, contents: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(contents.len().saturating_add(2048));
    out.extend_from_slice(&tar_header(name, contents.len()));
    out.extend_from_slice(contents);
    let padding = 512_usize.saturating_sub(contents.len() % 512) % 512;
    out.resize(out.len().saturating_add(padding), 0);
    // Two zero blocks end an archive.
    out.resize(out.len().saturating_add(1024), 0);
    out
}

/// Builds the bytes that are published for one binary.
///
/// Packing and compressing are one step here rather than two at the call site,
/// because the name the release uses says the result is gzipped and a caller
/// that forgot the compression would still produce a file that opens on a
/// machine which sniffs the format.
fn archive_bytes(name: &str, binary: &[u8]) -> Result<Vec<u8>, String> {
    gzip(&tar_single(name, binary))
}

fn archive_extension(target: &str) -> &'static str {
    if target.contains("-windows-") {
        "zip"
    } else {
        "tar.gz"
    }
}

/// Compresses an archive, as the `.tar.gz` name promises.
///
/// The name is part of the contract: a downloader picks its decompressor from
/// it, and a file that is not gzip refuses to open in anything but a tar program
/// that sniffs the format. The gzip header carries a modification time, so it is
/// pinned the way the tar fields are, or the same input would produce different
/// bytes on a different machine.
fn gzip(contents: &[u8]) -> Result<Vec<u8>, String> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(9));
    encoder
        .write_all(contents)
        .map_err(|err| format!("could not compress the archive: {err}"))?;
    encoder
        .finish()
        .map_err(|err| format!("could not finish the archive: {err}"))
}

/// Builds one tar header block.
fn tar_header(name: &str, size: usize) -> [u8; 512] {
    fn put(header: &mut [u8; 512], offset: usize, bytes: &[u8]) {
        let end = offset.saturating_add(bytes.len()).min(512);
        let len = end.saturating_sub(offset);
        header[offset..end].copy_from_slice(&bytes[..len]);
    }

    let mut header = [0_u8; 512];
    put(&mut header, 0, name.as_bytes());
    put(&mut header, 100, b"0000755\0");
    put(&mut header, 108, b"0000000\0");
    put(&mut header, 116, b"0000000\0");
    put(&mut header, 124, format!("{size:011o}\0").as_bytes());
    put(&mut header, 136, b"00000000000\0");
    // Spaces, because the checksum is computed with this field blank.
    put(&mut header, 148, b"        ");
    put(&mut header, 156, b"0");
    put(&mut header, 257, b"ustar");
    put(&mut header, 263, b"00");

    let sum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
    put(&mut header, 148, format!("{sum:06o}\0 ").as_bytes());
    header
}

/// Returns the path of the built binary for a target.
///
/// A cross build puts its output under the target name; a host build does not.
fn binary_path(target: &str) -> PathBuf {
    let mut path = target_directory();
    if target != env!("TARGET") {
        path.push(target);
    }
    path.push("release");
    path.push(executable_name(target));
    path
}

/// Returns the executable name a platform produces.
///
/// Derived from the target rather than from the host that happens to be
/// building, which is what keeps a matrix consistent.
#[must_use]
pub fn executable_name(target: &str) -> String {
    if target.contains("windows") {
        "rune.exe".to_owned()
    } else {
        "rune".to_owned()
    }
}

/// Reads the workspace version from the manifest.
///
/// The version is read rather than restated, so a release cannot be published
/// under a version the crate does not declare.
fn manifest_version() -> Result<String, String> {
    let text = std::fs::read_to_string("Cargo.toml")
        .map_err(|err| format!("could not read Cargo.toml: {err}"))?;
    let mut in_workspace = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_workspace = trimmed.starts_with("[workspace.package]");
            continue;
        }
        if in_workspace
            && let Some(value) = trimmed.strip_prefix("version")
            && let Some(value) = value.trim().strip_prefix('=')
        {
            return Ok(value.trim().trim_matches('"').to_owned());
        }
    }
    Err(String::from("the workspace manifest declares no version"))
}

/// Returns the lowercase hexadecimal SHA-256 digest of some bytes.
fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

/// Writes a file, reporting the path on failure.
fn write(path: &str, contents: &str) -> Result<(), String> {
    std::fs::write(path, contents).map_err(|err| format!("could not write {path}: {err}"))
}

/// Measures startup for the help and version paths.
///
/// Reports the raw distribution of spawn-to-exit wall times. The budget uses
/// the minimum of 31 runs minus the baseline minimum, clamped to zero. This
/// adjusted work estimate is distinct from the observed raw process latency.
fn check_startup(binary: &Path) -> Result<(), String> {
    let (program, label) = baseline_program()?;
    let floor = measure_floor(program)?;
    println!("process baseline minimum-of-31 {floor:.2} ms ({label})");

    let cases = [
        ("--version", targets::MAX_VERSION_MS),
        ("--help", targets::MAX_HELP_MS),
    ];

    let mut failures = Vec::new();

    for (flag, budget_ms) in cases {
        let raw = startup_metrics(binary, flag)?;
        println!("{}", raw.report(flag, floor, budget_ms));

        if let Some(failure) = raw.budget_failure(flag, floor, budget_ms) {
            failures.push(failure);
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

/// Chooses a real minimal process, never a shell or the application under test.
#[cfg(windows)]
fn baseline_program() -> Result<(&'static Path, &'static str), String> {
    Ok((
        Path::new(concat!(env!("OUT_DIR"), "/rune-process-baseline.exe")),
        "empty Rust executable",
    ))
}

#[cfg(not(windows))]
fn baseline_program() -> Result<(&'static Path, &'static str), String> {
    let candidates = ["/usr/bin/true", "/bin/true"];
    let Some(program) = candidates.into_iter().find(|path| Path::new(path).exists()) else {
        return Err(
            "no baseline is available; startup work ceilings cannot be enforced".to_owned(),
        );
    };
    Ok((Path::new(program), "true"))
}

/// Includes spawn, execution and waiting for exit of the baseline process.
/// Subtracting its minimum gives an estimate of additional work, not latency.
fn measure_floor(program: &Path) -> Result<f64, String> {
    let mut samples = [0.0; MEASUREMENT_SAMPLES];
    for sample in &mut samples {
        let start = std::time::Instant::now();
        let status = Command::new(program)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|err| {
                format!(
                    "could not run process baseline {}: {err}; startup work ceilings cannot be enforced",
                    program.display()
                )
            })?;
        if !status.success() {
            return Err(format!(
                "process baseline {} exited with a failure; startup work ceilings cannot be enforced",
                program.display()
            ));
        }
        *sample = start.elapsed().as_secs_f64() * 1000.0;
    }
    Ok(StartupMetrics::from_samples(samples).minimum)
}

/// Raw spawn-to-exit wall times in milliseconds, without baseline subtraction.
struct StartupMetrics {
    samples: [f64; MEASUREMENT_SAMPLES],
    minimum: f64,
    median: f64,
    p95: f64,
}

impl StartupMetrics {
    fn from_samples(samples: [f64; MEASUREMENT_SAMPLES]) -> Self {
        // Nearest rank: ceil(0.95 * 31) = 30, with a zero-based index of 29.
        const P95_INDEX: usize = (95 * MEASUREMENT_SAMPLES).div_ceil(100) - 1;
        let mut sorted = samples;
        sorted.sort_by(f64::total_cmp);
        Self {
            samples,
            minimum: sorted[0],
            median: sorted[MEASUREMENT_SAMPLES / 2],
            p95: sorted[P95_INDEX],
        }
    }

    fn raw_json(&self) -> serde_json::Value {
        serde_json::json!({
            "samples_ms": self.samples.as_slice(),
            "minimum_ms": self.minimum,
            "median_ms": self.median,
            "p95_ms": self.p95,
        })
    }

    fn work_ms(&self, floor: f64) -> f64 {
        (self.minimum - floor).max(0.0)
    }

    fn budget_failure(&self, flag: &str, floor: f64, budget_ms: f64) -> Option<String> {
        let work = self.work_ms(floor);
        (work > budget_ms).then(|| {
            format!(
                "{flag} took {work:.2} ms of baseline-subtracted work, budget is {budget_ms:.0} ms"
            )
        })
    }

    fn report(&self, flag: &str, floor: f64, budget_ms: f64) -> String {
        format!(
            "startup {flag:<10} raw process latency: minimum-of-{MEASUREMENT_SAMPLES} {:.2} ms, median {:.2} ms, p95 {:.2} ms; baseline-subtracted work {:.2} ms (budget {budget_ms:.0} ms)",
            self.minimum,
            self.median,
            self.p95,
            self.work_ms(floor)
        )
    }
}

/// Measures all 31 raw wall times for one invocation path.
fn startup_metrics(binary: &Path, flag: &str) -> Result<StartupMetrics, String> {
    let mut samples = [0.0; MEASUREMENT_SAMPLES];
    for sample in &mut samples {
        let start = std::time::Instant::now();
        let status = Command::new(binary)
            .arg(flag)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|err| format!("could not run {}: {err}", binary.display()))?;
        if !status.success() {
            return Err(format!(
                "`{} {flag}` exited with a failure",
                binary.display()
            ));
        }
        *sample = start.elapsed().as_secs_f64() * 1000.0;
    }

    Ok(StartupMetrics::from_samples(samples))
}

/// Crates whose duplicate versions are accepted, with the reason.
///
/// Each entry is a crate that two upstream dependencies pin independently.
/// Listing them explicitly keeps the check meaningful: anything not listed is a
/// regression worth fixing.
const DUPLICATE_EXCEPTIONS: &[(&str, &str)] = &[
    (
        "winnow",
        "toml 0.9 parses with winnow 0.7 and writes with toml_parser, which pins winnow 1",
    ),
    (
        "getrandom",
        "xtask replays scripted sessions through rune-testkit, whose tempfile uses getrandom 0.4; the shipped binary uses 0.2",
    ),
];

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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[test]
    fn the_perf_gate_allows_25_percent_and_fails_above() {
        let rss = perf_baseline::LONG_TURN_PEAK_RSS_BYTES;
        let cpu = perf_baseline::LONG_TURN_CPU_SECONDS;
        let frame = perf_baseline::FIRST_FRAME_MEDIAN_MS;
        assert!(perf_failures(rss, cpu, frame).is_empty());
        assert!(perf_failures(rss * 125 / 100, cpu * 1.25, frame * 1.25).is_empty());
        let over = perf_failures(rss * 126 / 100, cpu * 1.26, frame * 1.26);
        assert_eq!(over.len(), 3, "{over:?}");
        assert!(over[0].starts_with("long-turn peak memory"));
        assert!(over[1].starts_with("long-turn CPU time"));
        assert!(over[2].starts_with("first frame"));
        assert_eq!(perf_failures(rss * 126 / 100, cpu, frame).len(), 1);
        assert_eq!(perf_failures(rss, cpu * 1.26, frame).len(), 1);
        assert_eq!(perf_failures(rss, cpu, frame * 1.26).len(), 1);
    }

    #[test]
    fn time_output_is_parsed_on_both_hosts() {
        let macos = "        2.50 real         1.20 user         0.30 sys\n\
                     \x20          9830400  maximum resident set size\n\
                     \x20                 0  average shared memory size\n";
        assert_eq!(
            parse_time_output(macos),
            Some(TimeUsage {
                peak_rss_bytes: 9_830_400,
                cpu_seconds: 1.5
            })
        );
        let linux = "\tCommand being timed: \"rune\"\n\
                     \tUser time (seconds): 1.20\n\
                     \tSystem time (seconds): 0.30\n\
                     \tMaximum resident set size (kbytes): 9600\n";
        assert_eq!(
            parse_time_output(linux),
            Some(TimeUsage {
                peak_rss_bytes: 9_830_400,
                cpu_seconds: 1.5
            })
        );
        assert_eq!(parse_time_output("nothing useful"), None);
    }

    #[test]
    fn a_module_record_round_trips_and_rejects_a_partial_file() {
        let record = WebRecord {
            source: "abc".to_owned(),
            module: "def".to_owned(),
        };
        assert_eq!(WebRecord::parse(&record.render()), Some(record));
        assert_eq!(WebRecord::parse("source abc\n"), None);
    }

    #[test]
    fn startup_distribution_reports_raw_minimum_median_and_nearest_rank_p95() {
        let samples = std::array::from_fn(|index| (MEASUREMENT_SAMPLES - index) as f64);
        let metrics = StartupMetrics::from_samples(samples);
        assert!((metrics.minimum - 1.0).abs() < f64::EPSILON);
        assert!((metrics.median - 16.0).abs() < f64::EPSILON);
        assert!((metrics.p95 - 30.0).abs() < f64::EPSILON);
        // Tails above the budget must not change the existing minimum-based gate.
        assert!(metrics.median > targets::MAX_VERSION_MS);
        assert!(metrics.p95 > targets::MAX_VERSION_MS);
        assert!(metrics.work_ms(0.5) <= targets::MAX_VERSION_MS);
        assert_eq!(
            metrics.report("--version", 0.5, targets::MAX_VERSION_MS),
            "startup --version  raw process latency: minimum-of-31 1.00 ms, median 16.00 ms, p95 30.00 ms; baseline-subtracted work 0.50 ms (budget 2 ms)"
        );
    }

    #[test]
    fn startup_work_is_clamped_without_subtracting_from_raw_metrics() {
        let metrics = StartupMetrics::from_samples([1.0; MEASUREMENT_SAMPLES]);
        assert!(metrics.work_ms(2.0).abs() < f64::EPSILON);
        assert_eq!(
            metrics.report("--help", 2.0, targets::MAX_HELP_MS),
            "startup --help     raw process latency: minimum-of-31 1.00 ms, median 1.00 ms, p95 1.00 ms; baseline-subtracted work 0.00 ms (budget 2 ms)"
        );
    }

    #[test]
    fn startup_budgets_exclude_process_cost_but_reject_excess_application_work() {
        let floor = 6.0;
        for (flag, budget_ms) in [
            ("--version", targets::MAX_VERSION_MS),
            ("--help", targets::MAX_HELP_MS),
        ] {
            let at_ceiling = StartupMetrics::from_samples([floor + budget_ms; MEASUREMENT_SAMPLES]);
            assert!(at_ceiling.minimum > budget_ms);
            assert!(at_ceiling.budget_failure(flag, floor, budget_ms).is_none());

            let over_ceiling =
                StartupMetrics::from_samples([floor + budget_ms + 1.0; MEASUREMENT_SAMPLES]);
            let failure = over_ceiling
                .budget_failure(flag, floor, budget_ms)
                .expect("work above the ceiling must fail even with a baseline");
            assert!(failure.contains("baseline-subtracted work"), "{failure}");
        }
    }

    #[test]
    fn missing_baseline_refuses_to_enforce_work_ceilings() {
        let missing = Path::new(env!("CARGO_MANIFEST_DIR")).join("missing-process-baseline");
        let error = measure_floor(&missing).expect_err("missing baseline must fail the budget");
        assert!(error.contains("could not run process baseline"), "{error}");
        assert!(
            error.contains("startup work ceilings cannot be enforced"),
            "{error}"
        );
    }

    #[test]
    fn baseline_process_is_available_and_measurable() {
        let (program, label) = baseline_program().expect("platform baseline");
        assert!(program.is_file(), "{}", program.display());
        #[cfg(windows)]
        assert_eq!(label, "empty Rust executable");
        #[cfg(not(windows))]
        assert_eq!(label, "true");
        let floor = measure_floor(program).expect("measure real baseline process");
        assert!(floor.is_finite() && floor > 0.0, "{floor}");
    }

    #[test]
    fn windows_raw_latency_is_reported_separately_from_work() {
        let metrics = StartupMetrics::from_samples([7.31; MEASUREMENT_SAMPLES]);
        assert!(
            metrics
                .budget_failure("--version", 6.0, targets::MAX_VERSION_MS)
                .is_none()
        );
        assert_eq!(
            metrics.report("--version", 6.0, targets::MAX_VERSION_MS),
            "startup --version  raw process latency: minimum-of-31 7.31 ms, median 7.31 ms, p95 7.31 ms; baseline-subtracted work 1.31 ms (budget 2 ms)"
        );
    }

    #[test]
    fn the_prompt_ceiling_fails_only_above_it() {
        // The gate must pass at the ceiling and fail one byte past it, so a
        // prompt that grows past the floor is caught rather than reported.
        assert!(prompt_ceiling_failure(targets::MAX_PROMPT_BYTES).is_none());
        let message = prompt_ceiling_failure(targets::MAX_PROMPT_BYTES + 1)
            .expect("one byte over the ceiling was accepted");
        assert!(message.contains("assembled prompt exceeds"), "{message}");
    }

    #[test]
    fn the_scripted_session_ceilings_fail_only_above_them() {
        let first = targets::MAX_SCRIPTED_FIRST_REQUEST_BYTES;
        let total = targets::MAX_SCRIPTED_SESSION_BYTES;
        let count = targets::MAX_SCRIPTED_REQUESTS;
        let breaks = targets::MAX_SCRIPTED_PREFIX_BREAKS;
        assert!(scripted_session_failures(first, total, count, breaks).is_empty());
        let cases = [
            (
                scripted_session_failures(first.saturating_add(1), total, count, breaks),
                "first scripted request is",
                first < usize::MAX,
            ),
            (
                scripted_session_failures(first, total.saturating_add(1), count, breaks),
                "scripted session sent",
                total < usize::MAX,
            ),
            (
                scripted_session_failures(first, total, count.saturating_add(1), breaks),
                "scripted session made",
                count < usize::MAX,
            ),
            (
                scripted_session_failures(first, total, count, breaks + 1),
                "scripted session had",
                true,
            ),
        ];
        for (failures, text, reachable) in cases {
            if reachable {
                assert_eq!(failures.len(), 1, "{failures:?}");
                assert!(failures[0].contains(text), "{failures:?}");
            }
        }
    }

    #[test]
    fn workspace_paths_are_replaced_before_measuring() {
        let body = "cwd /tmp/ws/a and /tmp/ws/b";
        assert_eq!(
            normalized_len(body, &["/tmp/ws"]),
            "cwd <workspace>/a and <workspace>/b".len()
        );
    }

    /// Returns the tar header of an archive, for assertions about its fields.
    fn header_of(archive: &[u8]) -> &[u8] {
        &archive[..512]
    }

    #[test]
    fn the_archive_depends_on_nothing_but_its_input() {
        // A release hash is only useful if packing the same input gives one
        // answer. Every field that could carry the moment of packing is
        // asserted separately below, because two calls in the same second would
        // hide a timestamp and make this comparison pass.
        let first = tar_single("rune", b"binary contents");
        assert_eq!(first, tar_single("rune", b"binary contents"));
        assert_eq!(
            checksum_of(&first),
            "f9be084bc343354ce715b3230c912bc878baab25ce3d0d21d6f1a2455ab59692",
            "the archive bytes changed, so the published digest would change too"
        );
    }

    /// Decompresses a gzipped archive, for assertions about its contents.
    fn decompress(archive: &[u8]) -> Vec<u8> {
        use flate2::read::GzDecoder;
        use std::io::Read;

        let mut out = Vec::new();
        GzDecoder::new(archive)
            .read_to_end(&mut out)
            .expect("the archive is not readable as gzip");
        out
    }

    /// Returns the digest of some bytes.
    fn checksum_of(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(bytes))
    }

    #[test]
    fn the_archive_is_gzip_as_its_name_promises() {
        // A downloader chooses its decompressor from the file name, and a
        // decompressor reads the magic bytes rather than the name. An archive
        // that is really a plain tar opens with `tar` on some systems and fails
        // everywhere the name is taken at its word.
        let archive = archive_bytes("rune", b"x").expect("compression failed");
        assert_eq!(
            archive.get(..2),
            Some([0x1f, 0x8b].as_slice()),
            "the archive does not start with the gzip magic bytes"
        );
        assert!(!archive.is_empty());
    }

    #[test]
    fn compression_survives_a_round_trip() {
        let contents = b"a binary with some length to it".repeat(64);
        let archive = archive_bytes("rune", &contents).expect("compression failed");
        let decoded = decompress(&archive);
        assert_eq!(decoded, tar_single("rune", &contents));
        assert!(archive.len() < decoded.len(), "compression did not shrink");
    }

    #[test]
    fn the_compressed_archive_is_reproducible() {
        let first = archive_bytes("rune", b"binary contents").expect("compression failed");
        let second = archive_bytes("rune", b"binary contents").expect("compression failed");
        assert_eq!(
            first, second,
            "the same input compressed differently, so the digest would not describe it"
        );
    }

    #[test]
    fn different_contents_produce_different_archives() {
        assert_ne!(
            tar_single("rune", b"one"),
            tar_single("rune", b"two"),
            "different contents packed identically"
        );
    }

    #[test]
    fn the_archive_names_the_file_it_holds() {
        let archive = tar_single("rune", b"x");
        assert!(header_of(&archive).starts_with(b"rune\0"));
    }

    #[test]
    fn the_archive_records_a_usable_mode() {
        let archive = tar_single("rune", b"x");
        let mode = &header_of(&archive)[100..108];
        assert_eq!(mode, b"0000755\0", "the binary would not be executable");
    }

    #[test]
    fn the_archive_carries_no_timestamp_or_owner() {
        // A build machine's clock and user are not part of a release.
        let archive = tar_single("rune", b"x");
        let header = header_of(&archive);
        assert_eq!(
            &header[136..148],
            b"00000000000\0",
            "a timestamp was recorded"
        );
        assert_eq!(&header[108..116], b"0000000\0", "an owner was recorded");
        assert_eq!(&header[116..124], b"0000000\0", "a group was recorded");
    }

    #[test]
    fn the_recorded_size_matches_the_contents() {
        let archive = tar_single("rune", b"12345");
        let size = &header_of(&archive)[124..136];
        let text = std::str::from_utf8(size)
            .expect("ascii")
            .trim_end_matches(['\0']);
        assert_eq!(u64::from_str_radix(text, 8).expect("octal"), 5);
    }

    #[test]
    fn the_checksum_field_describes_the_header() {
        // A reader verifies the archive by recomputing this, so a wrong value
        // would make every extraction fail on a strict reader.
        let archive = tar_single("rune", b"x");
        let mut header = *header_of(&archive).first_chunk::<512>().expect("a header");
        let recorded = std::str::from_utf8(&header[148..154])
            .expect("ascii")
            .to_owned();
        header[148..156].fill(b' ');
        let sum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
        assert_eq!(
            u64::from_str_radix(&recorded, 8).expect("octal"),
            u64::from(sum)
        );
    }

    #[test]
    fn the_archive_is_padded_and_terminated() {
        // A tar archive ends with two zero blocks, and every entry is padded to
        // a block boundary.
        let contents = b"12345";
        let archive = tar_single("rune", contents);
        assert_eq!(archive.len() % 512, 0, "the archive is not block aligned");
        assert_eq!(archive.len(), 512 + 512 + 1024);
        assert!(
            archive[1024..].iter().all(|byte| *byte == 0),
            "the terminator is not zeroed"
        );
    }

    #[test]
    fn a_name_too_long_for_the_field_is_truncated_rather_than_overflowing() {
        let archive = tar_single(&"n".repeat(200), b"x");
        assert_eq!(archive.len() % 512, 0);
    }

    #[test]
    fn the_executable_name_follows_the_target() {
        assert_eq!(executable_name("x86_64-pc-windows-msvc"), "rune.exe");
        assert_eq!(executable_name("aarch64-apple-darwin"), "rune");
        assert_eq!(executable_name("x86_64-unknown-linux-musl"), "rune");
    }

    #[test]
    fn the_web_rebuild_remaps_the_workspace_and_cargo_home() {
        // Both paths are machine-specific, so leaving either in the module
        // would stop the committed bytes from ever matching a rebuild.
        assert_eq!(
            web_rustflags(
                Path::new("/work/rune"),
                Some(Path::new("/home/user/.cargo")),
                Some((
                    Path::new("/home/user/.rustup/toolchains/1.98/lib/rustlib/src/rust"),
                    "/rustc/0123456789abcdef",
                )),
            ),
            "--remap-path-prefix=/work/rune=/rune \
             --remap-path-prefix=/home/user/.cargo=/cargo \
             --remap-path-prefix=/home/user/.rustup/toolchains/1.98/lib/rustlib/src/rust=/rustc/0123456789abcdef"
        );
    }

    #[test]
    fn the_web_rebuild_remaps_the_workspace_without_a_known_cargo_home() {
        assert_eq!(
            web_rustflags(Path::new("/work/rune"), None, None),
            "--remap-path-prefix=/work/rune=/rune"
        );
    }

    #[test]
    fn the_web_rebuild_remaps_the_standard_library_when_rust_src_is_absent() {
        // A machine without rust-src has nothing recorded under the toolchain
        // tree, so the remap is simply not added.
        assert_eq!(
            web_rustflags(Path::new("/work/rune"), Some(Path::new("/cargo")), None),
            "--remap-path-prefix=/work/rune=/rune --remap-path-prefix=/cargo=/cargo"
        );
    }

    #[test]
    fn the_rust_src_remap_targets_the_precompiled_virtual_prefix() {
        // The precompiled standard library records its sources as
        // `/rustc/<commit>/library`, so the local rust-src tree must be mapped
        // onto exactly that prefix or a host with rust-src builds differently.
        let (from, to) = rust_src_remap().expect("rustc should report its sysroot and commit");
        assert!(
            from.ends_with("lib/rustlib/src/rust"),
            "unexpected rust-src root: {}",
            from.display()
        );
        assert!(to.starts_with("/rustc/"), "unexpected virtual prefix: {to}");
    }
}
