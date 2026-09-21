//! Context limit resolution.
//!
//! A limit is data: a compiled default, an accepted range, and the layer that
//! supplied the value in force. [`Limits`] checks the whole set once, so two
//! consumers cannot read different values for the same limit and a value outside
//! its accepted range is refused rather than clamped: a bound nobody believes is
//! worse than a visible error.
//!
//! A limit set to `off` stays `off` in a report and still resolves to the
//! emergency ceiling, so no setting can make a path unbounded.

use std::collections::BTreeMap;

use serde::Serialize;

use rune_core::budget::{Budget, BudgetSet, EMERGENCY_CEILING_BYTES, LimitName, Unit};
use rune_core::config::Layer;
use rune_core::error::{Result, RuneError};

/// One limit as reported by `limits --json`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
pub struct LimitRow {
    /// Limit name, also the configuration key.
    pub name: LimitName,
    /// Value as configured. `off` is reported as `off` rather than as the
    /// ceiling, so a reader can tell an unbounded setting from a large one.
    pub value: Budget,
    /// Value to use as a length, with the ceiling substituted for `off`.
    pub effective: usize,
    /// Compiled default, for comparison.
    pub default: Budget,
    /// Unit the value is expressed in.
    pub unit: Unit,
    /// Layer that supplied the value, absent when it is the default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Layer>,
}

/// The limits in force, checked once and then read without revalidating.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Limits {
    values: BTreeMap<LimitName, Budget>,
    sources: BTreeMap<LimitName, Layer>,
}

impl Limits {
    /// Resolves every limit, refusing a value outside its accepted range.
    ///
    /// The range is checked here because this is the point of use. A set loaded
    /// from a file never went through [`BudgetSet::set`], which is where the
    /// check normally happens, so a hand-edited value reaches a consumer only
    /// through this function, and it stops there with the limit, the value, and
    /// the range named.
    pub fn resolve(set: &BudgetSet) -> Result<Self> {
        let mut values = BTreeMap::new();
        let mut sources = BTreeMap::new();
        for name in LimitName::all().iter().copied() {
            let value = set.get(name);
            check(name, value)?;
            if let Some(layer) = set.source(name) {
                sources.insert(name, layer);
            }
            values.insert(name, value);
        }
        Ok(Self { values, sources })
    }

    /// Returns the value as configured, with `off` still `off`.
    #[must_use]
    pub fn get(&self, name: LimitName) -> Budget {
        self.values
            .get(&name)
            .copied()
            .unwrap_or_else(|| name.default_value())
    }

    /// Returns the value as a byte count, with the ceiling substituted for `off`.
    #[must_use]
    pub fn get_bytes(&self, name: LimitName) -> u64 {
        self.get(name).effective(EMERGENCY_CEILING_BYTES)
    }

    /// Returns the layer that supplied a value, or `None` when it is defaulted.
    #[must_use]
    pub fn source(&self, name: LimitName) -> Option<Layer> {
        self.sources.get(&name).copied()
    }
}

/// Refuses a configured value that falls outside its accepted range.
fn check(name: LimitName, value: Budget) -> Result<()> {
    let range = name.range();
    let Some(raw) = value.value().filter(|raw| !range.contains(*raw)) else {
        return Ok(());
    };
    Err(RuneError::invalid_field(
        name.as_str(),
        format!(
            "`{}` is set to {raw}, outside the accepted range {}..{}",
            name.as_str(),
            range.min,
            range
                .max
                .map_or_else(|| "unbounded".to_owned(), |max| max.to_string())
        ),
    )
    .with_hint(format!(
        "set `{}` inside its range, or to `off`",
        name.as_str()
    )))
}

/// Returns the value to use for a limit as a length.
///
/// A limit set to `off` resolves to the emergency ceiling here, which is what
/// keeps an unbounded setting from turning into an unbounded read.
#[must_use]
pub fn effective(name: LimitName, limits: &Limits) -> usize {
    usize::try_from(limits.get_bytes(name)).unwrap_or(usize::MAX)
}

/// Returns a limit's compiled default as a length.
///
/// Used where a caller holds no configured set, such as a discovery scan. The
/// ceiling substitution happens here and in [`effective`], nowhere else.
#[must_use]
pub fn default_limit(name: LimitName) -> usize {
    usize::try_from(name.default_value().effective(EMERGENCY_CEILING_BYTES)).unwrap_or(usize::MAX)
}

