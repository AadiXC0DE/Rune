//! Repository automation.
//!
//! Tasks are grouped by cost so the default is the cheap one. `check` is the
//! per-commit loop; `budget` and `gate` are for a pull request.

#![forbid(unsafe_code)]

use std::process::{Command, ExitCode};

/// Budget targets. These are Rune's own numbers, measured on the release
/// profile, not a restatement of another project's figures.
/// Runs per measurement. The fastest is kept, so more samples only reduce noise.
const MEASUREMENT_SAMPLES: usize = 31;

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
        "release" => release(&extra),
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
    println!("  release  stage a release artifact, its checksum, and a manifest");
    println!();
    println!("  release takes: cargo xtask release <channel> [version]");
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

/// Stages a release artifact for the host that is building it.
///
/// One target is staged at a time: a machine can only build the platform it is
/// running on without a cross-compilation toolchain, so a matrix is produced by
/// running this once per machine. The artifact is named after the target, and a
/// checksum and a manifest are written beside it, because an artifact without a
/// published checksum cannot be verified by whoever downloads it.
fn release(extra: &[String]) -> Result<(), String> {
    let channel = extra.first().map_or("dev", String::as_str);
    let version = match extra.get(1) {
        Some(value) => value.clone(),
        None => manifest_version()?,
    };
    let target = env!("TARGET");
    let directory = format!("target/dist/rune-{target}");

    cargo(&["build", "--release", "--locked", "-p", "rune"])?;

    let binary = release_binary_path();
    let name = executable_name(target);
    let archive = format!("rune-{version}-{target}.tar.gz");
    let archive_path = format!("{directory}/{archive}");

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
    let binary_bytes =
        std::fs::read(&binary).map_err(|err| format!("could not read {binary}: {err}"))?;
    std::fs::write(&archive_path, tar_single(&name, &binary_bytes))
        .map_err(|err| format!("could not write {archive_path}: {err}"))?;

    let bytes = std::fs::read(&archive_path)
        .map_err(|err| format!("could not read {archive_path}: {err}"))?;
    let checksum = digest(&bytes);

    // Read back and re-check, so a staging step that wrote something else cannot
    // publish a digest that does not describe the file.
    let reread = std::fs::read(&archive_path)
        .map_err(|err| format!("could not re-read {archive_path}: {err}"))?;
    if digest(&reread) != checksum {
        return Err(format!("{archive_path} changed while it was being staged"));
    }

    write(
        &format!("{directory}/{archive}.sha256"),
        &format!("{checksum}  {archive}\n"),
    )?;
    write(
        &format!("{directory}/manifest.json"),
        &format!(
            "{{\n  \"name\": \"rune\",\n  \"version\": \"{version}\",\n  \
             \"channel\": \"{channel}\",\n  \"target\": \"{target}\",\n  \
             \"archive\": \"{archive}\",\n  \"bytes\": {},\n  \
             \"sha256\": \"{checksum}\"\n}}\n",
            bytes.len()
        ),
    )?;

    println!("staged   {archive_path}");
    println!("bytes    {}", bytes.len());
    println!("sha256   {checksum}");
    println!("manifest {directory}/manifest.json");
    Ok(())
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

    let mut samples = Vec::with_capacity(MEASUREMENT_SAMPLES);
    for _ in 0..MEASUREMENT_SAMPLES {
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
    let mut samples = Vec::with_capacity(MEASUREMENT_SAMPLES);
    for _ in 0..MEASUREMENT_SAMPLES {
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;

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

    /// Returns the digest of some bytes.
    fn checksum_of(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(bytes))
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
}
