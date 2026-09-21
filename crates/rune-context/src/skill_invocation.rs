//! Loading skill instructions on demand.
//!
//! A skill body enters the prompt only when the skill is used, and then whole:
//! a body larger than `skill_file_bytes` is refused rather than cut, because a
//! truncated skill is a set of instructions the model would follow incorrectly,
//! and a refusal is visible while a silently shortened rule is not.
//!
//! A skill may ship files next to its `SKILL.md`. A reference to one resolves
//! from the skill directory, never from the workspace, and a reference that
//! leaves that directory is refused: the directory is the unit the user
//! reviewed, and a path that escapes it would load content the skill never
//! declared.

use camino::{Utf8Component, Utf8Path};

use rune_core::budget::LimitName;
use rune_core::error::{ErrorCode, Result, RuneError};

use crate::limits::{Limits, effective};
use crate::skills::Skill;

/// Prefix of the location form advertised in the skill catalog.
pub const LOCATION_PREFIX: &str = "skill:";

/// The reference returned for a skill that names itself.
pub const SELF_REFERENCE: &str = ".";

/// A skill's instructions, read whole.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LoadedSkill {
    /// The skill the instructions came from.
    pub skill: Skill,
    /// Full text of the file, frontmatter included.
    pub instructions: String,
    /// Size of `instructions` in bytes.
    pub bytes: usize,
}

impl LoadedSkill {
    /// Renders the body as a delimited section.
    ///
    /// The delimiter is what lets the model tell skill instructions from the
    /// turn's own text, so it is emitted even for an empty body.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "<skill_content name=\"{}\">\n{}\n</skill_content>\n",
            escape(&self.skill.name),
            self.instructions
        )
    }
}

/// Reads a skill's whole `SKILL.md`, refusing one over `skill_file_bytes`.
///
/// The file is also checked against its own directory, so a discovered entry
/// whose link was replaced after discovery cannot read something the skill never
/// declared.
pub fn load_whole(skill: &Skill, limits: &Limits) -> Result<LoadedSkill> {
    let path = skill.location.as_path();
    let directory = skill.directory();
    if leaves(path, directory) {
        return Err(RuneError::new(
            ErrorCode::UnsafePath,
            format!("skill file {path} is outside the skill directory {directory}"),
        ));
    }
    let instructions = read_within(path, limits)?;
    Ok(LoadedSkill {
        skill: skill.clone(),
        bytes: instructions.len(),
        instructions,
    })
}

/// Resolves a resource path from the skill directory.
///
/// The path is relative to the skill, so `refs/example.md` means the file next
/// to that skill's `SKILL.md` rather than one in the workspace. A path that
/// leaves the skill directory, by `..` or by a symlink, is refused.
pub fn resolve_reference(skill: &Skill, relative: &str, limits: &Limits) -> Result<String> {
    let relative = relative.trim();
    if relative.is_empty() {
        return Err(RuneError::invalid_field(
            "resource",
            "a skill resource path cannot be empty",
        ));
    }
    if Utf8Path::new(relative).is_absolute() {
        return Err(RuneError::invalid_field(
            "resource",
            format!("`{relative}` is absolute; a resource is named relative to its skill"),
        ));
    }
    let directory = skill.directory();
    let candidate = directory.join(relative);
    if leaves(candidate.as_path(), directory) {
        return Err(RuneError::new(
            ErrorCode::UnsafePath,
            format!("skill resource `{relative}` leaves the skill directory {directory}"),
        ));
    }
    if !candidate.is_file() {
        return Err(RuneError::new(
            ErrorCode::NotFound,
            format!("skill resource `{relative}` does not exist at {candidate}"),
        ));
    }
    read_within(candidate.as_path(), limits)
}

