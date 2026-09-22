//! Every tunable limit in the product.
//!
//! Each limit has a name, a compiled default, an accepted range, and a settings
//! key. Limits are data rather than constants so that `rune limits --json` can
//! report the effective value and the layer it came from, and so that a missing
//! limit is a test failure rather than an undocumented behavior.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{ErrorCode, Result, RuneError};

/// A limit value: either a bounded quantity or an explicit opt-out.
///
/// The opt-out is written as `off`, which is the spelling the command line and
/// the documentation use. It is not a number, so the encoding cannot be a plain
/// integer: without this the documented spelling would be unreadable in a
/// configuration file and a limit could never be switched off from one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Budget {
    /// A bounded quantity, in the unit the limit is defined in.
    Bounded(u64),
    /// No normal limit. A hard emergency ceiling still applies.
    Unbounded,
}

impl Serialize for Budget {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Bounded(value) => serializer.serialize_u64(*value),
            Self::Unbounded => serializer.serialize_str("off"),
        }
    }
}

impl<'de> Deserialize<'de> for Budget {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = Budget;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a count, or `off` to remove the limit")
            }

            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Budget, E> {
                Ok(Budget::Bounded(value))
            }

            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Budget, E> {
                u64::try_from(value)
                    .map(Budget::Bounded)
                    .map_err(|_| E::custom("a limit cannot be negative"))
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Budget, E> {
                if value.eq_ignore_ascii_case("off") {
                    return Ok(Budget::Unbounded);
                }
                value
                    .parse::<u64>()
                    .map(Budget::Bounded)
                    .map_err(|_| E::custom(format!("`{value}` is not a count or `off`")))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

impl Budget {
    /// A limit of zero, used as the lower bound of a range.
    pub const ZERO: Self = Self::Bounded(0);

    /// Returns the bounded value, or `None` when unbounded.
    #[must_use]
    pub const fn value(self) -> Option<u64> {
        match self {
            Self::Bounded(v) => Some(v),
            Self::Unbounded => None,
        }
    }

    /// Returns the effective value, substituting `ceiling` when unbounded.
    #[must_use]
    pub const fn effective(self, ceiling: u64) -> u64 {
        match self {
            Self::Bounded(v) => v,
            Self::Unbounded => ceiling,
        }
    }

    /// Returns true when the limit permits an observed size.
    #[must_use]
    pub const fn permits(self, observed: u64) -> bool {
        match self {
            Self::Bounded(v) => observed <= v,
            Self::Unbounded => true,
        }
    }
}

impl fmt::Display for Budget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bounded(v) => write!(f, "{v}"),
            Self::Unbounded => f.write_str("off"),
        }
    }
}

impl FromStr for Budget {
    type Err = RuneError;

    fn from_str(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        if trimmed.eq_ignore_ascii_case("off") {
            return Ok(Self::Unbounded);
        }
        let value: u64 = trimmed.parse().map_err(|_| {
            RuneError::new(
                ErrorCode::InvalidField,
                format!("`{raw}` is not a non-negative integer or `off`"),
            )
        })?;
        Ok(Self::Bounded(value))
    }
}

/// The unit a limit is expressed in.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    /// Bytes.
    Bytes,
    /// Milliseconds.
    Millis,
    /// A count of items.
    Count,
}

impl Unit {
    /// Returns the short suffix used in documentation.
    #[must_use]
    pub const fn suffix(self) -> &'static str {
        match self {
            Self::Bytes => "bytes",
            Self::Millis => "ms",
            Self::Count => "items",
        }
    }
}

/// Inclusive accepted range for a limit.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Range {
    /// Smallest accepted value.
    pub min: u64,
    /// Largest accepted value, or `None` when there is no upper bound.
    pub max: Option<u64>,
}

impl Range {
    /// Returns true when the value falls inside the range.
    #[must_use]
    pub const fn contains(&self, value: u64) -> bool {
        if value < self.min {
            return false;
        }
        match self.max {
            Some(max) => value <= max,
            None => true,
        }
    }
}

