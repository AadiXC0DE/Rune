//! Token accounting and context budgets.
//!
//! The estimator drives the compaction trigger, so its accuracy matters: too
//! coarse and compaction fires late and the request is rejected, too eager and
//! the session summarizes constantly. It is a byte-derived heuristic calibrated
//! per model from observed usage, and the calibration is stored rather than
//! recomputed.

use rune_core::budget::{BudgetSet, LimitName};
use serde::{Deserialize, Serialize};

/// Divisor applied to a byte count to estimate tokens.
///
/// Four bytes per token is the long-standing working figure for English text and
/// source code in the byte-pair vocabularies the current models use. It is a
/// starting point that calibration replaces.
pub const DEFAULT_BYTES_PER_TOKEN: u64 = 4;

/// Plausible range for an observed bytes-per-token ratio.
///
/// An observation outside this range is not a measurement of the tokenizer: it
/// is a degenerate report, such as a byte count with a near-zero token count.
/// Folding one in would corrupt the estimate for every later decision, so it is
/// rejected rather than clamped.
const MIN_PLAUSIBLE_BYTES_PER_TOKEN: f64 = 1.0;
const MAX_PLAUSIBLE_BYTES_PER_TOKEN: f64 = 16.0;

/// An estimate of the tokens a request will occupy.
///
/// Carries the capacity it is measured against, so a decision about what to do
/// is a method on the estimate rather than a function taking two loosely related
/// numbers. That removes a way for the caller to measure one capacity and gate
/// on another.
#[derive(Clone, Copy, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct Estimate {
    /// Serialized request size in bytes.
    pub serialized_bytes: u64,
    /// Estimated input tokens.
    pub input_tokens: u64,
    /// Output tokens reserved for the response.
    pub output_tokens: u64,
    /// Usable input capacity for this model.
    pub capacity: u64,
    /// Whether the estimate was calibrated against observed usage.
    pub calibrated: bool,
}

impl Estimate {
    /// Builds an estimate for a request.
    #[must_use]
    pub fn new(serialized_bytes: u64, input_tokens: u64, capacity: u64) -> Self {
        Self {
            serialized_bytes,
            input_tokens,
            output_tokens: 0,
            capacity,
            calibrated: false,
        }
    }

    /// Records the output reserve.
    #[must_use]
    pub const fn with_output_reserve(mut self, tokens: u64) -> Self {
        self.output_tokens = tokens;
        self
    }

    /// Returns the total the request occupies.
    #[must_use]
    pub const fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    /// Returns the fraction of the capacity the request occupies, as a percent.
    ///
    /// Saturates at one hundred so a request far over capacity reports as full
    /// rather than wrapping.
    #[must_use]
    pub fn used_percent(&self) -> u8 {
        if self.capacity == 0 {
            return 100;
        }
        let scaled = u128::from(self.total_tokens()).saturating_mul(100);
        let percent = scaled.checked_div(u128::from(self.capacity)).unwrap_or(0);
        u8::try_from(percent.min(100)).unwrap_or(100)
    }

    /// Returns the tokens still available.
    #[must_use]
    pub const fn remaining(&self) -> u64 {
        self.capacity.saturating_sub(self.total_tokens())
    }
}

/// Estimates tokens from a byte count.
///
/// The divisor is not a guess at the model's tokenizer: it is a stable working
/// figure that the calibration below corrects once a real response reports its
/// own counts.
#[must_use]
pub fn estimate_tokens(bytes: u64) -> u64 {
    bytes.saturating_div(DEFAULT_BYTES_PER_TOKEN)
}

/// Per-model calibration derived from observed usage.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub struct Calibration {
    /// Bytes per token observed for this model.
    pub bytes_per_token: f64,
    /// Requests this calibration was derived from.
    pub samples: u32,
}

impl Default for Calibration {
    fn default() -> Self {
        Self {
            bytes_per_token: DEFAULT_BYTES_PER_TOKEN as f64,
            samples: 0,
        }
    }
}

impl Calibration {
    /// Returns the divisor in use.
    fn divisor(&self) -> f64 {
        self.bytes_per_token
    }

    /// Folds one observed request into the calibration.
    ///
    /// Uses a running mean so a single unusual request moves the estimate a
    /// little rather than a lot. An observation outside the plausible range is
    /// rejected outright, because folding it in would corrupt every later
    /// decision rather than merely skewing one.
    pub fn observe(&mut self, serialized_bytes: u64, reported_input_tokens: u64) {
        if reported_input_tokens == 0 || serialized_bytes == 0 {
            return;
        }
        let observed = serialized_bytes as f64 / reported_input_tokens as f64;
        if !observed.is_finite()
            || observed < MIN_PLAUSIBLE_BYTES_PER_TOKEN
            || observed > MAX_PLAUSIBLE_BYTES_PER_TOKEN
        {
            return;
        }
        let samples = f64::from(self.samples);
        // Weight the new observation at one over the sample count, so the first
        // sample defines the value and each later one nudges it.
        let weight = 1.0 / (samples + 1.0);
        self.bytes_per_token = self
            .bytes_per_token
            .mul_add(1.0 - weight, observed * weight);
        self.samples = self.samples.saturating_add(1);
    }