/// Resolves a skill from a `skill:` location.
///
/// Two forms resolve. The catalog advertises `skill:<path>`, the full path of
/// the skill's file, and a caller may also write `skill:<root>/<leaf>`, where
/// the leaf is relative to the root that declared the skill, so `skill:alpha`
/// names the `alpha` skill of a root. Names are not deduplicated across roots,
/// so a leaf that two discovered skills answer to is reported as ambiguous
/// rather than resolved by discovery order.
pub fn load_by_location(skills: &[Skill], location: &str, limits: &Limits) -> Result<LoadedSkill> {
    let location = location.trim();
    let Some(advertised) = location.strip_prefix(LOCATION_PREFIX) else {
        return Err(RuneError::invalid_field(
            "location",
            format!(
                "`{location}` is not a skill location; expected `{LOCATION_PREFIX}<root>/<leaf>`"
            ),
        ));
    };
    let advertised = advertised.trim_end_matches('/');
    if advertised.is_empty() {
        return Err(RuneError::invalid_field(
            "location",
            "a skill location must name a skill",
        ));
    }
    let requested = Utf8Path::new(advertised);
    let matched: Vec<&Skill> = skills
        .iter()
        .filter(|skill| advertises(skill, requested))
        .collect();
    match matched.as_slice() {
        [] => Err(RuneError::new(
            ErrorCode::NotFound,
            format!("no discovered skill is located at `{location}`"),
        )
        .with_hint("list the catalog to see the current skill locations")),
        [skill] => load_whole(skill, limits),
        [first, rest @ ..] => {
            let mut locations = format!("{}", first.location);
            for skill in rest {
                locations.push_str(", ");
                locations.push_str(skill.location.as_str());
            }
            Err(RuneError::new(
                ErrorCode::AmbiguousMatch,
                format!(
                    "`{location}` names {} skills: {locations}",
                    rest.len().saturating_add(1)
                ),
            )
            .with_hint("address the skill by its full path to name one of them"))
        }
    }
}

/// Returns true when a location under a skill's root names that skill.
///
/// The location is joined to the root rather than to the skill directory,
/// because the root is the part the catalog makes explicit and the leaf is what
/// a caller omits. An absolute location joins as itself, so the full path of one
/// skill's file always addresses that one skill.
fn advertises(skill: &Skill, requested: &Utf8Path) -> bool {
    let candidate = skill.root.join(requested);
    candidate == *skill.directory() || candidate == *skill.location
}

/// Reads a file whole, refusing one larger than the skill file limit.
///
/// The size on disk is checked before the read, so a hostile or oversized file
/// is never loaded into memory in order to discover that it is too large.
fn read_within(path: &Utf8Path, limits: &Limits) -> Result<String> {
    let limit = effective(LimitName::SkillFileBytes, limits);
    let metadata = std::fs::metadata(path).map_err(|err| {
        RuneError::new(
            ErrorCode::NotFound,
            format!("{path} could not be read: {err}"),
        )
    })?;
    let observed = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    if observed > limit {
        return Err(
            RuneError::too_large(LimitName::SkillFileBytes.as_str(), observed, limit)
                .with_invariant("skill file fits skill_file_bytes"),
        );
    }
    std::fs::read_to_string(path).map_err(|err| {
        RuneError::new(
            ErrorCode::InvalidField,
            format!("{path} could not be read as text: {err}"),
        )
    })
}

/// Returns true when a path escapes a directory, by `..` or by a symlink.
///
/// Only `..` and a Windows prefix are rejected on their own, because those are
/// the escapes visible without touching the filesystem. An existing path is
/// canonicalized as well, which catches a link to a file outside the directory.
fn leaves(path: &Utf8Path, directory: &Utf8Path) -> bool {
    if path.components().any(|component| {
        matches!(
            component,
            Utf8Component::ParentDir | Utf8Component::Prefix(_)
        )
    }) {
        return true;
    }
    let (Ok(resolved), Ok(root)) = (
        std::fs::canonicalize(path),
        std::fs::canonicalize(directory),
    ) else {
        return false;
    };
    let (Some(resolved), Some(root)) = (Utf8Path::from_path(&resolved), Utf8Path::from_path(&root))
    else {
        return false;
    };
    !resolved.starts_with(root)
}

