//! The skill catalog placed in the prompt.
//!
//! The prompt carries names and descriptions only: a skill body is loaded when
//! the skill is used, which keeps a large skill collection from consuming the
//! input budget of every request. Descriptions are shortened to
//! `skill_description_bytes` so one verbose skill cannot crowd out the rest, and
//! the whole catalog is capped at `skill_catalog_bytes`.
//!
//! A body larger than `skill_file_bytes` is refused rather than cut, because a
//! truncated skill body is instructions the model would follow incorrectly;
//! refusal is visible and recoverable, a silent half rule is not.

use std::fmt::Write as _;

use camino::Utf8Path;

use rune_core::budget::LimitName;
use rune_core::error::{ErrorCode, Result, RuneError};

use crate::resolve_limit;
use crate::skills::Skill;

/// Header placed above the catalog.
const HEADER: &str = "Skills provide task instructions. Use a named skill when one matches.\n";

/// Opening tag of the catalog block.
const OPEN: &str = "<available_skills>\n";

/// Closing tag of the catalog block.
const CLOSE: &str = "</available_skills>\n";

/// Bytes reserved for one notice appended after the catalog.
const NOTICE_BYTES: usize = 256;

/// Renders one notice, shortening the name list when it exceeds its reserve.
///
/// The count is always exact; the names are cut with a trailing marker, because
/// the structured result carries the complete list.
fn notice(tag: &str, attributes: &str, names: &[String]) -> String {
    let head = format!("<{tag} {attributes}");
    if names.is_empty() {
        return format!("{head} />\n");
    }
    let open = format!("{head} count=\"{}\" names=\"", names.len());
    let suffix = "\" />\n";
    // Escaping happens before the list is fitted, because an escaped name is
    // longer than the name it came from and the reserve must cover the text
    // that is actually emitted.
    let joined = escape(&names.join(", "));
    let room = NOTICE_BYTES.saturating_sub(open.len().saturating_add(suffix.len()));
    let mut out = open;
    if joined.len() <= room {
        out.push_str(&joined);
    } else {
        let cut = prefix(&joined, room.saturating_sub(MARKER.len()));
        out.push_str(joined.get(..cut).unwrap_or_default());
        out.push_str(MARKER);
    }
    out.push_str(suffix);
    out
}

/// Appended to a shortened list so the cut is visible.
const MARKER: &str = "...";

/// The rendered skill catalog, with what did not fit.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct CatalogOutput {
    /// Catalog text, empty when nothing was included.
    pub text: String,
    /// Names of the skills that were left out entirely.
    pub omitted: Vec<String>,
    /// Names of the skills whose description was shortened.
    pub shortened: Vec<String>,
}

impl CatalogOutput {
    /// Returns true when the catalog contains no skill.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// Renders the catalog, including at most `limit` skills.
///
/// Every skill that does not fit is named in `omitted`, so a caller can report
/// the gap instead of silently offering an incomplete list. The rendered text
/// stays within `skill_catalog_bytes`: room for the notices is reserved before
/// any skill is accepted, and each notice is itself clamped to
/// [`NOTICE_BYTES`], so the guarantee holds regardless of how long the skill
/// names are.
#[must_use]
pub fn render_catalog(skills: &[Skill], limit: usize) -> CatalogOutput {
    render_catalog_within(
        skills,
        limit,
        resolve_limit(LimitName::SkillCatalogBytes),
        resolve_limit(LimitName::SkillDescriptionBytes),
    )
}

/// Renders the catalog within explicit byte caps.
///
/// Separate from [`render_catalog`] so a caller holding a resolved limit set can
/// honour an override. Reading the compiled default here would make a user's
/// configured catalog size silently ineffective.
#[must_use]
pub fn render_catalog_within(
    skills: &[Skill],
    limit: usize,
    budget: usize,
    per_description: usize,
) -> CatalogOutput {
    let mut out = CatalogOutput::default();
    let fixed = HEADER
        .len()
        .saturating_add(OPEN.len())
        .saturating_add(CLOSE.len())
        .saturating_add(NOTICE_BYTES.saturating_mul(2));
    if fixed > budget {
        // The header alone does not fit, so nothing can be listed. Every skill
        // is recorded as omitted rather than dropped quietly: a caller that
        // shows the omission list, and the model that reads the assembled
        // instructions, both need to know the catalog is absent rather than
        // empty.
        out.omitted = skills.iter().map(|skill| skill.name.clone()).collect();
        return out;
    }

    let mut listed: Vec<String> = Vec::new();
    let mut used = fixed;
    for (index, skill) in skills.iter().enumerate() {
        let (rendered, shortened) = line(skill, per_description);
        if index >= limit || used.saturating_add(rendered.len()) > budget {
            out.omitted.push(skill.name.clone());
            continue;
        }
        used = used.saturating_add(rendered.len());
        if shortened {
            out.shortened.push(skill.name.clone());
        }
        listed.push(rendered);
    }
    if listed.is_empty() {
        return out;
    }

    let mut text = String::new();
    text.push_str(HEADER);
    text.push_str(OPEN);
    for rendered in &listed {
        text.push_str(rendered);
    }
    text.push_str(CLOSE);
    if !out.shortened.is_empty() {
        text.push_str(&notice(
            "skill-descriptions-shortened",
            &format!("limit=\"{}\"", LimitName::SkillDescriptionBytes.as_str()),
            &[],
        ));
    }
    if !out.omitted.is_empty() {
        text.push_str(&notice("skill-catalog-omitted", "", &out.omitted));
    }
    out.text = text;
    out
}

/// Renders one catalog line, shortening a long description.
fn line(skill: &Skill, cap: usize) -> (String, bool) {
    let description = skill.description.as_deref().unwrap_or_default();
    let cut = prefix(description, cap);
    let shortened = cut < description.len();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "- {}: {}{} (location: {})",
        escape(&skill.name),
        escape(description.get(..cut).unwrap_or_default()),
        if shortened { "..." } else { "" },
        escape(skill.location.as_str())
    );
    (out, shortened)
}

