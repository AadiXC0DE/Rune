//! Tools for working with skills.
//!
//! A skill's description is advertised in the system prompt; its instructions are
//! not. Loading them is something the model asks for explicitly, which is what
//! keeps a catalog of a hundred skills from costing a hundred files of context.
//!
//! Invocation goes by location rather than by name, because two roots may hold
//! different skills that share a name and a name would be ambiguous.

use std::fmt::Write as _;
use std::sync::Arc;

use rune_context::limits::Limits;
use rune_context::skill_invocation::{self, SELF_REFERENCE};
use rune_context::skills::Skill;
use rune_core::error::{Result, RuneError};

use crate::contract::{Activity, ExecutionContext, Tool, ToolOutput};

/// A source of skills, so the tool can be driven without a filesystem walk.
pub trait Catalog: Send + Sync {
    /// Returns every skill the catalog holds.
    fn skills(&self) -> Vec<Skill>;
}

/// A catalog that always reports nothing.
///
/// A run with no workspace to scan gets this, so the tool reports an empty
/// catalog rather than failing.
#[derive(Clone, Copy, Debug, Default)]
pub struct Empty;

impl Catalog for Empty {
    fn skills(&self) -> Vec<Skill> {
        Vec::new()
    }
}

/// A fixed catalog, for tests and for a host that supplies its own.
#[derive(Clone, Debug, Default)]
pub struct Fixed {
    skills: Vec<Skill>,
}

impl Fixed {
    /// Builds a catalog from a list.
    #[must_use]
    pub fn new(skills: Vec<Skill>) -> Self {
        Self { skills }
    }
}

impl Catalog for Fixed {
    fn skills(&self) -> Vec<Skill> {
        self.skills.clone()
    }
}

/// Validates an argument object against a closed set of fields.
///
/// An unknown field is refused rather than ignored, because a misspelled argument
/// would otherwise produce a call that silently did the wrong thing.
fn require_only(arguments: &serde_json::Value, allowed: &[&str]) -> Result<()> {
    let Some(object) = arguments.as_object() else {
        return Err(RuneError::invalid_field(
            "arguments",
            "the tool expects an object",
        ));
    };
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(RuneError::invalid_field(
                key.as_str(),
                format!("`{key}` is not an argument this tool accepts"),
            )
            .with_hint(format!("accepted arguments: {}", allowed.join(", "))));
        }
    }
    Ok(())
}

/// Reads a required string argument.
fn required_string(arguments: &serde_json::Value, field: &str) -> Result<String> {
    arguments
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| RuneError::missing_field(field))
}

/// Loads one skill's instructions.
pub struct LoadSkill {
    catalog: Arc<dyn Catalog>,
    limits: Limits,
}

impl std::fmt::Debug for LoadSkill {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadSkill").finish_non_exhaustive()
    }
}

impl LoadSkill {
    /// Builds the tool.
    #[must_use]
    pub fn new(catalog: Arc<dyn Catalog>, limits: Limits) -> Self {
        Self { catalog, limits }
    }
}

impl Tool for LoadSkill {
    fn name(&self) -> &'static str {
        "skill"
    }

    fn description(&self) -> &'static str {
        "Load the full instructions for one skill named in the catalog. Pass the \
         location exactly as the catalog lists it. The instructions are returned \
         once, wrapped in a delimited section, and are not added to any other \
         request."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "location": {
                    "type": "string",
                    "description": "Location of the skill, as the catalog lists it."
                },
                "reference": {
                    "type": "string",
                    "description": "Optional path to a file the skill names, relative to the skill. Use `.` for the skill itself."
                }
            },
            "required": ["location"],
            "additionalProperties": false
        })
    }

    fn activity(&self) -> Activity {
        Activity::Read
    }

    fn call(
        &self,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        context.check_cancelled()?;
        require_only(arguments, &["location", "reference"])?;
        let location = required_string(arguments, "location")?;
        let skills = self.catalog.skills();

        let reference = arguments
            .get("reference")
            .and_then(serde_json::Value::as_str)
            .map_or(SELF_REFERENCE, str::trim);

        if reference.is_empty() || reference == SELF_REFERENCE {
            let loaded = skill_invocation::load_by_location(&skills, &location, &self.limits)?;
            return Ok(ToolOutput::success(loaded.render()));
        }

        let loaded = skill_invocation::load_by_location(&skills, &location, &self.limits)?;
        let text = skill_invocation::resolve_reference(&loaded.skill, reference, &self.limits)?;
        Ok(ToolOutput::success(format!(
            "<skill_file name=\"{}\" path=\"{reference}\">\n{text}\n</skill_file>\n",
            loaded.skill.name
        )))
    }
}