    /// Estimates tokens with this calibration applied.
    #[must_use]
    pub fn estimate(&self, bytes: u64) -> u64 {
        let divisor = self.divisor();
        if !divisor.is_finite() || divisor < f64::EPSILON {
            return estimate_tokens(bytes);
        }
        let tokens = bytes as f64 / divisor;
        if !tokens.is_finite() || tokens < 0.0 {
            return estimate_tokens(bytes);
        }
        // A saturating cast: an absurdly large input clamps rather than wrapping.
        tokens.min(u64::MAX as f64) as u64
    }
}

/// The usable input capacity for a model.
///
/// Output reserve is subtracted because a request that fills the entire context
/// leaves no room for the response, which the endpoint rejects.
#[must_use]
pub fn usable_input_tokens(context_window: u64, max_output_tokens: u64) -> u64 {
    context_window.saturating_sub(max_output_tokens)
}

/// What to do about capacity.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityDecision {
    /// The request fits with room to spare.
    Fits,
    /// The request fits but is close to the trigger, so compaction should be
    /// prepared before the next request is built.
    Approaching,
    /// The request is over the trigger and must be compacted first.
    Compact,
    /// The request cannot fit even after compaction, which means the retained
    /// tail is itself too large.
    OverCapacity,
}

impl CapacityDecision {
    /// Returns the wire representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fits => "fits",
            Self::Approaching => "approaching",
            Self::Compact => "compact",
            Self::OverCapacity => "over_capacity",
        }
    }

    /// Returns true when the request may be sent without compaction.
    #[must_use]
    pub const fn may_send(self) -> bool {
        matches!(self, Self::Fits | Self::Approaching)
    }
}

/// Percentage points below the trigger at which the caller is warned.
const APPROACHING_BAND_PERCENT: u8 = 10;

/// Decides what to do about the current capacity.
///
/// The trigger is a percentage of usable input, taken from the limits so it can
/// be tuned without a rebuild. The approaching band sits ten points below it,
/// which gives the caller one request of warning before compaction is required.
#[must_use]
pub fn decide_capacity(estimate: &Estimate, limits: &BudgetSet) -> CapacityDecision {
    let trigger = u8::try_from(
        limits
            .get_usize(LimitName::CompactionTriggerPercent)
            .min(99),
    )
    .unwrap_or(80);
    let used = estimate.used_percent();

    if estimate.total_tokens() > estimate.capacity {
        // Over capacity even before the trigger, which means the retained tail
        // alone does not fit and a summary will not rescue it.
        CapacityDecision::OverCapacity
    } else if used >= trigger {
        CapacityDecision::Compact
    } else if used.saturating_add(APPROACHING_BAND_PERCENT) >= trigger {
        CapacityDecision::Approaching
    } else {
        CapacityDecision::Fits
    }
}

/// A running record of usage for one session.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct UsageTotals {
    /// Input tokens reported.
    pub input_tokens: Option<u64>,
    /// Output tokens reported.
    pub output_tokens: Option<u64>,
    /// Cache read tokens reported.
    pub cache_read_tokens: Option<u64>,
    /// Cache write tokens reported.
    pub cache_write_tokens: Option<u64>,
    /// Reasoning tokens reported.
    pub reasoning_tokens: Option<u64>,
    /// Requests that contributed.
    pub requests: u64,
}

impl UsageTotals {
    /// Folds one report into the totals.
    ///
    /// A count the provider did not report leaves the running total unchanged
    /// rather than resetting it, and a reported zero is added as zero. The
    /// difference between absent and zero is preserved exactly.
    pub fn observe(&mut self, usage: &rune_net::stream::Usage) {
        self.input_tokens = add(self.input_tokens, usage.input_tokens);
        self.output_tokens = add(self.output_tokens, usage.output_tokens);
        self.cache_read_tokens = add(self.cache_read_tokens, usage.cache_read_tokens);
        self.cache_write_tokens = add(self.cache_write_tokens, usage.cache_write_tokens);
        self.reasoning_tokens = add(self.reasoning_tokens, usage.reasoning_tokens);
        self.requests = self.requests.saturating_add(1);
    }

