//! The `create_directory` tool.

use rune_core::error::{ErrorCode, Result, RuneError};
use serde_json::json;

use crate::contract::{Activity, ExecutionContext, Tool, ToolOutput, validate_closed_arguments};
use crate::workspace::FileLimits;
use crate::{mutation, workspace};

/// Maximum input path length, bounding path resolution work.
pub const MAX_PATH_BYTES: usize = 16 * 1024;
/// Maximum number of missing directories created by one call.
pub const MAX_NEW_DIRECTORIES: usize = 128;

/// Creates a directory and its missing parents within the permitted roots.
#[derive(Clone, Copy, Debug, Default)]
pub struct CreateDirectory {
    limits: FileLimits,
}

impl CreateDirectory {
    /// Creates the tool with host-supplied output limits.
    #[must_use]
    pub const fn with_limits(limits: FileLimits) -> Self {
        Self { limits }
    }
}

impl Tool for CreateDirectory {
    fn name(&self) -> &'static str {
        "create_directory"
    }

    fn description(&self) -> &'static str {
        "Creates a directory and any missing parents within permitted roots. Existing \
         directories succeed without changes; existing files are refused. Paths through \
         links must remain within permitted roots. Creates at most 128 missing directories \
         per call, with paths limited to 16384 bytes. Returns a bounded summary."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory to create. Relative paths resolve against the workspace root."
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    fn activity(&self) -> Activity {
        Activity::Write
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
        let raw = mutation::required_string(self.name(), arguments, "path")?;
        if raw.len() > MAX_PATH_BYTES {
            return Err(RuneError::new(
                ErrorCode::LimitExceeded,
                format!("directory path exceeds {MAX_PATH_BYTES} bytes"),
            )
            .with_hint("use a shorter directory path"));
        }
        let path = workspace::resolve(context, raw)?.path;

        // Preflight the complete chain before creating anything, so conflicts
        // and excessive depth cannot leave a partially created tree.
        let mut missing = Vec::new();
        for ancestor in path.ancestors() {
            context.check_cancelled()?;
            match std::fs::metadata(ancestor) {
                Ok(metadata) if metadata.is_dir() => break,
                Ok(_) => {
                    return Err(RuneError::new(
                        ErrorCode::AlreadyExists,
                        format!("`{ancestor}` exists and is not a directory"),
                    )
                    .with_hint("choose a directory path without an existing file in it"));
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    if missing.len() == MAX_NEW_DIRECTORIES {
                        return Err(RuneError::new(
                            ErrorCode::LimitExceeded,
                            format!("directory creation exceeds {MAX_NEW_DIRECTORIES} missing directories"),
                        )
                        .with_hint("create a shallower directory tree per call"));
                    }
                    missing.push(ancestor);
                }
                Err(err) => {
                    return Err(RuneError::new(
                        ErrorCode::UnsafePath,
                        format!("cannot inspect directory `{ancestor}`: {err}"),
                    ));
                }
            }
        }

        let mut created = 0_usize;
        for directory in missing.into_iter().rev() {
            context.check_cancelled()?;
            // Recheck containment before each mutation, including new parents.
            workspace::resolve(context, directory.as_str())?;
            match std::fs::create_dir(directory) {
                Ok(()) => created = created.saturating_add(1),
                Err(err)
                    if err.kind() == std::io::ErrorKind::AlreadyExists
                        && std::fs::metadata(directory).is_ok_and(|metadata| metadata.is_dir()) =>
                {
                    // Another caller may have created the directory meanwhile.
                }
                Err(err) => {
                    return Err(RuneError::new(
                        ErrorCode::UnsafePath,
                        format!("cannot create directory `{directory}`: {err}"),
                    )
                    .with_hint(
                        "check directory permissions and retry; existing parents are kept",
                    ));
                }
            }
        }
        let summary = format!("Directory `{}`", workspace::display_path(context, &path));
        let footer = format!("\nCreated {created} directories; directory exists.");
        Ok(ToolOutput::success(workspace::join_capped(
            summary,
            &footer,
            self.limits.output_cap(context),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8Path;

    #[test]
    fn creation_declares_a_write_and_the_directory_permission_target() {
        let tool = CreateDirectory::default();
        assert_eq!(tool.activity(), Activity::Write);
        assert!(!tool.is_read_only());
        assert_eq!(
            tool.permission_target(&json!({"path": "one/two"}))
                .as_deref(),
            Some("one/two")
        );
        assert_eq!(tool.permission_target(&json!({})), None);
    }

    #[test]
    fn nested_creation_is_idempotent_and_preserves_existing_contents() {
        let dir = tempfile::tempdir().expect("workspace");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let context = ExecutionContext::new(root.to_owned());
        let arguments = json!({"path": "one/two/three"});
        let output = CreateDirectory::default()
            .call(&arguments, &context)
            .expect("create");
        assert!(!output.is_error);
        assert!(output.text.contains("Created 3 directories"));
        assert!(root.join("one/two/three").is_dir());
        std::fs::write(root.join("one/two/three/keep"), "keep").expect("fixture");
        for path in ["one/two/three", ".", root.as_str()] {
            let output = CreateDirectory::default()
                .call(&json!({"path": path}), &context)
                .expect("existing directory");
            assert!(output.text.contains("Created 0 directories"));
        }
        assert_eq!(
            std::fs::read_to_string(root.join("one/two/three/keep")).expect("read"),
            "keep"
        );
    }

    #[test]
    fn results_obey_both_output_caps_including_utf8_and_tiny_caps() {
        let dir = tempfile::tempdir().expect("workspace");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        for configured in [0, 1, 16, 128, 256] {
            let tool = CreateDirectory::with_limits(FileLimits {
                output_bytes: configured,
                ..FileLimits::default()
            });
            for per_call in [0, 1, 17, 129, 257] {
                let output = tool
                    .call(
                        &json!({"path": "é".repeat(100)}),
                        &ExecutionContext::new(root.to_owned()).with_output_cap(per_call),
                    )
                    .expect("create");
                assert!(!output.is_error);
                assert!(output.text.len() <= configured.min(per_call));
                assert_eq!(output.produced_bytes, output.text.len() as u64);
                if configured == 128 && per_call == 129 {
                    assert!(output.text.contains("truncated"));
                }
            }
        }
        assert!(root.join("é".repeat(100)).is_dir());
    }

    #[test]
    fn outside_paths_are_refused_but_explicit_roots_and_external_grants_are_honored() {
        let dir = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let extra = Utf8Path::from_path(outside.path()).expect("utf8");
        let context = ExecutionContext::new(root.to_owned());
        let target = extra.join("one/two");
        let relative = format!("../{}/one/two", extra.file_name().expect("name"));
        for path in [target.as_str(), relative.as_str()] {
            let err = CreateDirectory::default()
                .call(&json!({"path": path}), &context)
                .expect_err("refused");
            assert_eq!(err.code(), ErrorCode::PathOutsideWorkspace);
            assert!(!extra.join("one").exists());
        }
        CreateDirectory::default()
            .call(
                &json!({"path": target}),
                &context.clone().with_root(extra.to_owned()),
            )
            .expect("additional root");
        assert!(target.is_dir());
        let granted = extra.join("granted/nested");
        CreateDirectory::default()
            .call(
                &json!({"path": granted}),
                &context.with_external_access(true),
            )
            .expect("external grant");
        assert!(granted.is_dir());
    }

    #[test]
    fn files_invalid_arguments_and_cancellation_leave_the_workspace_unchanged() {
        let dir = tempfile::tempdir().expect("workspace");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let context = ExecutionContext::new(root.to_owned());
        std::fs::write(root.join("file"), "keep").expect("fixture");
        for path in ["file", "file/nested/leaf"] {
            assert!(
                CreateDirectory::default()
                    .call(&json!({"path": path}), &context)
                    .is_err()
            );
        }
        for arguments in [
            json!({}),
            json!({"path": 1}),
            json!({"path": ""}),
            json!({"path": "bad\0path"}),
            json!({"path": "new", "typo": true}),
        ] {
            assert!(
                CreateDirectory::default()
                    .call(&arguments, &context)
                    .is_err()
            );
        }
        context.cancellation().cancel();
        assert_eq!(
            CreateDirectory::default()
                .call(&json!({"path": "new/nested"}), &context)
                .expect_err("cancelled")
                .code(),
            ErrorCode::Cancelled
        );
        assert_eq!(
            std::fs::read_to_string(root.join("file")).expect("read"),
            "keep"
        );
        assert_eq!(std::fs::read_dir(root).expect("entries").count(), 1);
    }

    #[test]
    fn excessive_paths_and_creation_depth_are_refused_before_any_mutation() {
        let dir = tempfile::tempdir().expect("workspace");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let context = ExecutionContext::new(root.to_owned());
        for path in [
            "a".repeat(MAX_PATH_BYTES.saturating_add(1)),
            vec!["a"; MAX_NEW_DIRECTORIES.saturating_add(1)].join("/"),
        ] {
            let err = CreateDirectory::default()
                .call(&json!({"path": path}), &context)
                .expect_err("limit");
            assert_eq!(err.code(), ErrorCode::LimitExceeded);
            assert!(err.detail().hint.is_some());
            assert_eq!(std::fs::read_dir(root).expect("entries").count(), 0);
        }
        let path = vec!["a"; MAX_NEW_DIRECTORIES].join("/");
        let output = CreateDirectory::default()
            .call(&json!({"path": path}), &context)
            .expect("at limit");
        assert!(root.join(path).is_dir());
        assert!(output.text.contains("Created 128 directories"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_redirect_creation_outside_the_workspace() {
        let dir = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        std::os::unix::fs::symlink(outside.path(), root.join("escape")).expect("link");
        std::os::unix::fs::symlink(outside.path().join("absent"), root.join("dangling"))
            .expect("link");
        for (path, code) in [
            ("escape", ErrorCode::PathOutsideWorkspace),
            ("escape/one/two", ErrorCode::PathOutsideWorkspace),
            ("dangling/one", ErrorCode::UnsafePath),
        ] {
            let err = CreateDirectory::default()
                .call(
                    &json!({"path": path}),
                    &ExecutionContext::new(root.to_owned()),
                )
                .expect_err("refused");
            assert_eq!(err.code(), code);
        }
        assert_eq!(
            std::fs::read_dir(outside.path()).expect("entries").count(),
            0
        );
    }
}
