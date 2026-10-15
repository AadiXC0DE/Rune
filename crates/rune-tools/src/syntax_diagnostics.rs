//! Rust syntax inspection through a bounded, supervised local rustfmt process.

use std::time::{Duration, Instant};

use rune_core::error::{ErrorCode, Result, RuneError};
use rune_exec::command::{Exit, minimal_environment};
use rune_exec::session::{Input, POLL_INTERVAL, Process, Reading};

use crate::contract::{Activity, ExecutionContext, Tool, ToolOutput, validate_closed_arguments};
use crate::workspace::{
    FileLimits, display_path, resolve, string_arg, truncate_to_bytes, usize_arg,
};

const TIMEOUT: Duration = Duration::from_secs(30);
const TRUNCATION: &str = "\n[syntax diagnostics truncated at byte cap]\n";
const LABEL: &str = "Rust syntax diagnostics (rustfmt):\n";

/// Checks one Rust file without compiling it or changing its contents.
#[derive(Clone, Copy, Debug, Default)]
pub struct SyntaxDiagnostics {
    limits: FileLimits,
}

impl SyntaxDiagnostics {
    /// Builds the checker with host-supplied output limits.
    #[must_use]
    pub const fn with_limits(limits: FileLimits) -> Self {
        Self { limits }
    }
}

impl Tool for SyntaxDiagnostics {
    fn name(&self) -> &'static str {
        "syntax_diagnostics"
    }

    fn description(&self) -> &'static str {
        "Check the syntax of one .rs file with local rustfmt, returning compiler-style \
         diagnostics with file, line, column, and message. Requires rustfmt on PATH. \
         Defaults to Rust edition 2024; edition can be 2015, 2018, 2021, or 2024. \
         Checks syntax only, not types or dependencies. Does not compile, expand procedural \
         macros, follow child modules, or modify source files. Ignores rustfmt configuration. \
         The supervised checker has a 30-second deadline, cancellation, resource limits, \
         and bounded output. Syntax errors return a tool error; max_bytes lowers the output cap."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Rust fixture file to check."},
                "edition": {
                    "type": "string", "enum": ["2015", "2018", "2021", "2024"],
                    "description": "Rust parsing edition. Defaults to 2024."
                },
                "max_bytes": {
                    "type": "integer", "minimum": 1,
                    "description": "Total output bytes, capped by host limits. Must fit the label and truncation marker."
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    fn activity(&self) -> Activity {
        Activity::Read
    }

    fn permission_target(&self, arguments: &serde_json::Value) -> Option<String> {
        arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    }

    fn call(
        &self,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        context.check_cancelled()?;
        validate_closed_arguments(arguments, &self.input_schema())?;
        let raw = string_arg(arguments, "path")?.ok_or_else(|| RuneError::missing_field("path"))?;
        let edition = string_arg(arguments, "edition")?.unwrap_or("2024");
        if !matches!(edition, "2015" | "2018" | "2021" | "2024") {
            return Err(RuneError::invalid_field(
                "edition",
                "use 2015, 2018, 2021, or 2024",
            ));
        }
        let cap = usize_arg(arguments, "max_bytes")?
            .unwrap_or(usize::MAX)
            .min(self.limits.output_cap(context));
        let minimum = LABEL.len().saturating_add(TRUNCATION.len());
        if cap < minimum {
            return Err(RuneError::new(
                ErrorCode::LimitExceeded,
                format!("syntax_diagnostics needs at least {minimum} output bytes for its labels"),
            ));
        }
        let path = resolve(context, raw)?.path;
        if path.extension() != Some("rs") {
            return Err(RuneError::invalid_field("path", "expected a .rs file"));
        }
        let metadata = std::fs::metadata(&path).map_err(|err| {
            let code = match err.kind() {
                std::io::ErrorKind::NotFound => ErrorCode::NotFound,
                std::io::ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
                _ => ErrorCode::IoFailure,
            };
            RuneError::new(code, format!("cannot inspect `{path}`: {err}"))
        })?;
        if !metadata.is_file() {
            return Err(RuneError::invalid_field(
                "path",
                "expected a regular Rust file",
            ));
        }
        // Explicit argv prevents paths from becoming shell syntax. stdout emission
        // prevents writes even for unformatted files. The empty configuration and
        // skip_children keep inspection confined to the requested fixture.
        let config = EmptyConfig::create()?;
        let argv = [
            "rustfmt",
            "--emit",
            "stdout",
            "--color",
            "never",
            "--edition",
            edition,
            "--config-path",
            config.0.as_str(),
            "--config",
            "skip_children=true",
            path.as_str(),
        ]
        .map(str::to_owned);
        let (exit, stderr) = inspect(&argv, cap, context, TIMEOUT)?;
        Ok(render(exit, &stderr, &display_path(context, &path), cap))
    }
}