/// Returns every limit with its value, ceiling, unit, and source.
///
/// The order is the order of [`LimitName::all`], so two reports of the same set
/// are identical.
#[must_use]
pub fn describe(limits: &Limits) -> Vec<LimitRow> {
    LimitName::all()
        .iter()
        .copied()
        .map(|name| LimitRow {
            name,
            value: limits.get(name),
            effective: effective(name, limits),
            default: name.default_value(),
            unit: name.unit(),
            source: limits.source(name),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune_core::error::ErrorCode;

    /// Returns the ceiling as a length.
    fn ceiling() -> usize {
        usize::try_from(EMERGENCY_CEILING_BYTES).expect("ceiling fits")
    }

    /// Builds a set with one override.
    fn set(name: LimitName, value: Budget) -> BudgetSet {
        let mut set = BudgetSet::new();
        set.set(name, value, Layer::User).expect("set");
        set
    }

    /// Returns the reported row for a limit.
    fn row(limits: &Limits, name: LimitName) -> LimitRow {
        describe(limits)
            .into_iter()
            .find(|row| row.name == name)
            .expect("row")
    }

    #[test]
    fn an_off_limit_is_reported_as_off_and_bounded_by_the_ceiling() {
        let limits =
            Limits::resolve(&set(LimitName::SkillFileBytes, Budget::Unbounded)).expect("resolve");

        assert_eq!(effective(LimitName::SkillFileBytes, &limits), ceiling());
        assert_eq!(
            limits.get_bytes(LimitName::SkillFileBytes),
            EMERGENCY_CEILING_BYTES
        );

        let reported = row(&limits, LimitName::SkillFileBytes);
        assert_eq!(
            reported.value,
            Budget::Unbounded,
            "`off` stays visible as `off`"
        );
        assert_eq!(reported.effective, ceiling());
        assert_eq!(reported.source, Some(Layer::User));
        assert_ne!(reported.effective, 0);

        let json = serde_json::to_value(reported).expect("serialize");
        assert_eq!(json["effective"].as_u64(), Some(EMERGENCY_CEILING_BYTES));
        assert_eq!(json["source"], "user");
    }

    #[test]
    fn an_override_is_resolved_with_its_layer() {
        let limits = Limits::resolve(&set(LimitName::SkillCatalogBytes, Budget::Bounded(4096)))
            .expect("resolve");

        assert_eq!(
            limits.get(LimitName::SkillCatalogBytes),
            Budget::Bounded(4096)
        );
        assert_eq!(effective(LimitName::SkillCatalogBytes, &limits), 4096);
        assert_eq!(
            limits.source(LimitName::SkillCatalogBytes),
            Some(Layer::User)
        );
        assert_eq!(limits.source(LimitName::SkillDescriptionBytes), None);
        assert_eq!(
            limits.get(LimitName::SkillDescriptionBytes),
            LimitName::SkillDescriptionBytes.default_value()
        );
    }

    #[test]
    fn a_value_outside_its_range_is_refused_rather_than_clamped() {
        // `set` refuses this value, so the only way it reaches a consumer is
        // through a document deserialized directly into the set.
        let raw = r#"{"overrides":{"compaction_trigger_percent":[5,"user"]}}"#;
        let corrupted: BudgetSet = serde_json::from_str(raw).expect("deserialize");

        let err = Limits::resolve(&corrupted).expect_err("outside its range");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(
            err.field(),
            Some(LimitName::CompactionTriggerPercent.as_str())
        );
        assert!(err.message().contains("10..99"), "{}", err.message());
        assert!(err.detail().hint.is_some());
    }

    #[test]
    fn a_value_above_its_range_is_refused() {
        let raw = r#"{"overrides":{"parallel_tool_calls":[1000,"user"]}}"#;
        let corrupted: BudgetSet = serde_json::from_str(raw).expect("deserialize");

        let err = Limits::resolve(&corrupted).expect_err("outside its range");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert!(err.message().contains("1..64"), "{}", err.message());
    }

    #[test]
    fn a_value_inside_its_range_resolves() {
        let mut set = BudgetSet::new();
        set.set(
            LimitName::CompactionTriggerPercent,
            Budget::Bounded(50),
            Layer::Project,
        )
        .expect("set");
        let limits = Limits::resolve(&set).expect("resolve");

        assert_eq!(
            limits.get(LimitName::CompactionTriggerPercent),
            Budget::Bounded(50)
        );
        assert_eq!(
            limits.source(LimitName::CompactionTriggerPercent),
            Some(Layer::Project)
        );
    }

    #[test]
    fn a_limit_at_its_lower_bound_resolves() {
        let limits = Limits::resolve(&set(
            LimitName::CompactionTriggerPercent,
            Budget::Bounded(10),
        ))
        .expect("resolve");
        assert_eq!(effective(LimitName::CompactionTriggerPercent, &limits), 10);
    }

    #[test]
    fn every_limit_appears_in_the_report_in_a_stable_order() {
        let limits = Limits::resolve(&BudgetSet::new()).expect("resolve");
        let rows = describe(&limits);

        assert_eq!(
            rows.iter().map(|row| row.name).collect::<Vec<_>>(),
            LimitName::all().to_vec()
        );
        assert!(rows.iter().all(|row| row.source.is_none()));
        assert!(rows.iter().all(|row| row.value == row.default));
        assert!(rows.iter().all(|row| {
            row.effective
                == usize::try_from(row.value.effective(EMERGENCY_CEILING_BYTES))
                    .unwrap_or(usize::MAX)
        }));
    }
}
