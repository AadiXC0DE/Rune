//! Token accounting and context budgets.
//!
//! The estimate drives the compaction trigger. It is the provider's reported
//! input for the latest request, plus four bytes per token for whatever was
//! added since; with no report yet it is four bytes per token for the whole
//! request.

use rune_core::budget::{Budget, BudgetSet, LimitName};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_net::message::ContentPart;
use rune_net::provider::{Provider, RequestPlan};
use serde::{Deserialize, Serialize};

/// Divisor applied to a byte count to estimate tokens.
///
/// Four bytes per token is the long-standing working figure for English text and
/// source code in the byte-pair vocabularies the current models use. It applies
/// to whatever the provider has not yet counted.
pub const DEFAULT_BYTES_PER_TOKEN: u64 = 4;

/// Tokens charged for one image, whatever its encoded size.
pub const IMAGE_TOKENS: u64 = 1_500;

/// The size of one built request, measured the way the estimate needs it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Measure {
    /// Serialized body bytes, minus the base64 bytes of attached images.
    pub text_bytes: u64,
    /// Image parts in the request.
    pub images: u64,
}

/// Reported input of one completed request, kept to anchor the next estimate.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Anchor {
    /// Dialect name (`Provider::name`) and model the request went to.
    pub dialect: String,
    pub model: String,
    /// Input tokens the provider reported, cached parts included.
    pub input_tokens: u64,
    /// The measure of that request.
    pub measure: Measure,
}

/// Measures a plan as the dialect would send it.
pub fn measure(dialect: &dyn Provider, plan: &RequestPlan) -> Result<Measure> {
    let body = dialect.build_request(plan)?;
    let bytes = serde_json::to_vec(&body)
        .map_err(|err| RuneError::new(ErrorCode::Internal, err.to_string()))?
        .len() as u64;
    Ok(measure_body(bytes, plan))
}

/// Measures a plan whose serialized body is `body_bytes` long.
pub(crate) fn measure_body(body_bytes: u64, plan: &RequestPlan) -> Measure {
    let mut measure = Measure {
        text_bytes: body_bytes,
        images: 0,
    };
    for part in plan.messages.iter().flat_map(|message| &message.parts) {
        if let ContentPart::Image { image } = part {
            measure.images = measure.images.saturating_add(1);
            if let Some(data) = plan.image_data.get(&image.id) {
                measure.text_bytes = measure.text_bytes.saturating_sub(data.len() as u64);
            }
        }
    }
    measure
}

/// Estimates the input tokens of a request.
/// Returns the estimate and whether it was anchored.
#[must_use]
pub fn estimate_input(
    now: Measure,
    anchor: Option<&Anchor>,
    dialect: &str,
    model: &str,
) -> (u64, bool) {
    if let Some(anchor) = anchor
        && anchor.dialect == dialect
        && anchor.model == model
        && now.text_bytes >= anchor.measure.text_bytes
        && now.images >= anchor.measure.images
    {
        let appended = estimate_tokens(now.text_bytes.saturating_sub(anchor.measure.text_bytes));
        let images = now
            .images
            .saturating_sub(anchor.measure.images)
            .saturating_mul(IMAGE_TOKENS);
        return (
            anchor
                .input_tokens
                .saturating_add(appended)
                .saturating_add(images),
            true,
        );
    }
    (
        estimate_tokens(now.text_bytes).saturating_add(now.images.saturating_mul(IMAGE_TOKENS)),
        false,
    )
}

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
    /// Whether the estimate was anchored on provider-reported usage.
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