/// An explicit empty configuration prevents project and user configuration
/// discovery. A regular file also works on platforms whose null device cannot
/// be canonicalized by rustfmt. The temporary file is removed on all exits.
struct EmptyConfig(camino::Utf8PathBuf);

impl EmptyConfig {
    fn create() -> Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "rune-syntax-{}.toml",
            rune_core::id::SessionId::generate()
        ));
        let path = camino::Utf8PathBuf::from_path_buf(path)
            .map_err(|_| RuneError::invalid_field("temp_dir", "temporary path is not UTF-8"))?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        // create_new refuses collisions and symlinks rather than overwriting.
        drop(options.open(&path)?);
        Ok(Self(path))
    }
}

impl Drop for EmptyConfig {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn inspect(
    argv: &[String],
    cap: usize,
    context: &ExecutionContext,
    timeout: Duration,
) -> Result<(Exit, Reading)> {
    context.check_cancelled()?;
    let started = Instant::now();
    let mut environment = checker_environment(minimal_environment())?;
    // A rustup proxy must use an installed checker, even when a workspace
    // requests another toolchain. Inspection must not download or install one.
    environment.insert("RUSTUP_AUTO_INSTALL".to_owned(), "0".to_owned());
    let process = Process::start_argv(
        argv,
        Some(context.workspace().as_std_path()),
        cap,
        &environment,
        Input::Closed,
    )
    .map_err(|err| {
        RuneError::new(
            ErrorCode::TransportFailure,
            format!("Rust syntax checker could not start: {err}"),
        )
        .with_hint("install rustfmt and make it available on PATH")
    })?;
    let exit = loop {
        context.check_cancelled()?;
        if let Some(exit) = process.exit() {
            break exit;
        }
        if started.elapsed() >= timeout {
            return Err(RuneError::new(
                ErrorCode::LimitExceeded,
                "Rust syntax checker exceeded its deadline",
            ));
        }
        std::thread::sleep(POLL_INTERVAL);
    };
    process.drain_output(Duration::from_millis(250));
    context.check_cancelled()?;
    // Dropping a running Process ends its group on timeout and cancellation.
    Ok((exit, process.stderr().since(0)))
}

// Pin the proxy before entering the workspace. Neither rust-toolchain files nor
// directory overrides may select an executable for this read-only tool.
fn checker_environment(
    mut environment: std::collections::BTreeMap<String, String>,
) -> Result<std::collections::BTreeMap<String, String>> {
    if environment
        .get("RUSTUP_TOOLCHAIN")
        .is_none_or(String::is_empty)
    {
        let home = environment
            .get("RUSTUP_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                environment
                    .get(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                    .map(|home| std::path::PathBuf::from(home).join(".rustup"))
            });
        let mut toolchain = "stable".to_owned();
        if let Some(home) = home {
            match std::fs::File::open(home.join("settings.toml")) {
                Ok(file) => {
                    use std::io::Read as _;
                    let mut contents = String::new();
                    file.take(65537).read_to_string(&mut contents)?;
                    if contents.len() > 65536 {
                        return Err(RuneError::new(
                            ErrorCode::TooLarge,
                            "rustup settings exceed 64 KiB",
                        ));
                    }
                    let settings: toml::Table = toml::from_str(&contents).map_err(|_| {
                        RuneError::new(
                            ErrorCode::InvalidConfiguration,
                            "invalid user rustup settings",
                        )
                    })?;
                    if let Some(default) = settings
                        .get("default_toolchain")
                        .and_then(toml::Value::as_str)
                        .filter(|value| !value.is_empty())
                    {
                        default.clone_into(&mut toolchain);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        environment.insert("RUSTUP_TOOLCHAIN".to_owned(), toolchain);
    }
    Ok(environment)
}

fn render(exit: Exit, stderr: &Reading, path: &str, cap: usize) -> ToolOutput {
    let body = if exit.is_success() {
        format!("No syntax diagnostics for `{path}`.\n")
    } else if stderr.bytes.is_empty() {
        format!("Rust syntax checker {} for `{path}`.\n", exit.describe())
    } else {
        String::from_utf8_lossy(&stderr.bytes).into_owned()
    };
    let truncated =
        stderr.produced > stderr.bytes.len() as u64 || LABEL.len().saturating_add(body.len()) > cap;
    let room = cap
        .saturating_sub(LABEL.len())
        .saturating_sub(if truncated { TRUNCATION.len() } else { 0 });
    let mut text = String::from(LABEL);
    text.push_str(truncate_to_bytes(&body, room));
    if truncated {
        text.push_str(TRUNCATION);
    }
    ToolOutput {
        produced_bytes: stderr
            .produced
            .max(body.len() as u64)
            .saturating_add(LABEL.len() as u64)
            .max(text.len() as u64),
        text,
        is_error: !exit.is_success(),
        plan_update: None,
    }
}

#[cfg(test)]
mod tests {
    use camino::Utf8Path;
    use serde_json::json;

    use super::*;

    #[cfg(unix)]
    #[test]
    fn project_toolchain_cannot_select_the_checker() {
        use std::os::unix::fs::PermissionsExt as _;
        // Run the public tool in a subprocess so removing an inherited
        // RUSTUP_TOOLCHAIN cannot race other tests' process environment.
        if let Ok(root) = std::env::var("RUNE_SYNTAX_TEST_WORKSPACE") {
            let output = SyntaxDiagnostics::default()
                .call(
                    &json!({"path": "broken.rs"}),
                    &ExecutionContext::new(root.into()),
                )
                .expect("syntax check");
            assert!(
                output.is_error,
                "project checker bypassed syntax validation: {output:?}"
            );
            assert!(!output.text.contains("PROJECT_CHECKER"));
            return;
        }
        let directory = tempfile::tempdir().expect("workspace");
        let root = directory.path();
        let toolchain = root.join("evil");
        std::fs::create_dir_all(toolchain.join("bin")).expect("bin");
        std::fs::create_dir_all(toolchain.join("lib")).expect("lib");
        for name in ["rustc", "rustfmt"] {
            let path = toolchain.join("bin").join(name);
            std::fs::write(&path, "#!/bin/sh\nprintf PROJECT_CHECKER >&2\nexit 0\n")
                .expect("checker");
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
                .expect("executable");
        }
        std::fs::write(
            root.join("rust-toolchain.toml"),
            format!(
                "[toolchain]\npath = {}\n",
                toml::Value::String(toolchain.to_string_lossy().into_owned())
            ),
        )
        .expect("project override");
        let mut environment = minimal_environment();
        environment.remove("RUSTUP_TOOLCHAIN");
        environment.insert("RUSTUP_AUTO_INSTALL".to_owned(), "0".to_owned());
        let run = |environment: &std::collections::BTreeMap<String, String>| {
            std::process::Command::new("rustfmt")
                .arg("--version")
                .current_dir(root)
                .env_clear()
                .envs(environment)
                .output()
                .expect("rustfmt proxy")
        };
        let unpinned = run(&environment);
        assert!(
            String::from_utf8_lossy(&unpinned.stderr).contains("PROJECT_CHECKER"),
            "fixture must exercise project executable selection: {unpinned:?}"
        );
        std::fs::write(root.join("broken.rs"), "fn broken( {").expect("invalid Rust");
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "syntax_diagnostics::tests::project_toolchain_cannot_select_the_checker",
                "--nocapture",
            ])
            .env_remove("RUSTUP_TOOLCHAIN")
            .env("RUNE_SYNTAX_TEST_WORKSPACE", root)
            .output()
            .expect("isolated tool call");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let mut explicit = environment;
        explicit.insert("RUSTUP_TOOLCHAIN".to_owned(), "user-selected".to_owned());
        assert_eq!(
            checker_environment(explicit).expect("explicit")["RUSTUP_TOOLCHAIN"],
            "user-selected"
        );
    }

    #[test]
    fn empty_configuration_is_private_unique_and_removed_on_drop() {
        let config = EmptyConfig::create().expect("configuration");
        let other = EmptyConfig::create().expect("another configuration");
        let path = config.0.clone();
        assert_ne!(path, other.0);
        assert_eq!(std::fs::read(&path).expect("config bytes"), b"");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&path)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        drop(config);
        assert!(!path.exists());
    }

    #[test]
    fn advertised_checker_returns_fixture_file_line_and_message_without_mutation() {
        let directory = tempfile::tempdir().expect("directory");
        let root = Utf8Path::from_path(directory.path()).expect("root");
        // Spaces and shell metacharacters must remain a single literal argument.
        let file = "fixture ; echo injected.rs";
        let source = "fn main() {\n    let value = ;\n}\n";
        std::fs::write(root.join(file), source).expect("fixture");
        let registry = crate::inventory::builtin_default().expect("registry");
        let context = ExecutionContext::new(root.to_owned()).with_offline(true);
        assert_eq!(
            registry.activity("syntax_diagnostics"),
            Some(Activity::Read)
        );
        assert_eq!(
            registry.permission_target("syntax_diagnostics", &json!({"path": file})),
            Some(file.to_owned())
        );
        let output = registry
            .call("syntax_diagnostics", &json!({"path": file}), &context)
            .expect("check");
        assert!(output.is_error, "{}", output.text);
        assert!(
            output.text.contains(&format!("{file}:2:17")),
            "{}",
            output.text
        );
        assert!(
            output.text.contains("expected expression"),
            "{}",
            output.text
        );
        assert_eq!(
            std::fs::read_to_string(root.join(file)).expect("contents"),
            source
        );
        assert_eq!(std::fs::read_dir(root).expect("entries").count(), 1);
    }

    #[test]
    fn unformatted_valid_syntax_ignores_config_and_child_modules_without_writes() {
        let directory = tempfile::tempdir().expect("directory");
        let root = Utf8Path::from_path(directory.path()).expect("root");
        // A missing child would fail ordinary rustfmt module resolution. Types
        // and procedural macros are intentionally not checked or expanded.
        let source = "mod missing;\n#[unknown_macro]\nasync fn example(){let value:Missing=1;}\n";
        std::fs::write(root.join("fixture.rs"), source).expect("fixture");
        let config = "not valid rustfmt configuration";
        std::fs::write(root.join("rustfmt.toml"), config).expect("config");
        let context = ExecutionContext::new(root.to_owned());
        let output = SyntaxDiagnostics::default()
            .call(&json!({"path": "fixture.rs", "edition": "2021"}), &context)
            .expect("check");
        assert!(!output.is_error, "{}", output.text);
        assert!(
            output
                .text
                .contains("No syntax diagnostics for `fixture.rs`")
        );
        assert_eq!(
            std::fs::read_to_string(root.join("fixture.rs")).expect("contents"),
            source
        );
        assert_eq!(
            std::fs::read_to_string(root.join("rustfmt.toml")).expect("config"),
            config
        );
        assert_eq!(std::fs::read_dir(root).expect("entries").count(), 2);
        let old_edition = SyntaxDiagnostics::default()
            .call(&json!({"path": "fixture.rs", "edition": "2015"}), &context)
            .expect("check");
        assert!(old_edition.is_error);
        assert!(
            old_edition.text.contains("fixture.rs:3:"),
            "{}",
            old_edition.text
        );
    }

    #[test]
    fn invalid_arguments_paths_and_pre_cancelled_calls_are_refused() {
        let directory = tempfile::tempdir().expect("directory");
        let root = Utf8Path::from_path(directory.path()).expect("root");
        std::fs::write(root.join("fixture.txt"), "fn main() {}").expect("fixture");
        std::fs::create_dir(root.join("directory.rs")).expect("directory");
        let context = ExecutionContext::new(root.to_owned());
        let tool = SyntaxDiagnostics::default();
        for arguments in [
            json!({}),
            json!({"path": 1}),
            json!({"path": "fixture.txt"}),
            json!({"path": "directory.rs"}),
            json!({"path": "missing.rs"}),
            json!({"path": "fixture.rs", "edition": "2027"}),
            json!({"path": "fixture.rs", "max_bytes": 0}),
            json!({"path": "fixture.rs", "max_bytes": -1}),
            json!({"path": "fixture.rs", "unexpected": true}),
        ] {
            assert!(tool.call(&arguments, &context).is_err(), "{arguments}");
        }
        let outside = tempfile::tempdir().expect("outside");
        let external = Utf8Path::from_path(outside.path())
            .expect("outside")
            .join("fixture.rs");
        std::fs::write(&external, "fn main() {}").expect("external");
        assert_eq!(
            tool.call(&json!({"path": external}), &context)
                .expect_err("outside refused")
                .code(),
            ErrorCode::PathOutsideWorkspace
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&external, root.join("link.rs")).expect("link");
            assert_eq!(
                tool.call(&json!({"path": "link.rs"}), &context)
                    .expect_err("link refused")
                    .code(),
                ErrorCode::PathOutsideWorkspace
            );
        }
        context.cancellation().cancel();
        assert_eq!(
            tool.call(&json!({"path": "fixture.rs"}), &context)
                .expect_err("cancelled")
                .code(),
            ErrorCode::Cancelled
        );
    }

    #[test]
    fn real_diagnostics_obey_requested_host_and_context_output_caps() {
        let directory = tempfile::tempdir().expect("directory");
        let root = Utf8Path::from_path(directory.path()).expect("root");
        std::fs::write(root.join("fixture.rs"), "fn main() {\nlet café = ;\n}\n").expect("fixture");
        let context = ExecutionContext::new(root.to_owned());
        let tool = SyntaxDiagnostics::with_limits(FileLimits {
            output_bytes: 160,
            ..FileLimits::default()
        });
        for (arguments, context, cap) in [
            (
                json!({"path": "fixture.rs", "max_bytes": 128}),
                context.fork(),
                128,
            ),
            (
                json!({"path": "fixture.rs", "max_bytes": 4096}),
                context.fork(),
                160,
            ),
            (
                json!({"path": "fixture.rs"}),
                context.with_output_cap(96),
                96,
            ),
        ] {
            let output = tool.call(&arguments, &context).expect("bounded check");
            assert!(output.is_error);
            assert!(output.text.len() <= cap);
            assert!(output.text.ends_with(TRUNCATION), "{}", output.text);
            assert!(output.produced_bytes > output.text.len() as u64);
        }
    }

    #[test]
    fn missing_checker_is_reported_with_an_install_hint() {
        let directory = tempfile::tempdir().expect("directory");
        let root = Utf8Path::from_path(directory.path()).expect("root");
        let argv = [root.join("missing-rustfmt").into_string()];
        let error = inspect(&argv, 128, &ExecutionContext::new(root.to_owned()), TIMEOUT)
            .expect_err("missing checker");
        assert_eq!(error.code(), ErrorCode::TransportFailure);
        assert_eq!(
            error.hint(),
            Some("install rustfmt and make it available on PATH")
        );
    }

    #[cfg(unix)]
    #[test]
    fn timeout_and_cancellation_end_the_running_checker_and_its_child() {
        for cancel in [false, true] {
            let directory = tempfile::tempdir().expect("directory");
            let root = Utf8Path::from_path(directory.path()).expect("root");
            let argv = [
                "/bin/sh",
                "-c",
                "(sleep 1; printf survived > survived) & printf started > started; wait",
            ]
            .map(str::to_owned);
            let context = ExecutionContext::new(root.to_owned());
            let cancellation = context.cancellation();
            let marker = root.join("started");
            let canceller = cancel.then(|| {
                std::thread::spawn(move || {
                    let start = Instant::now();
                    while !marker.exists() && start.elapsed() < Duration::from_secs(5) {
                        std::thread::sleep(POLL_INTERVAL);
                    }
                    let started = marker.exists();
                    cancellation.cancel();
                    assert!(started, "checker started before cancellation");
                })
            });
            let started = Instant::now();
            let error = inspect(
                &argv,
                128,
                &context,
                if cancel {
                    TIMEOUT
                } else {
                    Duration::from_millis(200)
                },
            )
            .expect_err("supervisor stopped checker");
            if let Some(canceller) = canceller {
                canceller.join().expect("canceller");
            }
            assert_eq!(
                error.code(),
                if cancel {
                    ErrorCode::Cancelled
                } else {
                    ErrorCode::LimitExceeded
                }
            );
            assert!(started.elapsed() < Duration::from_secs(5));
            assert!(root.join("started").exists());
            std::thread::sleep(Duration::from_millis(1100));
            assert!(
                !root.join("survived").exists(),
                "child survived checker termination"
            );
        }
    }
}