/// Searches the catalog by name and description.
pub struct CapabilitySearch {
    catalog: Arc<dyn Catalog>,
}

impl std::fmt::Debug for CapabilitySearch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapabilitySearch").finish_non_exhaustive()
    }
}

impl CapabilitySearch {
    /// Builds the tool.
    #[must_use]
    pub fn new(catalog: Arc<dyn Catalog>) -> Self {
        Self { catalog }
    }
}

impl Tool for CapabilitySearch {
    fn name(&self) -> &'static str {
        "capability_search"
    }

    fn description(&self) -> &'static str {
        "Search the skill catalog by name and description and return matching \
         locations. Use it to find a skill when the catalog in the instructions \
         does not obviously contain one."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Words to match against skill names and descriptions."
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    fn activity(&self) -> Activity {
        Activity::Read
    }

    fn call(
        &self,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        context.check_cancelled()?;
        require_only(arguments, &["query"])?;
        let query = required_string(arguments, "query")?;

        let terms: Vec<String> = query
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .filter(|term| !term.is_empty())
            .collect();
        if terms.is_empty() {
            return Err(RuneError::invalid_field(
                "query",
                "the query has no words to match against",
            ));
        }

        let mut matches = Vec::new();
        for skill in self.catalog.skills() {
            let haystack = format!(
                "{}\n{}",
                skill.name.to_ascii_lowercase(),
                skill
                    .description
                    .as_deref()
                    .unwrap_or("")
                    .to_ascii_lowercase()
            );
            // Every term must appear, so adding a word narrows the result
            // rather than widening it.
            if terms.iter().all(|term| haystack.contains(term)) {
                matches.push(skill);
            }
        }

        if matches.is_empty() {
            return Ok(ToolOutput::success(format!("no skill matches `{query}`")));
        }

        let mut out = String::new();
        for skill in matches {
            let _ = writeln!(
                out,
                "{}\n  {}",
                skill.location,
                skill.description.as_deref().unwrap_or("(no description)")
            );
        }
        Ok(ToolOutput::success(out.trim_end().to_owned()))
    }
}

/// Installs a skill into the managed directory.
///
/// The root must be a directory that discovery scans, or an installed skill is
/// never found again. [`InstallSkill::under_config`] builds the one that is.
pub struct InstallSkill {
    root: camino::Utf8PathBuf,
    limits: Limits,
}

impl std::fmt::Debug for InstallSkill {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstallSkill")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

/// Largest skill file that may be installed.
pub const MAX_SKILL_BYTES: u64 = 256 * 1024;

impl InstallSkill {
    /// Builds the tool, writing only under `root`.
    ///
    /// `root` is the directory holding skill directories, which discovery scans.
    #[must_use]
    pub fn new(root: camino::Utf8PathBuf, limits: Limits) -> Self {
        Self { root, limits }
    }

    /// Builds the tool writing into the managed skills directory.
    ///
    /// This is the directory discovery scans, so a skill installed here is
    /// visible to the catalog without any further registration.
    #[must_use]
    pub fn under_config(paths: &rune_core::paths::Paths, limits: Limits) -> Self {
        Self::new(paths.config_root.join("skills"), limits)
    }