/// Every configurable limit in the product.
///
/// Adding a variant requires a default, a range, a unit, and a documentation
/// entry. The consistency test fails otherwise.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LimitName {
    /// Maximum model tool-loop steps per turn. Zero means unlimited.
    MaxAgentSteps,
    /// Maximum bytes retained from one tool result.
    MaxToolResultBytes,
    /// Tool-result bytes retained across one turn.
    MaxTurnResultBytes,
    /// Combined size of the skill catalog placed in the prompt.
    SkillCatalogBytes,
    /// Size of one skill description in the catalog.
    SkillDescriptionBytes,
    /// Size of a skill file returned by one read.
    SkillFileBytes,
    /// Size of one MCP tool description.
    McpDescriptionBytes,
    /// Size of MCP tool search results.
    McpSearchResultBytes,
    /// Instructions received from one MCP server.
    McpServerInstructionsBytes,
    /// Schema size for one explicitly selected MCP tool.
    McpSelectedSchemaBytes,
    /// Size of one project instruction file.
    ProjectInstructionFileBytes,
    /// Combined size of all applicable project instructions.
    ProjectInstructionsTotalBytes,
    /// Text produced by an image analysis adapter.
    ImageAdapterOutputBytes,
    /// Bytes retained from one command's output.
    CommandOutputBytes,
    /// Lines returned by one file read.
    ReadFileLines,
    /// Bytes per line before truncation in a file read.
    ReadFileLineBytes,
    /// Entries returned by one directory or glob listing.
    ListEntries,
    /// Separate concurrent tool calls in one step.
    ParallelToolCalls,
    /// Children a parent session may register.
    SubagentChildren,
    /// Time to wait for a model response head before failing the attempt.
    ProviderHeadTimeoutMs,
    /// Total time allowed for one model request attempt.
    ProviderRequestTimeoutMs,
    /// Attempts made against a provider before failing the turn.
    ProviderMaxAttempts,
    /// Time allowed for one MCP operation.
    McpOperationTimeoutMs,
    /// Time allowed for an MCP server to start and complete discovery.
    McpStartupTimeoutMs,
    /// Automatic restarts permitted for a local MCP server.
    McpRestartLimit,
    /// Bytes of the current turn's tool results shown to the safety reviewer.
    ReviewContextBytes,
    /// Safety review holds permitted in one turn.
    ReviewHoldsPerTurn,
    /// Time allowed for one safety review request.
    ReviewTimeoutMs,
    /// Compaction trigger as a percentage of usable input capacity.
    CompactionTriggerPercent,
    /// Redirects followed by one web fetch.
    WebFetchRedirects,
    /// Bytes returned by one web fetch.
    WebFetchBytes,
    /// Time allowed for one web fetch.
    WebFetchTimeoutMs,
    /// Sources returned by one web search.
    WebSearchResults,
    /// Images analysed in one vision request.
    VisionBatchImages,
    /// Distinct additional workspace roots.
    AdditionalDirectories,
    /// Bytes stored in the prompt history file before compaction.
    PromptHistoryBytes,
    /// Messages accepted into a steering queue for one turn.
    SteeringQueueDepth,
}