/// Returns the largest prefix length of `text` that fits in `max` bytes.
fn prefix(text: &str, max: usize) -> usize {
    if text.len() <= max {
        return text.len();
    }
    let mut end = max.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    end
}

/// Escapes the characters that would break the catalog's framing.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(character),
        }
    }
    out
}

/// Reads a skill body, rejecting one larger than `skill_file_bytes`.
///
/// The whole body is read for the catalog's caller, which is the tool that
/// shows a skill on request. The cap is checked against the size on disk first,
/// so an oversized file is never read at all.
pub fn read_body(path: &Utf8Path) -> Result<String> {
    let limit = usize::try_from(
        LimitName::SkillFileBytes
            .default_value()
            .value()
            .unwrap_or(0),
    )
    .unwrap_or(usize::MAX);
    let meta = std::fs::metadata(path).map_err(RuneError::from)?;
    let observed = usize::try_from(meta.len()).unwrap_or(usize::MAX);
    if observed > limit {
        return Err(
            RuneError::too_large(LimitName::SkillFileBytes.as_str(), observed, limit)
                .with_invariant("skill body fits skill_file_bytes"),
        );
    }
    std::fs::read_to_string(path).map_err(|err| {
        RuneError::new(
            ErrorCode::InvalidField,
            format!("skill body at {path} could not be read as text: {err}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    /// Builds a skill with a description of a given length.
    fn skill(name: &str, description: Option<usize>) -> Skill {
        Skill {
            name: name.to_owned(),
            description: description.map(|len| "d".repeat(len)),
            location: Utf8PathBuf::from(format!("/skills/{name}/SKILL.md")),
            root: Utf8PathBuf::from("/skills"),
        }
    }

    /// Returns the resolved cap for a limit.
    fn cap(name: LimitName) -> usize {
        resolve_limit(name)
    }

    #[test]
    fn the_catalog_lists_names_and_descriptions_only() {
        let skills = vec![skill("alpha", Some(4)), skill("beta", None)];
        let output = render_catalog(&skills, 10);

        assert!(output.text.contains("- alpha: dddd"));
        assert!(output.text.contains("- beta:"));
        assert!(output.text.contains("location: /skills/alpha/SKILL.md"));
        assert!(output.omitted.is_empty());
        assert!(output.shortened.is_empty());
    }

    #[test]
    fn the_catalog_stays_within_its_byte_limit_and_reports_omissions() {
        let per_skill = cap(LimitName::SkillDescriptionBytes).saturating_mul(2);
        let skills: Vec<Skill> = (0..64)
            .map(|index| skill(&format!("skill-{index:03}"), Some(per_skill)))
            .collect();

        let output = render_catalog(&skills, 64);
        let budget = cap(LimitName::SkillCatalogBytes);

        assert!(output.text.len() <= budget);
        assert!(!output.shortened.is_empty());
        assert!(!output.omitted.is_empty());
        assert!(output.text.contains("<skill-catalog-omitted"));
        assert!(output.text.contains("<skill-descriptions-shortened"));
        assert!(
            output.text.contains(&output.omitted[0]),
            "the notice names what was left out"
        );
        assert_eq!(
            output.omitted.len() + output.text.matches("\n- ").count(),
            skills.len(),
            "every skill is either listed or reported as omitted"
        );
    }

    #[test]
    fn the_limit_bounds_how_many_skills_enter_the_catalog() {
        let skills: Vec<Skill> = (0..8)
            .map(|index| skill(&format!("s{index}"), Some(4)))
            .collect();

        let output = render_catalog(&skills, 3);
        assert_eq!(output.omitted, vec!["s3", "s4", "s5", "s6", "s7"]);
        assert!(!output.text.contains("- s3:"));
        assert!(output.text.contains("count=\"5\""));
    }

    #[test]
    fn a_long_description_is_shortened_and_named() {
        let long = cap(LimitName::SkillDescriptionBytes).saturating_mul(4);
        let skills = vec![skill("verbose", Some(long))];

        let output = render_catalog(&skills, 10);
        assert_eq!(output.shortened, vec!["verbose"]);
        assert!(output.text.contains("..."));
        assert!(
            !output
                .text
                .contains(&"d".repeat(cap(LimitName::SkillDescriptionBytes).saturating_add(1)))
        );
    }

    #[test]
    fn a_catalog_with_escaped_names_stays_within_its_byte_limit() {
        let per_skill = cap(LimitName::SkillDescriptionBytes).saturating_mul(2);
        let skills: Vec<Skill> = (0..64)
            .map(|index| skill(&format!("<skill-{index:03}>&\"quoted\""), Some(per_skill)))
            .collect();

        let output = render_catalog(&skills, 64);
        assert!(output.text.len() <= cap(LimitName::SkillCatalogBytes));
        assert!(!output.omitted.is_empty());
        assert!(output.text.contains("&lt;skill-000&gt;"));
        let notice = output
            .text
            .lines()
            .find(|line| line.contains("skill-catalog-omitted"))
            .expect("notice");
        assert!(
            notice.ends_with("...\" />"),
            "the list was shortened: {notice}"
        );
        assert!(notice.len() <= NOTICE_BYTES);
    }

    #[test]
    fn no_skill_produces_an_empty_catalog() {
        let output = render_catalog(&[], 10);
        assert!(output.is_empty());
        assert!(output.omitted.is_empty());
    }

    #[test]
    fn a_zero_limit_omits_every_skill_by_name() {
        let skills = vec![skill("alpha", Some(4)), skill("beta", Some(4))];
        let output = render_catalog(&skills, 0);

        assert!(output.is_empty());
        assert_eq!(output.omitted, vec!["alpha", "beta"]);
    }

    #[test]
    fn markup_in_a_name_is_escaped() {
        let skills = vec![skill("a<b>c&d", Some(2))];
        let output = render_catalog(&skills, 10);
        assert!(output.text.contains("a&lt;b&gt;c&amp;d"));
    }

    #[test]
    fn a_body_over_the_cap_is_rejected_naming_the_size_and_the_limit() {
        let dir = TempDir::new().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(dir.path().join("SKILL.md")).expect("utf8");
        let limit = cap(LimitName::SkillFileBytes);
        std::fs::write(&path, "x".repeat(limit.saturating_add(1))).expect("write");

        let err = read_body(&path).expect_err("too large");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(err.field(), Some(LimitName::SkillFileBytes.as_str()));
        let message = err.message();
        assert!(message.contains(&limit.saturating_add(1).to_string()));
        assert!(message.contains(&limit.to_string()));
    }

    #[test]
    fn a_body_at_the_cap_is_read() {
        let dir = TempDir::new().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(dir.path().join("SKILL.md")).expect("utf8");
        let limit = cap(LimitName::SkillFileBytes);
        std::fs::write(&path, "x".repeat(limit)).expect("write");

        assert_eq!(read_body(&path).expect("read").len(), limit);
    }

    #[test]
    fn a_missing_body_is_reported_as_not_found() {
        let dir = TempDir::new().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(dir.path().join("absent/SKILL.md")).expect("utf8");
        let err = read_body(&path).expect_err("missing");
        assert_eq!(err.code(), ErrorCode::NotFound);
    }
}