    /// Returns the total tokens across every reported category.
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens
            .unwrap_or(0)
            .saturating_add(self.output_tokens.unwrap_or(0))
    }

    /// Returns true when no report has been folded in.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.requests == 0
    }
}

/// Adds a reported count to a running total, preserving absence.
fn add(total: Option<u64>, reported: Option<u64>) -> Option<u64> {
    match (total, reported) {
        (Some(total), Some(reported)) => Some(total.saturating_add(reported)),
        (Some(total), None) => Some(total),
        (None, Some(reported)) => Some(reported),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_estimator_divides_bytes_by_four() {
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(4), 1);
        assert_eq!(estimate_tokens(400), 100);
        assert_eq!(estimate_tokens(3), 0);
    }

    #[test]
    fn calibration_starts_at_the_default() {
        let calibration = Calibration::default();
        assert!(
            (calibration.bytes_per_token - DEFAULT_BYTES_PER_TOKEN as f64).abs() < f64::EPSILON
        );
        assert_eq!(calibration.samples, 0);
    }

    #[test]
    fn calibration_moves_toward_the_first_observation() {
        let mut calibration = Calibration::default();
        // Eight bytes per token observed, so the estimate should rise.
        calibration.observe(800, 100);
        assert!(calibration.bytes_per_token > 4.0);
        assert_eq!(calibration.samples, 1);
        assert_eq!(calibration.estimate(800), 100);
    }

    #[test]
    fn calibration_ignores_a_report_with_no_tokens() {
        let mut calibration = Calibration::default();
        calibration.observe(800, 0);
        calibration.observe(0, 100);
        assert_eq!(calibration.samples, 0);
        assert!(
            (calibration.bytes_per_token - DEFAULT_BYTES_PER_TOKEN as f64).abs() < f64::EPSILON
        );
    }

    #[test]
    fn one_outlier_moves_the_calibration_only_a_little() {
        let mut calibration = Calibration::default();
        for _ in 0..20 {
            calibration.observe(400, 100);
        }
        let settled = calibration.bytes_per_token;
        calibration.observe(100_000, 10);
        let moved = (calibration.bytes_per_token - settled).abs();
        assert!(moved < settled, "one outlier dominated the calibration");
    }

    #[test]
    fn an_implausible_observation_is_rejected_rather_than_clamped() {
        let mut calibration = Calibration::default();
        // A million bytes claiming one token is a degenerate report, not a
        // measurement, so it must not move the calibration at all.
        calibration.observe(1_000_000, 1);
        assert_eq!(calibration.samples, 0);
        assert!(
            (calibration.bytes_per_token - DEFAULT_BYTES_PER_TOKEN as f64).abs() < f64::EPSILON
        );

        // The same applies at the other extreme.
        let mut low = Calibration::default();
        low.observe(1, 1000);
        assert_eq!(low.samples, 0);
    }

    #[test]
    fn calibrated_estimation_is_bounded() {
        let calibration = Calibration::default();
        assert_eq!(calibration.estimate(0), 0);
        assert!(calibration.estimate(u64::MAX) > 0);
    }

    #[test]
    fn usable_input_subtracts_the_output_reserve() {
        assert_eq!(usable_input_tokens(200_000, 8192), 191_808);
    }

    #[test]
    fn usable_input_never_underflows() {
        assert_eq!(usable_input_tokens(1000, 8192), 0);
        assert_eq!(usable_input_tokens(0, 0), 0);
    }

    #[test]
    fn a_percentage_is_computed_against_capacity() {
        let estimate = Estimate::new(400, 50, 1000).with_output_reserve(50);
        assert_eq!(estimate.used_percent(), 10);
        let full = Estimate::new(400, 50, 100).with_output_reserve(50);
        assert_eq!(full.used_percent(), 100);
    }

    #[test]
    fn a_percentage_saturates_rather_than_wrapping() {
        let estimate = Estimate::new(0, u64::MAX, 100);
        assert_eq!(estimate.used_percent(), 100);
    }

    #[test]
    fn a_zero_capacity_reports_full() {
        let estimate = Estimate::default();
        assert_eq!(estimate.used_percent(), 100);
    }

    #[test]
    fn remaining_capacity_never_underflows() {
        let estimate = Estimate::new(0, 10, 100);
        assert_eq!(estimate.remaining(), 90);
        let over = Estimate::new(0, 500, 100);
        assert_eq!(over.remaining(), 0);
    }

    #[test]
    fn capacity_below_the_trigger_fits() {
        let limits = BudgetSet::new();
        let estimate = Estimate::new(0, 10, 1000);
        assert_eq!(decide_capacity(&estimate, &limits), CapacityDecision::Fits);
    }

    #[test]
    fn capacity_near_the_trigger_warns_before_compacting() {
        let limits = BudgetSet::new();
        // The default trigger is eighty percent, so seventy-five is inside the
        // approaching band and still sendable.
        let estimate = Estimate::new(0, 750, 1000);
        assert_eq!(
            decide_capacity(&estimate, &limits),
            CapacityDecision::Approaching
        );
    }

    #[test]
    fn capacity_at_the_trigger_requires_compaction() {
        let limits = BudgetSet::new();
        let estimate = Estimate::new(0, 800, 1000);
        assert_eq!(
            decide_capacity(&estimate, &limits),
            CapacityDecision::Compact
        );
    }

    #[test]
    fn capacity_over_the_total_is_reported_as_such() {
        let limits = BudgetSet::new();
        let estimate = Estimate::new(0, 1200, 1000);
        assert_eq!(
            decide_capacity(&estimate, &limits),
            CapacityDecision::OverCapacity
        );
    }

    #[test]
    fn the_trigger_can_be_tuned_without_a_rebuild() {
        let mut limits = BudgetSet::new();
        limits
            .set(
                LimitName::CompactionTriggerPercent,
                rune_core::budget::Budget::Bounded(50),
                rune_core::config::Layer::User,
            )
            .expect("set");
        let estimate = Estimate::new(0, 600, 1000);
        assert_eq!(
            decide_capacity(&estimate, &limits),
            CapacityDecision::Compact
        );
    }

    #[test]
    fn capacity_decisions_have_distinct_names() {
        let all = [
            CapacityDecision::Fits,
            CapacityDecision::Approaching,
            CapacityDecision::Compact,
            CapacityDecision::OverCapacity,
        ];
        let mut seen = std::collections::HashSet::new();
        for decision in all {
            assert!(seen.insert(decision.as_str()), "duplicate {decision:?}");
        }
    }

    #[test]
    fn only_fitting_and_approaching_may_send_without_compaction() {
        assert!(CapacityDecision::Fits.may_send());
        assert!(CapacityDecision::Approaching.may_send());
        assert!(!CapacityDecision::Compact.may_send());
        assert!(!CapacityDecision::OverCapacity.may_send());
    }

    #[test]
    fn usage_totals_accumulate_reported_counts() {
        let mut totals = UsageTotals::default();
        assert!(totals.is_empty());

        totals.observe(&rune_net::stream::Usage {
            input_tokens: Some(100),
            output_tokens: Some(20),
            ..rune_net::stream::Usage::default()
        });
        totals.observe(&rune_net::stream::Usage {
            input_tokens: Some(50),
            output_tokens: Some(10),
            ..rune_net::stream::Usage::default()
        });

        assert_eq!(totals.input_tokens, Some(150));
        assert_eq!(totals.output_tokens, Some(30));
        assert_eq!(totals.requests, 2);
        assert_eq!(totals.total_tokens(), 180);
    }

    #[test]
    fn an_unreported_count_stays_absent_across_reports() {
        let mut totals = UsageTotals::default();
        totals.observe(&rune_net::stream::Usage {
            input_tokens: Some(10),
            ..rune_net::stream::Usage::default()
        });
        totals.observe(&rune_net::stream::Usage {
            input_tokens: Some(10),
            ..rune_net::stream::Usage::default()
        });
        assert_eq!(totals.output_tokens, None);
        assert_eq!(totals.reasoning_tokens, None);
    }

    #[test]
    fn a_reported_zero_is_accumulated_as_zero() {
        let mut totals = UsageTotals::default();
        totals.observe(&rune_net::stream::Usage {
            input_tokens: Some(0),
            ..rune_net::stream::Usage::default()
        });
        // The distinction between absent and zero is a contract, so a reported
        // zero must not become absent.
        assert_eq!(totals.input_tokens, Some(0));
    }

    #[test]
    fn a_count_first_reported_late_is_still_captured() {
        let mut totals = UsageTotals::default();
        totals.observe(&rune_net::stream::Usage::default());
        assert_eq!(totals.cache_read_tokens, None);
        totals.observe(&rune_net::stream::Usage {
            cache_read_tokens: Some(5),
            ..rune_net::stream::Usage::default()
        });
        assert_eq!(totals.cache_read_tokens, Some(5));
    }

    #[test]
    fn usage_totals_round_trip_through_json() {
        let mut totals = UsageTotals::default();
        totals.observe(&rune_net::stream::Usage {
            input_tokens: Some(1),
            ..rune_net::stream::Usage::default()
        });
        let json = serde_json::to_string(&totals).expect("serialize");
        let parsed: UsageTotals = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, totals);
    }
}
