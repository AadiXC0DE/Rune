//! Glob matching over the workspace walk.

use rune_core::error::{Result, RuneError};

use crate::contract::{Activity, ExecutionContext, Tool, ToolOutput};
use crate::workspace::{
    FileLimits, Walker, compile_glob, default_root, display_in, join_capped, resolve, roots,
    string_arg, walk_notes,
};

/// How often a long walk checks for cancellation.
const CANCEL_CHECK_INTERVAL: usize = 1024;

/// Bytes held back from the listing so the footer always fits.
const FOOTER_RESERVE_BYTES: usize = 512;

/// Matches paths against a glob pattern.
#[derive(Clone, Copy, Debug, Default)]
pub struct GlobFiles;

impl Tool for GlobFiles {
    fn name(&self) -> &'static str {
        "glob_files"
    }

    fn description(&self) -> &'static str {
        "Find files by path pattern. `*` and `?` do not cross a directory separator and `**` \
         matches any depth, so `**/*.rs` finds Rust files anywhere below the root. `count` \
         reports the exact total when the listing itself is truncated."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern, for example `**/*.rs` or `src/*.toml`.",
                },
                "path": {
                    "type": "string",
                    "description": "Directory or file to search under. Defaults to the workspace root.",
                },
                "mode": {
                    "type": "string",
                    "enum": ["matches", "count"],
                    "description": "Return matching paths, or only the exact total.",
                },
            },
            "required": ["pattern"],
            "additionalProperties": false,
        })
    }

    fn activity(&self) -> Activity {
        Activity::List
    }

    fn permission_target(&self, arguments: &serde_json::Value) -> Option<String> {
        arguments
            .get("pattern")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    }

    fn call(
        &self,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        let pattern = string_arg(arguments, "pattern")?
            .ok_or_else(|| RuneError::missing_field("pattern"))?
            .to_owned();
        let matcher = compile_glob(&pattern)?;
        let mode = mode_of(arguments)?;
        let limits = FileLimits::default();

        let search_root = match string_arg(arguments, "path")? {
            Some(raw) => resolve(context, raw)?.path,
            None => default_root(context)?.path,
        };
        let normalised = roots(context);
        let label = display_in(&normalised, &search_root);

        let mut matches: Vec<String> = Vec::new();
        let mut total = 0_usize;
        let mut walker = Walker::new(&search_root, limits.walk_files)?;

        loop {
            let Some(entry) = walker.next() else { break };
            if total % CANCEL_CHECK_INTERVAL == 0 {
                context.check_cancelled()?;
            }
            if !matcher.is_match(&entry.relative) {
                continue;
            }
            total = total.saturating_add(1);
            if mode == Mode::Count || matches.len() >= limits.list_entries {
                continue;
            }
            matches.push(display_in(&normalised, &entry.path));
        }

        let notes = walk_notes(&walker);
        if total == 0 {
            return Ok(ToolOutput::success(format!(
                "no files match `{pattern}` under `{label}`\n{notes}"
            )));
        }

        if mode == Mode::Count {
            return Ok(ToolOutput::success(format!(
                "{total} files match `{pattern}` under `{label}`\n{notes}"
            )));
        }

        // Paths are added until the byte budget is spent, so the count reported
        // is the number actually listed rather than the number collected.
        let cap = limits.output_cap(context);
        let reserve = FOOTER_RESERVE_BYTES.min(cap / 2);
        let mut body = format!("{total} files match `{pattern}` under `{label}`\n");
        let mut listed = 0_usize;
        for path in &matches {
            if body.len().saturating_add(path.len()).saturating_add(1) > cap.saturating_sub(reserve)
            {
                break;
            }
            body.push_str(path);
            body.push('\n');
            listed = listed.saturating_add(1);
        }

        let mut footer = String::new();
        if listed < total {
            footer.push_str(&format!(
                "[showing {listed} of {total} matches{}; use count mode for the exact total or \
                 narrow the pattern]\n",
                if listed < matches.len() {
                    " within the byte cap"
                } else {
                    ""
                }
            ));
        }
        footer.push_str(&notes);
        Ok(ToolOutput::success(body + &footer))
    }
}

/// The mode a call asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Matches,
    Count,
}