impl LimitName {
    /// Every limit, in a stable order.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::MaxAgentSteps,
            Self::MaxToolResultBytes,
            Self::MaxTurnResultBytes,
            Self::SkillCatalogBytes,
            Self::SkillDescriptionBytes,
            Self::SkillFileBytes,
            Self::McpDescriptionBytes,
            Self::McpSearchResultBytes,
            Self::McpServerInstructionsBytes,
            Self::McpSelectedSchemaBytes,
            Self::ProjectInstructionFileBytes,
            Self::ProjectInstructionsTotalBytes,
            Self::ImageAdapterOutputBytes,
            Self::CommandOutputBytes,
            Self::ReadFileLines,
            Self::ReadFileLineBytes,
            Self::ListEntries,
            Self::ParallelToolCalls,
            Self::SubagentChildren,
            Self::ProviderHeadTimeoutMs,
            Self::ProviderRequestTimeoutMs,
            Self::ProviderMaxAttempts,
            Self::McpOperationTimeoutMs,
            Self::McpStartupTimeoutMs,
            Self::McpRestartLimit,
            Self::ReviewContextBytes,
            Self::ReviewHoldsPerTurn,
            Self::ReviewTimeoutMs,
            Self::CompactionTriggerPercent,
            Self::WebFetchRedirects,
            Self::WebFetchBytes,
            Self::WebFetchTimeoutMs,
            Self::WebSearchResults,
            Self::VisionBatchImages,
            Self::AdditionalDirectories,
            Self::PromptHistoryBytes,
            Self::SteeringQueueDepth,
        ]
    }

    /// The configuration key, identical to the wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MaxAgentSteps => "max_agent_steps",
            Self::MaxToolResultBytes => "max_tool_result_bytes",
            Self::MaxTurnResultBytes => "max_turn_result_bytes",
            Self::SkillCatalogBytes => "skill_catalog_bytes",
            Self::SkillDescriptionBytes => "skill_description_bytes",
            Self::SkillFileBytes => "skill_file_bytes",
            Self::McpDescriptionBytes => "mcp_description_bytes",
            Self::McpSearchResultBytes => "mcp_search_result_bytes",
            Self::McpServerInstructionsBytes => "mcp_server_instructions_bytes",
            Self::McpSelectedSchemaBytes => "mcp_selected_schema_bytes",
            Self::ProjectInstructionFileBytes => "project_instruction_file_bytes",
            Self::ProjectInstructionsTotalBytes => "project_instructions_total_bytes",
            Self::ImageAdapterOutputBytes => "image_adapter_output_bytes",
            Self::CommandOutputBytes => "command_output_bytes",
            Self::ReadFileLines => "read_file_lines",
            Self::ReadFileLineBytes => "read_file_line_bytes",
            Self::ListEntries => "list_entries",
            Self::ParallelToolCalls => "parallel_tool_calls",
            Self::SubagentChildren => "subagent_children",
            Self::ProviderHeadTimeoutMs => "provider_head_timeout_ms",
            Self::ProviderRequestTimeoutMs => "provider_request_timeout_ms",
            Self::ProviderMaxAttempts => "provider_max_attempts",
            Self::McpOperationTimeoutMs => "mcp_operation_timeout_ms",
            Self::McpStartupTimeoutMs => "mcp_startup_timeout_ms",
            Self::McpRestartLimit => "mcp_restart_limit",
            Self::ReviewContextBytes => "review_context_bytes",
            Self::ReviewHoldsPerTurn => "review_holds_per_turn",
            Self::ReviewTimeoutMs => "review_timeout_ms",
            Self::CompactionTriggerPercent => "compaction_trigger_percent",
            Self::WebFetchRedirects => "web_fetch_redirects",
            Self::WebFetchBytes => "web_fetch_bytes",
            Self::WebFetchTimeoutMs => "web_fetch_timeout_ms",
            Self::WebSearchResults => "web_search_results",
            Self::VisionBatchImages => "vision_batch_images",
            Self::AdditionalDirectories => "additional_directories",
            Self::PromptHistoryBytes => "prompt_history_bytes",
            Self::SteeringQueueDepth => "steering_queue_depth",
        }
    }

    /// The compiled default.
    #[must_use]
    pub const fn default_value(self) -> Budget {
        match self {
            // Zero means unlimited steps. Embedded hosts lower this.
            Self::MaxAgentSteps => Budget::Bounded(0),
            Self::MaxToolResultBytes => Budget::Bounded(64 * 1024),
            Self::MaxTurnResultBytes => Budget::Bounded(8 * 1024 * 1024),
            Self::SkillCatalogBytes => Budget::Bounded(32768),
            Self::SkillDescriptionBytes => Budget::Bounded(1024),
            Self::SkillFileBytes => Budget::Bounded(1024 * 1024),
            Self::McpDescriptionBytes => Budget::Bounded(1024),
            Self::McpSearchResultBytes => Budget::Bounded(16 * 1024),
            Self::McpServerInstructionsBytes => Budget::Bounded(2 * 1024),
            Self::McpSelectedSchemaBytes => Budget::Bounded(64 * 1024),
            Self::ProjectInstructionFileBytes => Budget::Bounded(64 * 1024),
            Self::ProjectInstructionsTotalBytes => Budget::Bounded(128 * 1024),
            Self::ImageAdapterOutputBytes => Budget::Bounded(20 * 1024),
            Self::CommandOutputBytes => Budget::Bounded(64 * 1024),
            Self::ReadFileLines => Budget::Bounded(2000),
            Self::ReadFileLineBytes => Budget::Bounded(2000),
            Self::ListEntries => Budget::Bounded(1000),
            Self::ParallelToolCalls => Budget::Bounded(8),
            Self::SubagentChildren => Budget::Bounded(256),
            Self::ProviderHeadTimeoutMs => Budget::Bounded(120_000),
            Self::ProviderRequestTimeoutMs => Budget::Bounded(600_000),
            Self::ProviderMaxAttempts => Budget::Bounded(10),
            Self::McpOperationTimeoutMs => Budget::Bounded(60_000),
            Self::McpStartupTimeoutMs => Budget::Bounded(30_000),
            Self::McpRestartLimit => Budget::Bounded(1),
            Self::ReviewContextBytes => Budget::Bounded(8 * 1024),
            Self::ReviewHoldsPerTurn => Budget::Bounded(64),
            Self::ReviewTimeoutMs => Budget::Bounded(30_000),
            Self::CompactionTriggerPercent => Budget::Bounded(80),
            Self::WebFetchRedirects => Budget::Bounded(5),
            Self::WebFetchBytes => Budget::Bounded(1024 * 1024),
            Self::WebFetchTimeoutMs => Budget::Bounded(30_000),
            Self::WebSearchResults => Budget::Bounded(10),
            Self::VisionBatchImages => Budget::Bounded(8),
            Self::AdditionalDirectories => Budget::Bounded(16),
            Self::PromptHistoryBytes => Budget::Bounded(1024 * 1024),
            Self::SteeringQueueDepth => Budget::Bounded(64),
        }
    }

    /// Accepted range for an override.
    #[must_use]
    pub const fn range(self) -> Range {
        match self {
            Self::MaxAgentSteps => Range {
                min: 0,
                max: Some(10_000),
            },
            Self::CompactionTriggerPercent => Range {
                min: 10,
                max: Some(99),
            },
            Self::ParallelToolCalls => Range {
                min: 1,
                max: Some(64),
            },
            Self::SubagentChildren => Range {
                min: 0,
                max: Some(4096),
            },
            Self::ProviderMaxAttempts => Range {
                min: 1,
                max: Some(20),
            },
            Self::McpRestartLimit => Range {
                min: 0,
                max: Some(10),
            },
            Self::VisionBatchImages => Range {
                min: 1,
                max: Some(16),
            },
            Self::AdditionalDirectories => Range {
                min: 0,
                max: Some(64),
            },
            Self::WebFetchRedirects => Range {
                min: 0,
                max: Some(20),
            },
            Self::ProviderHeadTimeoutMs => Range {
                min: 1000,
                max: Some(3_600_000),
            },
            Self::ProviderRequestTimeoutMs => Range {
                min: 1000,
                max: Some(7_200_000),
            },
            // Everything else accepts any non-negative value.
            _ => Range { min: 0, max: None },
        }
    }

    /// The unit the value is expressed in.
    #[must_use]
    pub const fn unit(self) -> Unit {
        match self {
            Self::ProviderHeadTimeoutMs
            | Self::ProviderRequestTimeoutMs
            | Self::McpOperationTimeoutMs
            | Self::McpStartupTimeoutMs
            | Self::ReviewTimeoutMs
            | Self::WebFetchTimeoutMs => Unit::Millis,
            Self::ParallelToolCalls
            | Self::SubagentChildren
            | Self::McpRestartLimit
            | Self::ReviewHoldsPerTurn
            | Self::CompactionTriggerPercent
            | Self::WebFetchRedirects
            | Self::WebSearchResults
            | Self::VisionBatchImages
            | Self::AdditionalDirectories
            | Self::SteeringQueueDepth
            | Self::MaxAgentSteps => Unit::Count,
            Self::ReadFileLines => Unit::Count,
            _ => Unit::Bytes,
        }
    }

    /// Short description used in `limits --json` and in documentation.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::MaxAgentSteps => "Maximum model tool-loop steps per turn; zero means unlimited.",
            Self::MaxToolResultBytes => "Bytes retained from one tool result before spilling.",
            Self::MaxTurnResultBytes => "Tool-result bytes one turn retains across its steps.",
            Self::SkillCatalogBytes => "Combined size of the skill catalog placed in the prompt.",
            Self::SkillDescriptionBytes => "Size of one skill description in the catalog.",
            Self::SkillFileBytes => "Largest skill file that may be loaded.",
            Self::McpDescriptionBytes => "Size of one MCP tool description.",
            Self::McpSearchResultBytes => "Size of MCP tool search results.",
            Self::McpServerInstructionsBytes => "Instructions accepted from one MCP server.",
            Self::McpSelectedSchemaBytes => "Schema size for one explicitly selected MCP tool.",
            Self::ProjectInstructionFileBytes => "Size of one project instruction file.",
            Self::ProjectInstructionsTotalBytes => {
                "Combined size of all applicable project instructions."
            }
            Self::ImageAdapterOutputBytes => "Text produced by an image analysis adapter.",
            Self::CommandOutputBytes => "Bytes retained from one command's output.",
            Self::ReadFileLines => "Lines returned by one file read.",
            Self::ReadFileLineBytes => "Bytes per line before truncation in a file read.",
            Self::ListEntries => "Entries returned by one listing or glob.",
            Self::ParallelToolCalls => "Tool calls executed concurrently within one step.",
            Self::SubagentChildren => "Subagent children one parent session may register.",
            Self::ProviderHeadTimeoutMs => "Time to wait for a model response head.",
            Self::ProviderRequestTimeoutMs => "Total time allowed for one model request attempt.",
            Self::ProviderMaxAttempts => "Provider attempts before the turn fails.",
            Self::McpOperationTimeoutMs => "Time allowed for one MCP operation.",
            Self::McpStartupTimeoutMs => {
                "Time allowed for an MCP server to start and be discovered."
            }
            Self::McpRestartLimit => "Automatic restarts permitted for a local MCP server.",
            Self::ReviewContextBytes => "Current-turn tool output shown to the safety reviewer.",
            Self::ReviewHoldsPerTurn => "Safety review holds permitted in one turn.",
            Self::ReviewTimeoutMs => "Time allowed for one safety review request.",
            Self::CompactionTriggerPercent => "Compaction trigger as a percentage of usable input.",
            Self::WebFetchRedirects => "Redirects followed by one web fetch.",
            Self::WebFetchBytes => "Bytes returned by one web fetch.",
            Self::WebFetchTimeoutMs => "Time allowed for one web fetch.",
            Self::WebSearchResults => "Sources returned by one web search.",
            Self::VisionBatchImages => "Images analysed in one vision request.",
            Self::AdditionalDirectories => "Additional workspace roots permitted.",
            Self::PromptHistoryBytes => "Prompt history size before compaction.",
            Self::SteeringQueueDepth => "Messages accepted into a steering queue for one turn.",
        }
    }
}