/// Estimates tokens from a byte count, rounding up.
#[must_use]
pub fn estimate_tokens(bytes: u64) -> u64 {
    bytes.div_ceil(DEFAULT_BYTES_PER_TOKEN)
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

/// Percentage points below the ceiling at which the caller is warned.
const APPROACHING_BAND_PERCENT: u64 = 10;

/// Share of the window held back, and its floor for windows of 32,000 tokens or more.
pub const RESERVE_PERCENT: u64 = 15;
pub const RESERVE_FLOOR: u64 = 16_000;

/// Smallest window the reserve floor applies to.
const RESERVE_FLOOR_FROM: u64 = 32_000;

/// The window the ceiling is computed from: the model's window, capped by
/// `context_cap_tokens` unless that is zero.
#[must_use]
pub fn effective_window(window: u64, limits: &BudgetSet) -> u64 {
    match limits.get(LimitName::ContextCapTokens) {
        Budget::Bounded(cap) if cap > 0 => window.min(cap),
        _ => window,
    }
}

/// The input-token level at which compaction is required, or `None` for an unknown window.
///
/// An explicitly configured `compaction_trigger_percent` replaces the reserve.
#[must_use]
pub fn ceiling(window: u64, limits: &BudgetSet) -> Option<u64> {
    if window == 0 {
        return None;
    }
    let effective = effective_window(window, limits);
    if limits.source(LimitName::CompactionTriggerPercent).is_some() {
        return Some(effective.saturating_mul(u64::from(trigger_percent(limits))) / 100);
    }
    let floor = if effective >= RESERVE_FLOOR_FROM {
        RESERVE_FLOOR
    } else {
        0
    };
    let reserve = (effective.saturating_mul(RESERVE_PERCENT) / 100).max(floor);
    Some(effective.saturating_sub(reserve))
}

/// The percent of usable input at which a request is compacted.
#[must_use]
pub fn trigger_percent(limits: &BudgetSet) -> u8 {
    u8::try_from(
        limits
            .get_usize(LimitName::CompactionTriggerPercent)
            .min(99),
    )
    .unwrap_or(80)
}

/// Decides what to do about the current capacity.
///
/// Compaction is required once the input reaches `ceiling` (see [`ceiling`]),
/// or when the input and the output reserve together exceed the capacity. The
/// approaching band sits ten percentage points of capacity below the ceiling,
/// which gives the caller one request of warning before compaction is required.
/// `estimate` measures the full request; `retained_estimate` measures the request
/// after removing the planned prefix, including any fixed request overhead. If
/// no prefix can be removed, pass the full estimate as the retained estimate.
#[must_use]
pub fn decide_capacity(
    estimate: &Estimate,
    retained_estimate: &Estimate,
    ceiling: u64,
) -> CapacityDecision {
    let band = estimate.capacity.saturating_mul(APPROACHING_BAND_PERCENT) / 100;

    if retained_estimate.total_tokens() > estimate.capacity {
        // Only the retained request proves that removing the prefix cannot help.
        CapacityDecision::OverCapacity
    } else if estimate.total_tokens() > estimate.capacity || estimate.input_tokens >= ceiling {
        CapacityDecision::Compact
    } else if estimate.input_tokens.saturating_add(band) >= ceiling {
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

    fn anchor(input_tokens: u64, text_bytes: u64) -> Anchor {
        Anchor {
            dialect: "chat_completions".to_owned(),
            model: "model-a".to_owned(),
            input_tokens,
            measure: Measure {
                text_bytes,
                images: 0,
            },
        }
    }

    #[test]
    fn estimates_round_up() {
        assert_eq!(estimate_tokens(5), 2);
        assert_eq!(estimate_tokens(0), 0);
    }

    #[test]
    fn an_anchor_adds_the_bytes_appended_since_it() {
        let anchor = anchor(1_000, 4_000);
        let now = Measure {
            text_bytes: 4_401,
            images: 0,
        };
        assert_eq!(
            estimate_input(now, Some(&anchor), "chat_completions", "model-a"),
            (1_101, true)
        );
    }

    #[test]
    fn an_anchor_from_another_model_is_ignored() {
        let anchor = anchor(1_000, 4_000);
        let now = Measure {
            text_bytes: 4_401,
            images: 0,
        };
        assert_eq!(
            estimate_input(now, Some(&anchor), "chat_completions", "model-b"),
            (1_101, false)
        );
        assert_eq!(
            estimate_input(now, Some(&anchor), "anthropic", "model-a"),
            (1_101, false)
        );
    }

    #[test]
    fn a_request_smaller_than_its_anchor_is_estimated_from_bytes() {
        let anchor = anchor(5_000, 4_000);
        let now = Measure {
            text_bytes: 3_999,
            images: 0,
        };
        assert_eq!(
            estimate_input(now, Some(&anchor), "chat_completions", "model-a"),
            (1_000, false)
        );
    }

    #[test]
    fn images_count_a_fixed_amount_whatever_their_size() {
        let provider = rune_net::chat_completions::ChatCompletions;
        let mut plan = RequestPlan::new("model-a");
        plan.messages = vec![rune_net::message::Message::user_parts(vec![
            ContentPart::Text {
                text: "what is in this picture".to_owned(),
            },
            ContentPart::Image {
                image: rune_net::message::ImageRef {
                    id: 7,
                    media_type: "image/png".to_owned(),
                    encoded_bytes: 300_000,
                },
            },
        ])];
        plan.image_data.insert(7, "A".repeat(400_000).into());
        let measured = measure(&provider, &plan).expect("measure");
        assert_eq!(measured.images, 1);
        assert!(measured.text_bytes < 2_000, "{measured:?}");
        let (tokens, anchored) = estimate_input(measured, None, provider.name(), "model-a");
        assert!(!anchored);
        assert!(tokens < IMAGE_TOKENS + 500, "{tokens}");
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
        let ceiling = 800;
        let estimate = Estimate::new(0, 10, 1000);
        assert_eq!(
            decide_capacity(&estimate, &estimate, ceiling),
            CapacityDecision::Fits
        );
    }

    #[test]
    fn capacity_near_the_trigger_warns_before_compacting() {
        let ceiling = 800;
        // Seventy-five percent of capacity is within ten points of a ceiling at
        // eighty, so it is inside the approaching band and still sendable.
        let estimate = Estimate::new(0, 750, 1000);
        assert_eq!(
            decide_capacity(&estimate, &estimate, ceiling),
            CapacityDecision::Approaching
        );
    }

    #[test]
    fn capacity_at_the_trigger_requires_compaction() {
        let ceiling = 800;
        let estimate = Estimate::new(0, 800, 1000);
        assert_eq!(
            decide_capacity(&estimate, &estimate, ceiling),
            CapacityDecision::Compact
        );
    }

    #[test]
    fn capacity_over_the_total_is_reported_as_such() {
        let ceiling = 800;
        let estimate = Estimate::new(0, 1200, 1000);
        assert_eq!(
            decide_capacity(&estimate, &estimate, ceiling),
            CapacityDecision::OverCapacity
        );
    }

    #[test]
    fn full_history_over_capacity_is_compactable_when_the_retained_request_fits() {
        let ceiling = 800;
        let full = Estimate::new(8000, 2000, 1000);
        let retained = Estimate::new(400, 100, 1000);
        assert_eq!(
            decide_capacity(&full, &retained, ceiling),
            CapacityDecision::Compact
        );
    }

    #[test]
    fn retained_capacity_includes_the_output_reserve_and_allows_an_exact_fit() {
        let ceiling = 800;
        let full = Estimate::new(8000, 2000, 1000).with_output_reserve(100);
        let retained = Estimate::new(3600, 900, 1000).with_output_reserve(100);
        assert_eq!(
            decide_capacity(&full, &retained, ceiling),
            CapacityDecision::Compact
        );
        let too_large = retained.with_output_reserve(101);
        assert_eq!(
            decide_capacity(&full, &too_large, ceiling),
            CapacityDecision::OverCapacity
        );
    }

    #[test]
    fn the_trigger_can_be_tuned_without_a_rebuild() {
        let mut limits = BudgetSet::new();
        limits
            .set(
                LimitName::CompactionTriggerPercent,
                Budget::Bounded(50),
                rune_core::config::Layer::User,
            )
            .expect("set");
        let estimate = Estimate::new(0, 600, 1000);
        let ceiling = ceiling(1000, &limits).expect("ceiling");
        assert_eq!(
            decide_capacity(&estimate, &estimate, ceiling),
            CapacityDecision::Compact
        );
    }

    /// A limit set with one limit set in the user file.
    fn with_limit(name: LimitName, value: u64) -> BudgetSet {
        let mut limits = BudgetSet::new();
        limits
            .set(name, Budget::Bounded(value), rune_core::config::Layer::User)
            .expect("set");
        limits
    }

    #[test]
    fn the_ceiling_reserves_fifteen_percent_or_sixteen_thousand() {
        let limits = BudgetSet::new();
        assert_eq!(ceiling(200_000, &limits), Some(170_000));
        assert_eq!(ceiling(128_000, &limits), Some(108_800));
        assert_eq!(ceiling(16_000, &limits), Some(13_600));
        assert_eq!(ceiling(1_000_000, &limits), Some(217_600));
        let uncapped = with_limit(LimitName::ContextCapTokens, 0);
        assert_eq!(ceiling(1_000_000, &uncapped), Some(850_000));
        assert_eq!(effective_window(1_000_000, &limits), 256_000);
        assert_eq!(effective_window(1_000_000, &uncapped), 1_000_000);
    }

    #[test]
    fn an_explicit_percent_replaces_the_reserve() {
        let limits = with_limit(LimitName::CompactionTriggerPercent, 80);
        assert_eq!(ceiling(128_000, &limits), Some(102_400));
    }

    #[test]
    fn an_unknown_window_has_no_ceiling() {
        assert_eq!(ceiling(0, &BudgetSet::new()), None);
        let limits = with_limit(LimitName::CompactionTriggerPercent, 80);
        assert_eq!(ceiling(0, &limits), None);
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