    /// Returns the directory skills are written into.
    #[must_use]
    pub fn root(&self) -> &camino::Utf8Path {
        &self.root
    }
}

impl Tool for InstallSkill {
    fn name(&self) -> &'static str {
        "install_skill"
    }

    fn description(&self) -> &'static str {
        "Install a skill by writing its file into the managed skill directory. \
         Reports the final path. The name must be a single path segment."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Directory name for the skill. One path segment."
                },
                "content": {
                    "type": "string",
                    "description": "Full contents of the skill file, frontmatter included."
                }
            },
            "required": ["name", "content"],
            "additionalProperties": false
        })
    }

    fn activity(&self) -> Activity {
        Activity::Write
    }

    fn call(
        &self,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        context.check_cancelled()?;
        require_only(arguments, &["name", "content"])?;
        let name = required_string(arguments, "name")?;
        let content = required_string(arguments, "content")?;

        // A single segment only: a name containing a separator would write
        // outside the managed directory, and `..` is a relative component rather
        // than a name.
        let valid = !name.is_empty()
            && name != "."
            && name != ".."
            && !name.contains('/')
            && !name.contains('\\')
            && !name.contains('\0');
        if !valid {
            return Err(RuneError::invalid_field(
                "name",
                format!("`{name}` is not a single path segment"),
            )
            .with_hint("use a plain directory name, without a separator"));
        }

        if content.trim().is_empty() {
            return Err(RuneError::invalid_field(
                "content",
                "the skill file would be empty",
            ));
        }
        if u64::try_from(content.len()).unwrap_or(u64::MAX) > MAX_SKILL_BYTES {
            return Err(RuneError::too_large(
                "content",
                content.len(),
                usize::try_from(MAX_SKILL_BYTES).unwrap_or(usize::MAX),
            )
            .with_hint("split the skill, or move the extra material into a referenced file"));
        }

        let directory = self.root.join(&name);
        let path = directory.join(rune_context::skills::SKILL_FILE);
        rune_core::paths::create_dir_private(&directory)?;
        rune_core::paths::write_private(&path, &content)?;

        // The written file is read back, so the reported size is what is on disk
        // rather than what was submitted.
        let written = std::fs::metadata(&path).map_err(RuneError::from)?;
        let _ = &self.limits;
        // Reported relative to the directory skills are read from, and in the
        // spelling every other path is shown in, so a reader sees the location
        // the catalog names rather than an absolute path of this machine.
        let shown = crate::workspace::display_in(std::slice::from_ref(&self.root), &path);
        Ok(ToolOutput::success(format!(
            "installed {name} at {shown} ({} bytes)",
            written.len()
        )))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;
    use camino::Utf8PathBuf;

    fn skill(name: &str, description: &str, root: &Utf8PathBuf) -> Skill {
        Skill {
            name: name.to_owned(),
            description: Some(description.to_owned()),
            location: root.join("SKILL.md"),
            root: root.clone(),
        }
    }

    fn write_skill(root: &Utf8PathBuf, body: &str) {
        std::fs::create_dir_all(root).expect("mkdir");
        std::fs::write(root.join("SKILL.md"), body).expect("write");
    }

    fn context(workspace: &Utf8PathBuf) -> ExecutionContext {
        ExecutionContext::new(workspace.clone())
    }

    fn limits() -> Limits {
        Limits::resolve(&rune_core::budget::BudgetSet::new()).expect("limits")
    }

    #[test]
    fn the_tools_report_their_identity() {
        let catalog: Arc<dyn Catalog> = Arc::new(Fixed::default());
        assert_eq!(
            LoadSkill::new(Arc::clone(&catalog), limits()).name(),
            "skill"
        );
        assert_eq!(CapabilitySearch::new(catalog).name(), "capability_search");
    }

    #[test]
    fn loading_returns_the_instructions_wrapped_in_a_delimited_section() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let skill_root = root.join("skills/deploy");
        write_skill(&skill_root, "---\nname: deploy\n---\nSteps here.\n");
        let catalog: Arc<dyn Catalog> =
            Arc::new(Fixed::new(vec![skill("deploy", "Deploys", &skill_root)]));

        let tool = LoadSkill::new(catalog, limits());
        let location = format!("skill:{}", skill_root.join("SKILL.md"));
        let output = tool
            .call(
                &serde_json::json!({ "location": location }),
                &context(&root),
            )
            .expect("loaded");
        assert!(!output.is_error, "{}", output.text);
        assert!(output.text.contains("<skill_content"), "{}", output.text);
        assert!(output.text.contains("Steps here."), "{}", output.text);
        assert!(output.text.contains("</skill_content>"), "{}", output.text);
    }

    #[test]
    fn an_unknown_field_is_refused_rather_than_ignored() {
        let catalog: Arc<dyn Catalog> = Arc::new(Fixed::default());
        let tool = LoadSkill::new(catalog, limits());
        let err = tool
            .call(
                &serde_json::json!({ "location": "x", "nope": 1 }),
                &context(&Utf8PathBuf::from("/tmp")),
            )
            .expect_err("refused");
        assert_eq!(err.code(), rune_core::error::ErrorCode::InvalidField);
        assert!(err.hint().is_some());
    }

    #[test]
    fn a_missing_location_is_reported() {
        let catalog: Arc<dyn Catalog> = Arc::new(Fixed::default());
        let tool = LoadSkill::new(catalog, limits());
        let err = tool
            .call(&serde_json::json!({}), &context(&Utf8PathBuf::from("/tmp")))
            .expect_err("refused");
        assert_eq!(err.code(), rune_core::error::ErrorCode::MissingField);
    }

    #[test]
    fn search_matches_name_and_description() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let first = root.join("skills/deploy");
        let second = root.join("skills/lint");
        write_skill(&first, "body");
        write_skill(&second, "body");
        let catalog: Arc<dyn Catalog> = Arc::new(Fixed::new(vec![
            skill("deploy", "Ships the service", &first),
            skill("lint", "Checks formatting", &second),
        ]));

        let tool = CapabilitySearch::new(catalog);
        let output = tool
            .call(&serde_json::json!({ "query": "Ships" }), &context(&root))
            .expect("searched");
        assert!(output.text.contains("deploy"), "{}", output.text);
        assert!(!output.text.contains("lint"), "{}", output.text);
    }

    #[test]
    fn every_search_term_must_match() {
        // Adding a word must narrow the result, not widen it.
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let one = root.join("skills/one");
        write_skill(&one, "body");
        let catalog: Arc<dyn Catalog> =
            Arc::new(Fixed::new(vec![skill("deploy", "Ships the service", &one)]));

        let tool = CapabilitySearch::new(catalog);
        let output = tool
            .call(
                &serde_json::json!({ "query": "deploy unrelated" }),
                &context(&root),
            )
            .expect("searched");
        assert!(output.text.contains("no skill matches"), "{}", output.text);
    }

    #[test]
    fn a_search_with_no_words_is_refused() {
        let catalog: Arc<dyn Catalog> = Arc::new(Fixed::default());
        let tool = CapabilitySearch::new(catalog);
        let err = tool
            .call(
                &serde_json::json!({ "query": "   " }),
                &context(&Utf8PathBuf::from("/tmp")),
            )
            .expect_err("refused");
        assert_eq!(err.code(), rune_core::error::ErrorCode::InvalidField);
    }

    #[test]
    fn an_empty_catalog_searches_to_nothing_rather_than_failing() {
        let catalog: Arc<dyn Catalog> = Arc::new(Empty);
        let tool = CapabilitySearch::new(catalog);
        let output = tool
            .call(
                &serde_json::json!({ "query": "anything" }),
                &context(&Utf8PathBuf::from("/tmp")),
            )
            .expect("searched");
        assert!(!output.is_error);
        assert!(output.text.contains("no skill matches"), "{}", output.text);
    }

    #[test]
    fn installing_writes_under_the_managed_directory_and_reports_the_path() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let managed = root.join("managed");
        let tool = InstallSkill::new(managed.clone(), limits());

        let output = tool
            .call(
                &serde_json::json!({ "name": "deploy", "content": "---\nname: deploy\n---\nBody" }),
                &context(&root),
            )
            .expect("installed");
        assert!(!output.is_error, "{}", output.text);
        assert!(output.text.contains("installed deploy"), "{}", output.text);

        let path = managed.join("deploy/SKILL.md");
        assert!(path.exists(), "the skill was not written");
        // The location is reported relative to the managed directory, in the
        // spelling every other path is shown in.
        assert!(
            output.text.contains("at deploy/SKILL.md"),
            "{}",
            output.text
        );
    }

    #[test]
    fn a_name_that_escapes_the_managed_directory_is_refused() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let managed = root.join("managed");
        let tool = InstallSkill::new(managed.clone(), limits());

        for name in ["..", ".", "a/b", "../escape", "a\\b", ""] {
            let err = tool
                .call(
                    &serde_json::json!({ "name": name, "content": "body" }),
                    &context(&root),
                )
                .expect_err("refused");
            assert_eq!(
                err.code(),
                rune_core::error::ErrorCode::InvalidField,
                "`{name}` was accepted"
            );
        }
        assert!(!root.join("escape").exists(), "a traversal wrote outside");
    }

    #[test]
    fn an_oversized_skill_is_refused_naming_the_size_and_the_limit() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let tool = InstallSkill::new(root.join("managed"), limits());

        let big = "x".repeat(usize::try_from(MAX_SKILL_BYTES).expect("fits") + 1);
        let err = tool
            .call(
                &serde_json::json!({ "name": "big", "content": big }),
                &context(&root),
            )
            .expect_err("refused");
        assert_eq!(err.code(), rune_core::error::ErrorCode::TooLarge);
        let text = err.to_string();
        assert!(text.contains(&MAX_SKILL_BYTES.to_string()), "{text}");
    }

    #[test]
    fn an_empty_skill_is_refused() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let tool = InstallSkill::new(root.join("managed"), limits());
        let err = tool
            .call(
                &serde_json::json!({ "name": "blank", "content": "  \n" }),
                &context(&root),
            )
            .expect_err("refused");
        assert_eq!(err.code(), rune_core::error::ErrorCode::InvalidField);
    }

    #[test]
    fn an_installed_skill_is_readable_by_the_load_tool() {
        // The round trip is what makes installation useful: the file must land
        // where discovery and invocation look for it.
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        // The name matters: this is the directory discovery scans.
        let managed = root.join("skills");
        let install = InstallSkill::new(managed.clone(), limits());
        install
            .call(
                &serde_json::json!({
                    "name": "deploy",
                    "content": "---\nname: deploy\ndescription: Ships\n---\nSteps."
                }),
                &context(&root),
            )
            .expect("installed");

        // Discovery looks at a workspace root for a `skills` directory, so the
        // managed directory is the root and the skill sits under it.
        std::fs::create_dir_all(managed.join("deploy")).expect("mkdir");
        std::fs::write(
            managed.join("deploy/SKILL.md"),
            "---\nname: deploy\ndescription: Ships\n---\nSteps.",
        )
        .expect("write");
        let discovered = rune_context::skills::discover(&managed, None, &root).expect("discovered");
        assert_eq!(
            discovered.len(),
            1,
            "the installed skill was not discovered"
        );

        let catalog: Arc<dyn Catalog> = Arc::new(Fixed::new(discovered));
        let load = LoadSkill::new(catalog, limits());
        let location = format!("skill:{}", managed.join("deploy/SKILL.md"));
        let output = load
            .call(
                &serde_json::json!({ "location": location }),
                &context(&root),
            )
            .expect("loaded");
        assert!(output.text.contains("Steps."), "{}", output.text);
    }
}