impl fmt::Display for LimitName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for LimitName {
    type Err = RuneError;

    fn from_str(raw: &str) -> Result<Self> {
        Self::all()
            .iter()
            .copied()
            .find(|name| name.as_str() == raw)
            .ok_or_else(|| {
                RuneError::new(
                    ErrorCode::InvalidField,
                    format!("`{raw}` is not a known limit"),
                )
                .with_hint("run `rune limits` to list every limit")
            })
    }
}

/// Hard ceiling applied even when a limit is set to unbounded.
///
/// Exists so that `off` cannot be used to disable a bound entirely, which would
/// make a hostile or broken input able to exhaust memory.
pub const EMERGENCY_CEILING_BYTES: u64 = 64 * 1024 * 1024;

/// A resolved set of limits with the source of each value.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BudgetSet {
    overrides: std::collections::BTreeMap<LimitName, (Budget, crate::config::Layer)>,
}

impl BudgetSet {
    /// Returns a set with no overrides, so every limit resolves to its default.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an override for one limit.
    ///
    /// The value is validated against the limit's range before being stored.
    pub fn set(
        &mut self,
        name: LimitName,
        value: Budget,
        layer: crate::config::Layer,
    ) -> Result<()> {
        if let Some(raw) = value.value().filter(|raw| !name.range().contains(*raw)) {
            {
                let range = name.range();
                return Err(RuneError::invalid_field(
                    name.as_str(),
                    format!(
                        "`{}` was given {raw}, outside the accepted range {}{}",
                        name.as_str(),
                        range.min,
                        match range.max {
                            Some(max) => format!("..{max}"),
                            None => String::new(),
                        }
                    ),
                ));
            }
        }
        self.overrides.insert(name, (value, layer));
        Ok(())
    }

