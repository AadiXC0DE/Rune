//! Project instructions, skills, MCP client, and prompt assembly.
//!
//! Two sources of context are assembled here. Project instructions come from
//! `AGENTS.md` files and are selected per tool target, so a call inside a nested
//! package sees that package's rules on top of the workspace-wide ones. Skills
//! come from `SKILL.md` directories and enter the prompt as a catalog of names
//! and descriptions, with bodies loaded on demand.

#![forbid(unsafe_code)]
// Tests assert by panicking. The guards that forbid panicking apply to the
// shipped build, where a panic on user input is a defect.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

pub mod catalog;
pub mod instructions;
pub mod limits;
pub mod mcp;
pub mod mcp_trust;
pub mod prompt;
pub mod skill_invocation;
pub mod skills;

pub use catalog::{CatalogOutput, read_body, render_catalog};
// `discover` exists in both modules with a different signature, so each stays
// reachable through its module rather than being re-exported bare.
pub use instructions::{InstructionFile, Options, render, resolve_for_target};
pub use limits::{LimitRow, Limits, describe, effective};
pub use skill_invocation::{LoadedSkill, load_by_location, load_whole, resolve_reference};
pub use skills::{Discovery, SKILL_FILE, Skill, USER_ROOTS, WORKSPACE_ROOTS, Warning};

use rune_core::budget::LimitName;

/// Resolves a limit to a byte or item count from its compiled default.
///
/// Used where no configured set is available, such as a discovery scan. A
/// caller holding a resolved [`Limits`] reads the configured value instead. The
/// emergency ceiling stands in for a limit set to `off` either way, so an
/// unbounded setting cannot make discovery or rendering unbounded.
pub(crate) fn resolve_limit(name: LimitName) -> usize {
    limits::default_limit(name)
}