/// Escapes the characters that would break out of an attribute value.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(character),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SKILL_FILE;
    use camino::Utf8PathBuf;
    use rune_core::budget::BudgetSet;
    use tempfile::TempDir;

    /// Creates a fixture tree and returns its root.
    fn tree() -> (TempDir, Utf8PathBuf) {
        let dir = TempDir::new().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 tempdir");
        (dir, root)
    }

    /// Writes a fixture file, creating its parents.
    fn write(path: &Utf8Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, contents).expect("write fixture");
    }

    /// Builds a skill rooted at a fixture directory.
    fn skill(root: &Utf8Path, name: &str, body: &str) -> Skill {
        let directory = root.join(name);
        write(
            &directory.join(SKILL_FILE),
            &format!("---\nname: {name}\n---\n{body}"),
        );
        Skill {
            name: name.to_owned(),
            description: None,
            location: directory.join(SKILL_FILE),
            root: root.to_owned(),
        }
    }

    /// Returns the limits with every compiled default.
    fn limits() -> Limits {
        Limits::resolve(&BudgetSet::new()).expect("resolve")
    }

    /// Returns the file limit as a length.
    fn file_limit() -> usize {
        effective(LimitName::SkillFileBytes, &limits())
    }

    #[test]
    fn a_whole_skill_loads_and_renders_inside_its_delimiter() {
        let (_dir, root) = tree();
        let skill = skill(&root, "alpha", "First rule.\nSecond rule.\n");

        let loaded = load_whole(&skill, &limits()).expect("load");
        assert!(loaded.instructions.contains("First rule.\nSecond rule."));
        assert_eq!(loaded.bytes, loaded.instructions.len());
        assert_eq!(loaded.skill.name, "alpha");

        let rendered = loaded.render();
        assert!(rendered.starts_with("<skill_content name=\"alpha\">\n"));
        assert!(rendered.ends_with("</skill_content>\n"));
        assert!(rendered.contains("Second rule."));
    }

    #[test]
    fn a_skill_name_is_escaped_inside_the_delimiter() {
        let (_dir, root) = tree();
        let mut skill = skill(&root, "alpha", "body\n");
        skill.name = "a\"b<c>".to_owned();

        let rendered = load_whole(&skill, &limits()).expect("load").render();
        assert!(rendered.contains("name=\"a&quot;b&lt;c&gt;\""));
    }

    #[test]
    fn an_oversized_skill_is_refused_naming_the_size_and_the_limit() {
        let (_dir, root) = tree();
        let directory = root.join("big");
        let limit = file_limit();
        write(
            &directory.join(SKILL_FILE),
            &"x".repeat(limit.saturating_add(1)),
        );
        let skill = Skill {
            name: "big".to_owned(),
            description: None,
            location: directory.join(SKILL_FILE),
            root: root.clone(),
        };

        let err = load_whole(&skill, &limits()).expect_err("too large");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(err.field(), Some(LimitName::SkillFileBytes.as_str()));
        assert!(err.message().contains(&limit.saturating_add(1).to_string()));
        assert!(err.message().contains(&limit.to_string()));
        assert_eq!(
            err.detail().invariant.as_deref(),
            Some("skill file fits skill_file_bytes")
        );
    }

    #[test]
    fn a_skill_at_the_limit_loads() {
        let (_dir, root) = tree();
        let directory = root.join("exact");
        write(&directory.join(SKILL_FILE), &"x".repeat(file_limit()));
        let skill = Skill {
            name: "exact".to_owned(),
            description: None,
            location: directory.join(SKILL_FILE),
            root: root.clone(),
        };

        assert_eq!(
            load_whole(&skill, &limits()).expect("load").bytes,
            file_limit()
        );
    }

    #[test]
    fn a_configured_file_limit_is_honoured() {
        let (_dir, root) = tree();
        let skill = skill(&root, "alpha", &"x".repeat(4096));
        let mut set = BudgetSet::new();
        set.set(
            LimitName::SkillFileBytes,
            rune_core::budget::Budget::Bounded(1024),
            rune_core::config::Layer::User,
        )
        .expect("set");
        let limits = Limits::resolve(&set).expect("resolve");

        let err = load_whole(&skill, &limits).expect_err("too large");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert!(err.message().contains("1024"), "{}", err.message());
    }

    #[test]
    fn a_missing_skill_file_is_reported_as_not_found() {
        let (_dir, root) = tree();
        let absent = Skill {
            name: "gone".to_owned(),
            description: None,
            location: root.join("gone/SKILL.md"),
            root: root.clone(),
        };

        assert_eq!(
            load_whole(&absent, &limits()).expect_err("missing").code(),
            ErrorCode::NotFound
        );
    }

    #[test]
    fn a_resource_resolves_from_the_skill_directory() {
        let (_dir, root) = tree();
        let skill = skill(&root, "alpha", "body\n");
        write(&root.join("alpha/refs/example.md"), "reference text\n");

        assert_eq!(
            resolve_reference(&skill, "refs/example.md", &limits()).expect("resolve"),
            "reference text\n"
        );
        assert_eq!(
            resolve_reference(&skill, "./refs/example.md", &limits()).expect("resolve"),
            "reference text\n"
        );
    }

    #[test]
    fn a_resource_is_read_from_the_skill_rather_than_the_workspace() {
        let (_dir, root) = tree();
        let skill = skill(&root, "alpha", "body\n");
        write(&root.join("notes.md"), "workspace copy\n");
        write(&root.join("alpha/notes.md"), "skill copy\n");

        let resolved = resolve_reference(&skill, "notes.md", &limits()).expect("resolve");
        assert_eq!(resolved, "skill copy\n");
    }

    #[test]
    fn a_reference_leaving_the_skill_directory_is_refused() {
        let (_dir, root) = tree();
        let skill = skill(&root, "alpha", "body\n");
        write(&root.join("secret.md"), "outside\n");

        let err = resolve_reference(&skill, "../secret.md", &limits()).expect_err("escape");
        assert_eq!(err.code(), ErrorCode::UnsafePath);
        assert!(
            err.message().contains("../secret.md"),
            "the refusal names the path: {}",
            err.message()
        );
        assert_eq!(
            resolve_reference(&skill, "refs/../../secret.md", &limits())
                .expect_err("escape")
                .code(),
            ErrorCode::UnsafePath
        );
    }

    #[test]
    fn an_absolute_reference_is_refused() {
        let (_dir, root) = tree();
        let skill = skill(&root, "alpha", "body\n");
        let absolute = root.join("alpha/notes.md");
        write(&absolute, "text\n");

        let err = resolve_reference(&skill, absolute.as_str(), &limits()).expect_err("absolute");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("resource"));
        assert!(err.message().contains("absolute"));
    }

    #[test]
    fn a_reference_through_a_symlink_out_of_the_skill_is_refused() {
        let (_dir, root) = tree();
        let skill = skill(&root, "alpha", "body\n");
        write(&root.join("outside.md"), "outside\n");
        std::os::unix::fs::symlink(root.join("outside.md"), root.join("alpha/linked.md"))
            .expect("symlink");

        let err = resolve_reference(&skill, "linked.md", &limits()).expect_err("escape");
        assert_eq!(err.code(), ErrorCode::UnsafePath);
        assert!(err.message().contains("linked.md"));
    }

    #[test]
    fn a_missing_resource_is_reported_as_not_found() {
        let (_dir, root) = tree();
        let skill = skill(&root, "alpha", "body\n");

        let err = resolve_reference(&skill, "refs/absent.md", &limits()).expect_err("missing");
        assert_eq!(err.code(), ErrorCode::NotFound);
        assert!(err.message().contains("refs/absent.md"));
    }

    #[test]
    fn an_oversized_resource_is_refused_naming_the_size_and_the_limit() {
        let (_dir, root) = tree();
        let skill = skill(&root, "alpha", "body\n");
        let limit = file_limit();
        write(
            &root.join("alpha/refs/big.md"),
            &"x".repeat(limit.saturating_add(1)),
        );

        let err = resolve_reference(&skill, "refs/big.md", &limits()).expect_err("too large");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert!(err.message().contains(&limit.saturating_add(1).to_string()));
        assert!(err.message().contains(&limit.to_string()));
    }

    #[test]
    fn a_blank_reference_is_refused() {
        let (_dir, root) = tree();
        let skill = skill(&root, "alpha", "body\n");

        assert_eq!(
            resolve_reference(&skill, "  ", &limits())
                .expect_err("blank")
                .code(),
            ErrorCode::InvalidField
        );
    }

    #[test]
    fn an_advertised_location_resolves_its_skill() {
        let (_dir, root) = tree();
        let alpha = skill(&root, "alpha", "alpha body\n");
        let beta = skill(&root, "beta", "beta body\n");
        let skills = vec![alpha.clone(), beta];

        for location in [
            format!("{LOCATION_PREFIX}{}", alpha.location),
            format!("{LOCATION_PREFIX}{}/", alpha.directory()),
            format!("{LOCATION_PREFIX}alpha"),
        ] {
            let loaded = load_by_location(&skills, &location, &limits()).expect("load");
            assert_eq!(loaded.skill.name, "alpha");
            assert!(loaded.instructions.contains("alpha body"));
        }
    }

    #[test]
    fn two_skills_sharing_a_name_are_reported_as_ambiguous() {
        let (_dir, root) = tree();
        let first = skill(&root.join("one"), "shared", "first body\n");
        let second = skill(&root.join("two"), "shared", "second body\n");
        let skills = vec![first.clone(), second.clone()];

        let err = load_by_location(&skills, &format!("{LOCATION_PREFIX}shared"), &limits())
            .expect_err("ambiguous");
        assert_eq!(err.code(), ErrorCode::AmbiguousMatch);
        assert_eq!(
            err.message().matches(SKILL_FILE).count(),
            2,
            "both locations are named: {}",
            err.message()
        );
        assert!(err.message().contains(first.location.as_str()));
        assert!(err.message().contains(second.location.as_str()));
        assert!(!err.message().contains("first body"));
    }

    #[test]
    fn a_name_two_skills_share_still_resolves_by_its_full_location() {
        let (_dir, root) = tree();
        let first = skill(&root.join("one"), "shared", "first body\n");
        let second = skill(&root.join("two"), "shared", "second body\n");
        let skills = vec![first.clone(), second.clone()];

        for (location, body) in [
            (second.directory().to_owned(), "second body"),
            (first.location.clone(), "first body"),
        ] {
            let loaded =
                load_by_location(&skills, &format!("{LOCATION_PREFIX}{location}"), &limits())
                    .expect("load");
            assert!(loaded.instructions.contains(body), "{location}");
        }
    }

    #[test]
    fn an_unknown_location_is_reported_as_not_found() {
        let (_dir, root) = tree();
        let skill = skill(&root, "alpha", "body\n");
        let location = format!("{LOCATION_PREFIX}{root}/skills/gone/SKILL.md");

        let err = load_by_location(&[skill], &location, &limits()).expect_err("missing");
        assert_eq!(err.code(), ErrorCode::NotFound);
        assert!(err.message().contains(&location));
    }

    #[test]
    fn a_location_without_the_prefix_is_refused() {
        let (_dir, root) = tree();
        let skill = skill(&root, "alpha", "body\n");

        let err = load_by_location(&[skill], "/skills/alpha/SKILL.md", &limits())
            .expect_err("not a location");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert!(err.message().contains(LOCATION_PREFIX));
    }
}