    /// Returns the effective value for a limit.
    #[must_use]
    pub fn get(&self, name: LimitName) -> Budget {
        self.overrides
            .get(&name)
            .map_or_else(|| name.default_value(), |(value, _)| *value)
    }

    /// Returns the effective value clamped by the emergency ceiling.
    #[must_use]
    pub fn get_bytes(&self, name: LimitName) -> u64 {
        self.get(name).effective(EMERGENCY_CEILING_BYTES)
    }

    /// Returns the effective value as a `usize`, clamped by the ceiling.
    ///
    /// Used where a length is needed. The ceiling is far below `usize::MAX` on
    /// every supported platform, so the conversion is lossless in practice.
    #[must_use]
    pub fn get_usize(&self, name: LimitName) -> usize {
        let value = self.get_bytes(name);
        usize::try_from(value).unwrap_or(usize::MAX)
    }

    /// Returns the layer that supplied a limit, or `None` when it is defaulted.
    #[must_use]
    pub fn source(&self, name: LimitName) -> Option<crate::config::Layer> {
        self.overrides.get(&name).map(|(_, layer)| *layer)
    }

    /// Returns every limit with its effective value, default, unit, and source.
    #[must_use]
    pub fn describe(&self) -> Vec<LimitDescription> {
        LimitName::all()
            .iter()
            .copied()
            .map(|name| LimitDescription {
                name,
                value: self.get(name),
                default: name.default_value(),
                unit: name.unit(),
                min: name.range().min,
                max: name.range().max,
                source: self.source(name),
                description: name.description(),
            })
            .collect()
    }
}

