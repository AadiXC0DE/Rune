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
pub mod mcp;
pub mod prompt;
pub mod skills;

pub use catalog::{CatalogOutput, read_body, render_catalog};
// `discover` exists in both modules with a different signature, so each stays
// reachable through its module rather than being re-exported bare.
pub use instructions::{InstructionFile, Options, render, resolve_for_target};
pub use skills::{Discovery, SKILL_FILE, Skill, USER_ROOTS, WORKSPACE_ROOTS, Warning};

use rune_core::budget::{EMERGENCY_CEILING_BYTES, LimitName};

/// Resolves a limit to a byte or item count.
///
/// The emergency ceiling stands in for a limit set to `off`, so an unbounded
/// setting cannot make discovery or rendering unbounded.
pub(crate) fn resolve_limit(name: LimitName) -> usize {
    usize::try_from(name.default_value().effective(EMERGENCY_CEILING_BYTES)).unwrap_or(usize::MAX)
}
