//! Repository automation.
//!
//! Tasks are grouped by cost so the default is the cheap one. `check` is the
//! per-commit loop; `budget` and `gate` are for a pull request.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

mod macos;
mod windows;

/// Serial runs per measurement, used for the minimum, median and p95.
const MEASUREMENT_SAMPLES: usize = 31;

/// Enforced release budget ceilings, distinct from measured build results.
mod targets {
    /// Largest accepted stripped release binary, in bytes.
    pub const MAX_BINARY_BYTES: u64 = 8 * 1024 * 1024;

    /// Baseline-subtracted minimum-of-31 work for `rune --version`, in milliseconds.
    pub const MAX_VERSION_MS: f64 = 5.0;

    /// Baseline-subtracted minimum-of-31 work for `rune --help`, in milliseconds.
    pub const MAX_HELP_MS: f64 = 8.0;

    /// Largest accepted assembled system prompt, in bytes.
    ///
    /// Shared with the crate that compiles the prompt, so the gate and the
    /// compiled-in floor cannot drift apart.
    pub const MAX_PROMPT_BYTES: usize = rune_core::budget::PROMPT_CEILING_BYTES;

    /// Divisor used to turn prompt bytes into an estimated token count.
    ///
    /// Four is the estimator's working bytes-per-token figure, not a tokenizer:
    /// the number printed beside the byte count is an estimate at this divisor.
    pub const PROMPT_BYTES_PER_TOKEN: usize = 4;
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
        "gate" => gate(),
        "release" => release(&extra),
        "verify-release" => macos::verify_release(&extra),
        "benchmark" => benchmark(&extra),
        "web" => web(),
        "verify-web" => verify_web(&extra),
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
    println!("  gate     format, lint, budget, and the full workspace test suite");
    println!("  release  stage a release artifact, its checksum, and a manifest");
    println!("  benchmark  print raw size and startup JSON for an existing executable");
    println!("  verify-release  check a signed macOS archive against a trusted identity");
    println!("  web      build the harness for the landing page into site/demo/rune.wasm");
    println!("  verify-web  fail unless site/demo/rune.wasm matches a fresh build");
    println!();
    println!(
        "  release takes: cargo xtask release <channel> [version] [target] [--sign-identity <certificate SHA-1>]"
    );
    println!(
        "  verify-release takes: cargo xtask verify-release <manifest-path> <certificate SHA-1>"
    );
    println!("  benchmark takes: cargo xtask benchmark <executable-path>");
    println!();
    print_budget_help();
}

/// Explains the measured reference build and the enforced size ceiling.
fn print_budget_help() {
    println!("usage: cargo xtask budget [--help]");
    println!();
    println!("  Builds the release profile and checks size and startup.");
    println!("  The audited stripped release build measured 5.46 MiB for");
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
    let bytes = std::fs::metadata(&placed)
        .map_err(|err| err.to_string())?
        .len();
    println!("site/demo/rune.wasm: {bytes} bytes");
    Ok(())
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
    let built = build_web()?;
    let expected = std::fs::read(&placed)
        .map_err(|err| format!("could not read {}: {err}", placed.display()))?;
    let actual = std::fs::read(&built)
        .map_err(|err| format!("could not read {}: {err}", built.display()))?;
    if expected != actual {
        return Err(format!(
            "{} is stale: the source builds {} bytes ({}), the committed module is {} bytes ({}); run `cargo xtask web` and commit the result",
            placed.display(),
            actual.len(),
            digest(&actual),
            expected.len(),
            digest(&expected),
        ));
    }
    println!(
        "{} matches the current source ({} bytes, {})",
        placed.display(),
        actual.len(),
        digest(&actual)
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
    let isolated = std::env::temp_dir().join(format!("rune-budget-{}", std::process::id()));
    std::fs::create_dir_all(&isolated)
        .map_err(|err| format!("could not create {}: {err}", isolated.display()))?;
    let output = Command::new(binary)
        .args(["prompt", "--show"])
        .env("HOME", &isolated)
        .env("RUNE_HOME", &isolated)
        .env("XDG_CONFIG_HOME", &isolated)
        .env("XDG_STATE_HOME", &isolated)
        .env("XDG_DATA_HOME", &isolated)
        .output()
        .map_err(|err| format!("could not run {}: {err}", binary.display()))?;
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

    if target == env!("TARGET") {
        cargo(&["build", "--release", "--locked", "-p", "rune"])?;
    } else {
        cargo(&[
            "build",
            "--release",
            "--locked",
            "-p",
            "rune",
            "--target",
            &target,
        ])?;
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;

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
            "startup --version  raw process latency: minimum-of-31 1.00 ms, median 16.00 ms, p95 30.00 ms; baseline-subtracted work 0.50 ms (budget 5 ms)"
        );
    }

    #[test]
    fn startup_work_is_clamped_without_subtracting_from_raw_metrics() {
        let metrics = StartupMetrics::from_samples([1.0; MEASUREMENT_SAMPLES]);
        assert!(metrics.work_ms(2.0).abs() < f64::EPSILON);
        assert_eq!(
            metrics.report("--help", 2.0, targets::MAX_HELP_MS),
            "startup --help     raw process latency: minimum-of-31 1.00 ms, median 1.00 ms, p95 1.00 ms; baseline-subtracted work 0.00 ms (budget 8 ms)"
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
            "startup --version  raw process latency: minimum-of-31 7.31 ms, median 7.31 ms, p95 7.31 ms; baseline-subtracted work 1.31 ms (budget 5 ms)"
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