/// One row of `rune limits --json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LimitDescription {
    /// Limit name, also the configuration key.
    pub name: LimitName,
    /// Effective value.
    pub value: Budget,
    /// Compiled default.
    pub default: Budget,
    /// Unit the value is expressed in.
    pub unit: Unit,
    /// Smallest accepted value.
    pub min: u64,
    /// Largest accepted value, absent when unbounded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<u64>,
    /// Layer that supplied the value, absent when it is the default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<crate::config::Layer>,
    /// One-line description.
    pub description: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Layer;

    #[test]
    fn every_limit_has_a_distinct_key() {
        let mut seen = std::collections::HashSet::new();
        for name in LimitName::all() {
            assert!(seen.insert(name.as_str()), "duplicate key {name:?}");
        }
    }

    #[test]
    fn every_limit_key_is_snake_case() {
        for name in LimitName::all() {
            let key = name.as_str();
            assert!(
                key.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "not snake_case: {key}"
            );
        }
    }

    #[test]
    fn every_limit_has_a_description() {
        for name in LimitName::all() {
            assert!(
                name.description().len() > 10,
                "missing description for {name:?}"
            );
        }
    }

    #[test]
    fn defaults_are_inside_their_own_range() {
        for name in LimitName::all() {
            if let Some(value) = name.default_value().value() {
                assert!(
                    name.range().contains(value),
                    "default {value} outside range for {name:?}"
                );
            }
        }
    }

    #[test]
    fn limit_parses_from_its_key() {
        for name in LimitName::all() {
            let parsed: LimitName = name.as_str().parse().expect("parse");
            assert_eq!(parsed, *name);
        }
    }

    #[test]
    fn unknown_limit_name_is_rejected() {
        let err = "not_a_limit".parse::<LimitName>().expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert!(err.detail().hint.is_some());
    }

    #[test]
    fn budget_parses_off_and_numbers() {
        assert_eq!("off".parse::<Budget>().expect("off"), Budget::Unbounded);
        assert_eq!("OFF".parse::<Budget>().expect("off"), Budget::Unbounded);
        assert_eq!(
            "1024".parse::<Budget>().expect("num"),
            Budget::Bounded(1024)
        );
        assert!("-1".parse::<Budget>().is_err());
        assert!("abc".parse::<Budget>().is_err());
    }

    use toml::{Table, Value};

    #[test]
    fn the_opt_out_round_trips_through_its_written_form() {
        // `off` is the spelling the command line and the documentation use, so
        // a configuration file must accept it; without that the documented
        // spelling would be unreadable.
        let written = toml::to_string(&Table::from_iter([(
            "limit".to_owned(),
            Value::try_from(Budget::Unbounded).expect("encoded"),
        )]))
        .expect("serialized");
        assert!(written.contains("\"off\""), "{written}");

        let parsed: Table = toml::from_str(&written).expect("parsed");
        let value: Budget = parsed["limit"].clone().try_into().expect("decoded");
        assert_eq!(value, Budget::Unbounded);
    }

    #[test]
    fn a_count_round_trips_as_a_number() {
        // Written as a number rather than a string, so a hand-edited file reads
        // the way a user would write it.
        let written = toml::to_string(&Table::from_iter([(
            "limit".to_owned(),
            Value::try_from(Budget::Bounded(42)).expect("encoded"),
        )]))
        .expect("serialized");
        assert!(written.contains("42"), "{written}");
        assert!(!written.contains('"'), "{written}");

        let parsed: Table = toml::from_str(&written).expect("parsed");
        let value: Budget = parsed["limit"].clone().try_into().expect("decoded");
        assert_eq!(value, Budget::Bounded(42));
    }

    #[test]
    fn a_negative_limit_is_refused_rather_than_wrapping() {
        let table: Table = toml::from_str("limit = -1").expect("parsed");
        let result: std::result::Result<Budget, _> = table["limit"].clone().try_into();
        assert!(result.is_err(), "a negative limit was accepted");
    }

    #[test]
    fn an_unrecognized_word_is_refused() {
        let table: Table = toml::from_str("limit = \"none\"").expect("parsed");
        let result: std::result::Result<Budget, _> = table["limit"].clone().try_into();
        assert!(result.is_err(), "an unknown word was accepted as a limit");
    }

    #[test]
    fn unbounded_budget_still_honors_the_emergency_ceiling() {
        let mut set = BudgetSet::new();
        set.set(LimitName::SkillCatalogBytes, Budget::Unbounded, Layer::User)
            .expect("set");
        assert_eq!(
            set.get_bytes(LimitName::SkillCatalogBytes),
            EMERGENCY_CEILING_BYTES
        );
    }

    #[test]
    fn override_outside_range_is_rejected() {
        let mut set = BudgetSet::new();
        let err = set
            .set(
                LimitName::CompactionTriggerPercent,
                Budget::Bounded(5),
                Layer::User,
            )
            .expect_err("out of range");
        assert_eq!(err.code(), ErrorCode::InvalidField);
    }

    #[test]
    fn override_is_reported_with_its_layer() {
        let mut set = BudgetSet::new();
        set.set(LimitName::ListEntries, Budget::Bounded(50), Layer::Project)
            .expect("set");
        assert_eq!(set.get(LimitName::ListEntries), Budget::Bounded(50));
        assert_eq!(set.source(LimitName::ListEntries), Some(Layer::Project));
    }

    #[test]
    fn unset_limit_reports_its_default_and_no_source() {
        let set = BudgetSet::new();
        assert_eq!(
            set.get(LimitName::ListEntries),
            LimitName::ListEntries.default_value()
        );
        assert_eq!(set.source(LimitName::ListEntries), None);
    }

    #[test]
    fn describe_covers_every_limit_exactly_once() {
        let described = BudgetSet::new().describe();
        assert_eq!(described.len(), LimitName::all().len());
        for row in &described {
            assert!(!row.description.is_empty());
        }
    }

    #[test]
    fn get_usize_is_lossless_for_defaults() {
        let set = BudgetSet::new();
        for name in LimitName::all() {
            let value = set.get_usize(*name);
            assert!(value > 0 || matches!(name, LimitName::MaxAgentSteps));
        }
    }

    #[test]
    fn max_agent_steps_zero_means_unlimited_steps() {
        assert_eq!(LimitName::MaxAgentSteps.default_value(), Budget::Bounded(0));
    }

    #[test]
    fn permits_respects_bounds() {
        assert!(Budget::Bounded(10).permits(10));
        assert!(!Budget::Bounded(10).permits(11));
        assert!(Budget::Unbounded.permits(u64::MAX));
    }

    #[test]
    fn range_check_handles_open_upper_bound() {
        let range = Range { min: 0, max: None };
        assert!(range.contains(u64::MAX));
        let bounded = Range {
            min: 5,
            max: Some(10),
        };
        assert!(!bounded.contains(4));
        assert!(bounded.contains(5));
        assert!(bounded.contains(10));
        assert!(!bounded.contains(11));
    }
}
