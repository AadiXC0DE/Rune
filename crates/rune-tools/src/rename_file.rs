//! The `rename_file` tool.

use rune_core::error::{ErrorCode, Result, RuneError};
use serde_json::json;

use crate::contract::{Activity, ExecutionContext, Tool, ToolOutput, validate_closed_arguments};
use crate::{mutation, workspace};

/// Moves a regular file to a new, unused name.
#[derive(Clone, Copy, Debug, Default)]
pub struct RenameFile;

impl Tool for RenameFile {
    fn name(&self) -> &'static str {
        "rename_file"
    }

    fn description(&self) -> &'static str {
        "Renames a regular file from path to new_path. Both paths must be permitted. \
         Refuses existing destinations, symlinks, and directories. The destination's \
         parent directory must already exist. File contents and permissions are preserved."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Existing file. Relative paths resolve against the workspace root."
                },
                "new_path": {
                    "type": "string",
                    "description": "Unused destination path, relative to the workspace root or absolute."
                }
            },
            "required": ["path", "new_path"],
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
        let new_raw = mutation::required_string(self.name(), arguments, "new_path")?;
        let path = workspace::resolve(context, raw)?.path;
        let new_path = workspace::resolve(context, new_raw)?.path;
        let metadata = std::fs::symlink_metadata(&path).map_err(|err| {
            RuneError::new(
                if err.kind() == std::io::ErrorKind::NotFound {
                    ErrorCode::NotFound
                } else {
                    ErrorCode::UnsafePath
                },
                format!("cannot inspect rename source `{path}`: {err}"),
            )
        })?;
        if !metadata.file_type().is_file() {
            return Err(RuneError::new(
                ErrorCode::UnsafePath,
                format!("rename source `{path}` must be a regular file"),
            ));
        }
        match std::fs::symlink_metadata(&new_path) {
            Ok(_) => {
                return Err(RuneError::new(
                    ErrorCode::AlreadyExists,
                    format!("rename destination `{new_path}` already exists"),
                )
                .with_hint("choose an unused destination path"));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(RuneError::new(
                    ErrorCode::UnsafePath,
                    format!("cannot inspect rename destination `{new_path}`: {err}"),
                ));
            }
        }
        context.check_cancelled()?;
        std::fs::rename(&path, &new_path).map_err(|err| {
            RuneError::new(
                ErrorCode::UnsafePath,
                format!("cannot rename `{path}` to `{new_path}`: {err}"),
            )
            .with_hint(
                "ensure the destination parent exists and both paths are on the same filesystem",
            )
        })?;
        Ok(ToolOutput::success(format!(
            "Renamed `{path}` to `{new_path}`"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8Path;

    #[test]
    fn a_rename_preserves_binary_contents_and_permissions() {
        let dir = tempfile::tempdir().expect("workspace");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let source = root.join("before.bin");
        std::fs::write(&source, [0, 255, 13, 10]).expect("fixture");
        let permissions = std::fs::metadata(&source).expect("metadata").permissions();
        let output = RenameFile
            .call(
                &json!({"path": "before.bin", "new_path": "after.bin"}),
                &ExecutionContext::new(root.to_owned()),
            )
            .expect("rename");
        assert!(!output.is_error);
        assert!(!source.exists());
        assert_eq!(
            std::fs::read(root.join("after.bin")).expect("read"),
            [0, 255, 13, 10]
        );
        assert_eq!(
            std::fs::metadata(root.join("after.bin"))
                .expect("metadata")
                .permissions(),
            permissions
        );
    }

    #[test]
    fn either_path_outside_the_roots_is_refused() {
        let dir = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        std::fs::write(root.join("inside"), "inside").expect("fixture");
        let external = outside.path().join("outside");
        std::fs::write(&external, "outside").expect("fixture");
        for arguments in [
            json!({"path": "inside", "new_path": outside.path().join("new")}),
            json!({"path": external, "new_path": "new"}),
        ] {
            let err = RenameFile
                .call(&arguments, &ExecutionContext::new(root.to_owned()))
                .expect_err("refused");
            assert_eq!(err.code(), ErrorCode::PathOutsideWorkspace);
        }
        assert_eq!(
            std::fs::read_to_string(root.join("inside")).expect("read"),
            "inside"
        );
        assert_eq!(std::fs::read_to_string(&external).expect("read"), "outside");
        assert!(!root.join("new").exists());
        assert!(!outside.path().join("new").exists());
    }

    #[test]
    fn existing_destinations_directories_missing_parents_and_missing_sources_are_refused() {
        let dir = tempfile::tempdir().expect("workspace");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        std::fs::write(root.join("source"), "source").expect("fixture");
        std::fs::write(root.join("existing"), "keep").expect("fixture");
        std::fs::create_dir(root.join("directory")).expect("directory");
        for (path, new_path, code) in [
            ("source", "existing", ErrorCode::AlreadyExists),
            ("source", "source", ErrorCode::AlreadyExists),
            ("directory", "new", ErrorCode::UnsafePath),
            ("source", "missing/new", ErrorCode::UnsafePath),
            ("missing", "new", ErrorCode::NotFound),
        ] {
            let err = RenameFile
                .call(
                    &json!({"path": path, "new_path": new_path}),
                    &ExecutionContext::new(root.to_owned()),
                )
                .expect_err("refused");
            assert_eq!(err.code(), code);
        }
        assert_eq!(
            std::fs::read_to_string(root.join("source")).expect("read"),
            "source"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("existing")).expect("read"),
            "keep"
        );
        assert!(!root.join("new").exists());
    }

    #[test]
    fn cancelled_and_malformed_calls_leave_the_source_unchanged() {
        let dir = tempfile::tempdir().expect("workspace");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        std::fs::write(root.join("source"), "keep").expect("fixture");
        for arguments in [
            json!({"path": "source"}),
            json!({"path": "source", "new_path": 1}),
            json!({"path": "source", "new_path": ""}),
        ] {
            assert!(
                RenameFile
                    .call(&arguments, &ExecutionContext::new(root.to_owned()))
                    .is_err()
            );
        }
        let context = ExecutionContext::new(root.to_owned());
        context.cancellation().cancel();
        assert_eq!(
            RenameFile
                .call(&json!({"path": "source", "new_path": "new"}), &context)
                .expect_err("cancelled")
                .code(),
            ErrorCode::Cancelled
        );
        assert_eq!(
            std::fs::read_to_string(root.join("source")).expect("read"),
            "keep"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_redirect_a_rename_outside_the_workspace() {
        let dir = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        std::fs::write(root.join("source"), "keep").expect("fixture");
        std::os::unix::fs::symlink(outside.path(), root.join("escape")).expect("link");
        std::os::unix::fs::symlink(root.join("source"), root.join("link")).expect("link");
        for (path, new_path, code) in [
            ("source", "escape/new", ErrorCode::PathOutsideWorkspace),
            ("link", "new", ErrorCode::UnsafePath),
            ("source", "link", ErrorCode::AlreadyExists),
        ] {
            assert_eq!(
                RenameFile
                    .call(
                        &json!({"path": path, "new_path": new_path}),
                        &ExecutionContext::new(root.to_owned())
                    )
                    .expect_err("refused")
                    .code(),
                code
            );
        }
        assert_eq!(
            std::fs::read_to_string(root.join("source")).expect("read"),
            "keep"
        );
        assert!(!outside.path().join("new").exists());
    }
}