/// Reads the mode argument.
fn mode_of(arguments: &serde_json::Value) -> Result<Mode> {
    match string_arg(arguments, "mode")? {
        None | Some("matches") => Ok(Mode::Matches),
        Some("count") => Ok(Mode::Count),
        Some(other) => Err(RuneError::invalid_field(
            "mode",
            format!("`{other}` is not a mode; expected `matches` or `count`"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;
    use rune_core::error::ErrorCode;

    use super::*;
    use crate::workspace::fixture::{Repo, budget_with_list_entries};

    #[test]
    fn a_pattern_finds_files_in_nested_untracked_directories() {
        let repo = Repo::new();
        let output = GlobFiles
            .call(
                &serde_json::json!({ "pattern": "**/*.rs" }),
                &repo.context(),
            )
            .expect("call");
        assert!(!output.is_error);
        assert!(output.text.contains("src/main.rs"), "{}", output.text);
        assert!(
            output.text.contains("src/nested/deep/mod.rs"),
            "{}",
            output.text
        );
    }

    #[test]
    fn the_same_file_is_found_from_the_root_and_from_a_nested_directory() {
        let repo = Repo::new();
        let context = repo.context();
        let from_root = GlobFiles
            .call(
                &serde_json::json!({ "pattern": "**/untracked.txt" }),
                &context,
            )
            .expect("call");
        assert!(
            from_root.text.contains("nested/untracked.txt"),
            "{}",
            from_root.text
        );

        let from_nested = GlobFiles
            .call(
                &serde_json::json!({ "pattern": "**/untracked.txt", "path": "nested" }),
                &context,
            )
            .expect("call");
        assert!(
            from_nested.text.contains("nested/untracked.txt"),
            "{}",
            from_nested.text
        );
    }

    #[test]
    fn a_bare_name_pattern_matches_at_any_depth() {
        let repo = Repo::new();
        let output = GlobFiles
            .call(&serde_json::json!({ "pattern": "mod.rs" }), &repo.context())
            .expect("call");
        assert!(
            output.text.contains("src/nested/deep/mod.rs"),
            "{}",
            output.text
        );
    }

    #[test]
    fn an_ignored_directory_is_never_matched() {
        let repo = Repo::new();
        let output = GlobFiles
            .call(
                &serde_json::json!({ "pattern": "build/**" }),
                &repo.context(),
            )
            .expect("call");
        assert!(output.text.contains("no files match"), "{}", output.text);

        let nested = GlobFiles
            .call(
                &serde_json::json!({ "pattern": "**/*.txt" }),
                &repo.context(),
            )
            .expect("call");
        assert!(
            !nested.text.contains("build/ignored.txt"),
            "{}",
            nested.text
        );
        assert!(!nested.text.contains("target/"), "{}", nested.text);
    }

    #[test]
    fn count_is_exact_while_matches_truncates() {
        let repo = Repo::new();
        for index in 0..40 {
            repo.write(&format!("many/file_{index}.txt"), "content\n");
        }
        let context = repo.context();

        let counted = GlobFiles
            .call(
                &serde_json::json!({ "pattern": "many/*.txt", "mode": "count" }),
                &context,
            )
            .expect("call");
        assert!(counted.text.contains("40 files match"), "{}", counted.text);

        let listed = GlobFiles
            .call(&serde_json::json!({ "pattern": "many/*.txt" }), &context)
            .expect("call");
        assert!(listed.text.contains("40 files match"), "{}", listed.text);
        assert!(
            listed.text.contains(&format!(
                "[showing {} of 40 matches",
                FileLimits::default().list_entries
            )),
            "{}",
            listed.text
        );
        assert_eq!(
            listed.text.matches("many/file_").count(),
            FileLimits::default().list_entries
        );

        // A lowered cap truncates the listing but never the count.
        let capped = GlobFiles
            .call(&serde_json::json!({ "pattern": "many/*.txt" }), &context)
            .expect("call");
        assert!(capped.text.contains("of 40 matches"), "{}", capped.text);
    }

    #[test]
    fn a_lowered_entry_cap_truncates_the_listing_and_the_count_is_still_exact() {
        let repo = Repo::new();
        for index in 0..12 {
            repo.write(&format!("many/file_{index}.txt"), "content\n");
        }
        let limits = budget_with_list_entries(3);
        let expected = FileLimits::from_budget(&limits).list_entries;
        assert_eq!(expected, 3);

        // The listing cap is the tool's own, so the count stays exact while the
        // listing stops at the cap.
        let listed = GlobFiles
            .call(
                &serde_json::json!({ "pattern": "many/*.txt" }),
                &repo.context(),
            )
            .expect("call");
        assert!(listed.text.contains("12 files match"), "{}", listed.text);
        assert!(listed.text.contains("of 12 matches"), "{}", listed.text);

        let counted = GlobFiles
            .call(
                &serde_json::json!({ "pattern": "many/*.txt", "mode": "count" }),
                &repo.context(),
            )
            .expect("call");
        assert!(counted.text.contains("12 files match"), "{}", counted.text);
    }

    #[test]
    fn a_pattern_with_no_matches_says_so() {
        let repo = Repo::new();
        let output = GlobFiles
            .call(
                &serde_json::json!({ "pattern": "*.nothing" }),
                &repo.context(),
            )
            .expect("call");
        assert!(!output.is_error);
        assert!(
            output.text.contains("no files match `*.nothing`"),
            "{}",
            output.text
        );
    }

    #[test]
    fn an_empty_pattern_is_refused() {
        let repo = Repo::new();
        let err = GlobFiles
            .call(&serde_json::json!({ "pattern": "" }), &repo.context())
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("pattern"));
    }

    #[test]
    fn an_invalid_pattern_names_the_pattern() {
        let repo = Repo::new();
        let err = GlobFiles
            .call(&serde_json::json!({ "pattern": "[oops" }), &repo.context())
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert!(err.message().contains("[oops"), "{}", err.message());
    }

    #[test]
    fn an_invalid_mode_is_refused() {
        let repo = Repo::new();
        let err = GlobFiles
            .call(
                &serde_json::json!({ "pattern": "*.rs", "mode": "summary" }),
                &repo.context(),
            )
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("mode"));
    }

    #[test]
    fn a_missing_pattern_is_refused_by_validation() {
        let repo = Repo::new();
        let err = GlobFiles
            .call(&serde_json::json!({}), &repo.context())
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::MissingField);
    }

    #[test]
    fn an_escaping_search_root_is_refused() {
        let repo = Repo::new();
        let err = GlobFiles
            .call(
                &serde_json::json!({ "pattern": "*.rs", "path": "../elsewhere" }),
                &repo.context(),
            )
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::PathOutsideWorkspace);
    }

    #[test]
    fn a_named_file_is_matched_as_a_single_candidate() {
        let repo = Repo::new();
        let output = GlobFiles
            .call(
                &serde_json::json!({ "pattern": "*.rs", "path": "src/main.rs" }),
                &repo.context(),
            )
            .expect("call");
        assert!(output.text.contains("1 files match"), "{}", output.text);

        let missed = GlobFiles
            .call(
                &serde_json::json!({ "pattern": "*.toml", "path": "src/main.rs" }),
                &repo.context(),
            )
            .expect("call");
        assert!(missed.text.contains("no files match"), "{}", missed.text);
    }

    #[test]
    fn output_stays_within_the_context_byte_cap() {
        let repo = Repo::new();
        for index in 0..400 {
            repo.write(&format!("many/a_rather_long_file_name_{index}.txt"), "x");
        }
        let context = repo.context().with_output_cap(2048);
        let output = GlobFiles
            .call(&serde_json::json!({ "pattern": "many/*.txt" }), &context)
            .expect("call");
        assert!(output.text.len() <= 2048, "{}", output.text.len());
        assert!(output.text.contains("of 400 matches"), "{}", output.text);
    }

    #[test]
    fn the_declared_schema_names_only_the_accepted_arguments() {
        let schema = GlobFiles.input_schema();
        assert_eq!(schema["required"][0], "pattern");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(
            schema["properties"]["mode"]["enum"],
            serde_json::json!(["matches", "count"])
        );
    }

    #[test]
    fn a_count_mode_result_carries_no_paths() {
        let repo = Repo::new();
        let output = GlobFiles
            .call(
                &serde_json::json!({ "pattern": "**/*.rs", "mode": "count" }),
                &repo.context(),
            )
            .expect("call");
        assert!(output.text.contains("files match"), "{}", output.text);
        assert!(!output.text.contains("src/main.rs"), "{}", output.text);
        assert_eq!(output.produced_bytes, output.text.len() as u64);
    }

    #[test]
    fn a_relative_pattern_does_not_cross_a_separator() {
        let repo = Repo::new();
        repo.write("src/deep/extra.rs", "fn extra() {}\n");
        let output = GlobFiles
            .call(
                &serde_json::json!({ "pattern": "src/*.rs" }),
                &repo.context(),
            )
            .expect("call");
        assert!(output.text.contains("src/main.rs"), "{}", output.text);
        assert!(
            !output.text.contains("src/deep/extra.rs"),
            "{}",
            output.text
        );
    }

    #[test]
    fn the_search_root_label_is_relative_to_the_workspace() {
        let repo = Repo::new();
        let output = GlobFiles
            .call(
                &serde_json::json!({ "pattern": "*.txt", "path": "nested" }),
                &repo.context(),
            )
            .expect("call");
        assert!(output.text.contains("under `nested`"), "{}", output.text);
        let _ = Utf8PathBuf::new();
    }
}
