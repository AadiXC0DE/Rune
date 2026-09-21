//! Querying and rendering the usage ledger.
//!
//! A report states what the ledger holds and, just as importantly, what it does
//! not. Two properties keep it honest:
//!
//! - A cost that was never reported renders as absent, never as zero. Zero is a
//!   price; absence is the lack of one.
//! - A period the ledger only partly covers says so. Records are retained for a
//!   fixed window, so a thirty day report from a ledger that starts nine days
//!   ago is not a quiet thirty day total.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::str::FromStr;

use jiff::Timestamp;
use rune_core::error::{ErrorCode, RuneError};
use serde_json::{Map, Value, json};

use crate::usage::{HelperKind, UsageRecord};

/// Window a report covers.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Period {
    /// The last twenty-four hours.
    Last24Hours,
    /// The last seven days.
    Last7Days,
    /// The last thirty days.
    Last30Days,
}

impl Period {
    /// Every period, shortest first.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[Self::Last24Hours, Self::Last7Days, Self::Last30Days]
    }

    /// Returns the length of the window in milliseconds.
    #[must_use]
    pub const fn duration_ms(self) -> i64 {
        match self {
            Self::Last24Hours => 24 * 60 * 60 * 1_000,
            Self::Last7Days => 7 * 24 * 60 * 60 * 1_000,
            Self::Last30Days => 30 * 24 * 60 * 60 * 1_000,
        }
    }

    /// Returns the wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Last24Hours => "24h",
            Self::Last7Days => "7d",
            Self::Last30Days => "30d",
        }
    }
}

impl fmt::Display for Period {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Period {
    type Err = RuneError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::all()
            .iter()
            .copied()
            .find(|period| period.as_str() == raw.trim())
            .ok_or_else(|| {
                RuneError::new(
                    ErrorCode::InvalidField,
                    format!("`{raw}` is not a known period"),
                )
                .with_hint("accepted periods are 24h, 7d, and 30d")
            })
    }
}

/// Tokens summed for one measurement kind.
///
/// Each field is optional so an unreported count stays absent through the whole
/// path. Summing treats absence as no contribution, which is why the caller must
/// check whether any record reported the field at all before reading a total of
/// zero as a real zero.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct TokenTotals {
    /// Input tokens summed over the records that reported them.
    pub input_tokens: Option<u64>,
    /// Output tokens summed over the records that reported them.
    pub output_tokens: Option<u64>,
    /// Cache read tokens summed over the records that reported them.
    pub cache_read_tokens: Option<u64>,
    /// Cache write tokens summed over the records that reported them.
    pub cache_write_tokens: Option<u64>,
    /// Reasoning tokens summed over the records that reported them.
    pub reasoning_tokens: Option<u64>,
}

impl TokenTotals {
    /// Folds one record into the totals.
    fn add(&mut self, record: &UsageRecord) {
        self.input_tokens = sum(self.input_tokens, record.input_tokens);
        self.output_tokens = sum(self.output_tokens, record.output_tokens);
        self.cache_read_tokens = sum(self.cache_read_tokens, record.cache_read_tokens);
        self.cache_write_tokens = sum(self.cache_write_tokens, record.cache_write_tokens);
        self.reasoning_tokens = sum(self.reasoning_tokens, record.reasoning_tokens);
    }

    /// Returns true when no count was reported.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.input_tokens.is_none()
            && self.output_tokens.is_none()
            && self.cache_read_tokens.is_none()
            && self.cache_write_tokens.is_none()
            && self.reasoning_tokens.is_none()
    }
}

/// Totals for one model, or for one helper kind.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Breakdown {
    /// Requests accounted for.
    pub requests: u64,
    /// Tokens, per measurement kind.
    pub tokens: TokenTotals,
    /// Summed cost, as an exact decimal string.
    ///
    /// Absent when no record in this group reported a cost. Present, possibly as
    /// `"0"`, when at least one did.
    pub total_cost: Option<String>,
}

/// A report over a period.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Summary {
    /// Period the report covers.
    pub period: Period,
    /// Instant the report was produced at.
    pub now_ms: i64,
    /// Oldest instant included.
    pub since_ms: i64,
    /// Requests accounted for.
    pub requests: u64,
    /// Tokens across every request in the period.
    pub tokens: TokenTotals,
    /// Summed cost, absent when no request reported one.
    pub total_cost: Option<String>,
    /// Requests and tokens per helper kind, always listing every kind.
    pub by_helper: BTreeMap<HelperKind, Breakdown>,
    /// Requests and tokens per model, ordered by model name.
    pub by_model: BTreeMap<String, Breakdown>,
    /// Whether the period is only partly covered by the records available.
    pub coverage_is_partial: bool,
    /// Timestamp of the oldest record the ledger holds, at any age.
    ///
    /// Absent when the ledger holds no records. A caller compares it with
    /// [`Summary::since_ms`] to see how far back the records reach.
    pub first_record_at_ms: Option<i64>,
}

/// Summarizes records over a period.
///
/// `records` is normally the whole ledger, because records older than the period
/// are what decide whether coverage is partial. Records at or after `now_ms` are
/// excluded: a clock that moved backwards must not add future records to a past
/// window.
#[must_use]
pub fn summarize(records: &[UsageRecord], period: Period, now_ms: i64) -> Summary {
    let since_ms = now_ms.saturating_sub(period.duration_ms());
    let first_record_at_ms = records.iter().map(|record| record.created_at_ms).min();

    let mut summary = Summary {
        period,
        now_ms,
        since_ms,
        requests: 0,
        tokens: TokenTotals::default(),
        total_cost: None,
        by_helper: HelperKind::all()
            .iter()
            .copied()
            .map(|kind| (kind, Breakdown::default()))
            .collect(),
        by_model: BTreeMap::new(),
        coverage_is_partial: false,
        first_record_at_ms,
    };

    for record in records {
        if record.created_at_ms < since_ms || record.created_at_ms > now_ms {
            continue;
        }
        summary.requests = summary.requests.saturating_add(record.request_count);
        summary.tokens.add(record);
        summary.total_cost = add_cost(summary.total_cost.take(), record.total_cost.as_deref());

        // A record must appear in both breakdowns, so both are folded in one
        // pass. A missing helper kind cannot happen: every kind is pre-seeded.
        if let Some(group) = summary.by_helper.get_mut(&record.helper) {
            fold(group, record);
        }
        fold(
            summary.by_model.entry(record.model.clone()).or_default(),
            record,
        );
    }

    // The window starts before the oldest record the ledger has, so the report
    // cannot see everything the period covers.
    summary.coverage_is_partial = first_record_at_ms.is_some_and(|first| first > since_ms);
    summary
}

/// Renders a summary as JSON.
///
/// Absent counts and an absent cost are omitted rather than written as `null` or
/// zero, so a consumer reads absence as absence.
#[must_use]
pub fn to_json(summary: &Summary) -> Value {
    let mut object = Map::new();
    object.insert("period".to_owned(), json!(summary.period.as_str()));
    object.insert("since_ms".to_owned(), json!(summary.since_ms));
    object.insert("now_ms".to_owned(), json!(summary.now_ms));
    object.insert("requests".to_owned(), json!(summary.requests));
    object.insert("tokens".to_owned(), tokens_json(&summary.tokens));
    insert_cost(&mut object, summary.total_cost.as_deref());
    object.insert(
        "coverage_is_partial".to_owned(),
        json!(summary.coverage_is_partial),
    );
    object.insert(
        "first_record_at_ms".to_owned(),
        json!(summary.first_record_at_ms),
    );

    let by_helper: Map<String, Value> = summary
        .by_helper
        .iter()
        .map(|(kind, group)| (kind.as_str().to_owned(), breakdown_json(group)))
        .collect();
    object.insert("by_helper".to_owned(), Value::Object(by_helper));

    let by_model: Map<String, Value> = summary
        .by_model
        .iter()
        .map(|(model, group)| (model.clone(), breakdown_json(group)))
        .collect();
    object.insert("by_model".to_owned(), Value::Object(by_model));

    Value::Object(object)
}

/// Renders a summary as aligned text.
#[must_use]
pub fn render_text(summary: &Summary) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Usage for the last {}", summary.period);
    let _ = writeln!(out, "Requests: {}", summary.requests);
    let _ = writeln!(out, "Tokens:   {}", render_tokens(&summary.tokens));
    let _ = writeln!(
        out,
        "Cost:     {}",
        render_cost(summary.total_cost.as_deref())
    );

    if summary.coverage_is_partial {
        let _ = writeln!(
            out,
            "Coverage: partial, records start {}",
            render_instant(summary.first_record_at_ms)
        );
    } else {
        let _ = writeln!(out, "Coverage: full");
    }

    let _ = writeln!(out, "\nBy model:");
    if summary.by_model.is_empty() {
        let _ = writeln!(out, "  none");
    }
    for (model, group) in &summary.by_model {
        let _ = writeln!(
            out,
            "  {}: {} requests, {}, cost {}",
            model,
            group.requests,
            render_tokens(&group.tokens),
            render_cost(group.total_cost.as_deref())
        );
    }

    let _ = writeln!(out, "\nBy request kind:");
    for (kind, group) in &summary.by_helper {
        let _ = writeln!(
            out,
            "  {}: {} requests, {}, cost {}",
            kind,
            group.requests,
            render_tokens(&group.tokens),
            render_cost(group.total_cost.as_deref())
        );
    }
    out
}

/// Folds one record into a group.
fn fold(group: &mut Breakdown, record: &UsageRecord) {
    group.requests = group.requests.saturating_add(record.request_count);
    group.tokens.add(record);
    group.total_cost = add_cost(group.total_cost.take(), record.total_cost.as_deref());
}

/// Adds two optional counts.
fn sum(total: Option<u64>, value: Option<u64>) -> Option<u64> {
    match (total, value) {
        (Some(total), Some(value)) => Some(total.saturating_add(value)),
        (Some(total), None) => Some(total),
        (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

/// Adds an optional cost to an optional running total.
///
/// Costs are decimal strings reported by a provider, so the sum is exact decimal
/// addition rather than floating point. A malformed cost is ignored: the ledger
/// never invents a figure from text it cannot read.
fn add_cost(total: Option<String>, value: Option<&str>) -> Option<String> {
    let Some(value) = value else {
        return total;
    };
    let Some(value) = Decimal::parse(value) else {
        return total;
    };
    match total {
        Some(total) => Decimal::parse(&total)
            .and_then(|total| total.add(&value))
            .map(|sum| sum.to_string())
            // A total that cannot be re-read means the caller supplied one from
            // outside this module; the reported value is the honest fallback.
            .or_else(|| Some(value.to_string())),
        None => Some(value.to_string()),
    }
}

/// Adds a cost field to a JSON object only when a cost was reported.
fn insert_cost(object: &mut Map<String, Value>, cost: Option<&str>) {
    if let Some(cost) = cost {
        object.insert("total_cost".to_owned(), json!(cost));
    }
}

/// Renders token totals as a JSON object, omitting unreported counts.
fn tokens_json(tokens: &TokenTotals) -> Value {
    let mut object = Map::new();
    for (name, value) in [
        ("input_tokens", tokens.input_tokens),
        ("output_tokens", tokens.output_tokens),
        ("cache_read_tokens", tokens.cache_read_tokens),
        ("cache_write_tokens", tokens.cache_write_tokens),
        ("reasoning_tokens", tokens.reasoning_tokens),
    ] {
        if let Some(value) = value {
            object.insert(name.to_owned(), json!(value));
        }
    }
    Value::Object(object)
}

/// Renders one group as a JSON object.
fn breakdown_json(group: &Breakdown) -> Value {
    let mut object = Map::new();
    object.insert("requests".to_owned(), json!(group.requests));
    object.insert("tokens".to_owned(), tokens_json(&group.tokens));
    insert_cost(&mut object, group.total_cost.as_deref());
    Value::Object(object)
}

/// Renders token totals as one line.
fn render_tokens(tokens: &TokenTotals) -> String {
    if tokens.is_empty() {
        return "not reported".to_owned();
    }
    let mut parts: Vec<String> = Vec::new();
    for (name, value) in [
        ("input", tokens.input_tokens),
        ("output", tokens.output_tokens),
        ("cache read", tokens.cache_read_tokens),
        ("cache write", tokens.cache_write_tokens),
        ("reasoning", tokens.reasoning_tokens),
    ] {
        if let Some(value) = value {
            parts.push(format!("{name} {value}"));
        }
    }
    parts.join(", ")
}

/// Renders an optional cost.
fn render_cost(cost: Option<&str>) -> String {
    match cost {
        Some(cost) => cost.to_owned(),
        None => "not reported".to_owned(),
    }
}

/// Renders an optional instant.
fn render_instant(at: Option<i64>) -> String {
    match at.and_then(|ms| Timestamp::from_millisecond(ms).ok()) {
        Some(at) => at.strftime("%Y-%m-%dT%H:%M:%SZ").to_string(),
        None => "unknown".to_owned(),
    }
}

/// An exact non-negative decimal.
///
/// The ledger stores cost exactly as a provider reported it, so the sum must not
/// round. This is a scaled integer sum, not a float one.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Decimal {
    /// Digits with no decimal point, leading zeros removed.
    digits: String,
    /// Digits after the decimal point.
    scale: u32,
}

impl Decimal {
    /// Parses a decimal string. Returns `None` when it is not one.
    ///
    /// A sign is rejected: usage costs are never negative, and accepting one
    /// would let a malformed record hide spending.
    fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        let (whole, fraction) = match raw.split_once('.') {
            Some((whole, fraction)) => (whole, Some(fraction)),
            None => (raw, None),
        };
        if whole.is_empty() && fraction.is_none() {
            return None;
        }
        let scale = fraction.map_or(0, |digits| u32::try_from(digits.len()).unwrap_or(u32::MAX));
        let mut digits =
            String::with_capacity(whole.len().saturating_add(fraction.map_or(0, str::len)));
        digits.push_str(whole);
        if let Some(fraction) = fraction {
            digits.push_str(fraction);
        }
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let trimmed = digits.trim_start_matches('0');
        Some(Self {
            digits: if trimmed.is_empty() {
                "0".to_owned()
            } else {
                trimmed.to_owned()
            },
            scale,
        })
    }

    /// Adds another decimal, aligning the scales.
    fn add(&self, other: &Self) -> Option<Self> {
        let scale = self.scale.max(other.scale);
        let left = self.scaled_digits(scale)?;
        let right = other.scaled_digits(scale)?;
        let sum = add_digit_strings(&left, &right)?;
        Some(Self { digits: sum, scale })
    }

    /// Returns the digits scaled up to `scale` places.
    fn scaled_digits(&self, scale: u32) -> Option<String> {
        let pad = scale.checked_sub(self.scale)?;
        let pad = usize::try_from(pad).ok()?;
        let mut out = String::with_capacity(self.digits.len().saturating_add(pad));
        out.push_str(&self.digits);
        for _ in 0..pad {
            out.push('0');
        }
        Some(out)
    }
}

impl fmt::Display for Decimal {
    /// Renders the value with a decimal point where it belongs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scale = usize::try_from(self.scale).unwrap_or(usize::MAX);
        if scale == 0 {
            return f.write_str(&self.digits);
        }
        // Pad so the value always has at least one digit before the point.
        let width = scale.saturating_add(1);
        let mut padded = String::with_capacity(width);
        for _ in self.digits.len()..width {
            padded.push('0');
        }
        padded.push_str(&self.digits);
        let split = padded.len().saturating_sub(scale);
        let (whole, fraction) = padded.split_at(split);
        write!(f, "{whole}.{fraction}")
    }
}

/// Adds two strings of digits.
fn add_digit_strings(left: &str, right: &str) -> Option<String> {
    let mut out = String::with_capacity(left.len().max(right.len()).saturating_add(1));
    let mut carry = 0_u8;
    let mut left = left.bytes().rev();
    let mut right = right.bytes().rev();

    loop {
        let a = left.next().map(|b| b.saturating_sub(b'0'));
        let b = right.next().map(|b| b.saturating_sub(b'0'));
        if a.is_none() && b.is_none() {
            break;
        }
        let sum = a
            .unwrap_or(0)
            .saturating_add(b.unwrap_or(0))
            .saturating_add(carry);
        carry = sum / 10;
        out.push(char::from(b'0'.saturating_add(sum % 10)));
    }
    if carry > 0 {
        out.push(char::from(b'0'.saturating_add(carry)));
    }
    Some(out.chars().rev().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{HelperKind as Kind, Ledger, LedgerCaps, now_ms};
    use camino::Utf8PathBuf;

    const DAY_MS: i64 = 24 * 60 * 60 * 1_000;

    fn record(at: i64, model: &str) -> UsageRecord {
        UsageRecord::new(at, model, HelperKind::Main)
    }

    /// A ledger in a temporary directory, for the end-to-end path.
    fn ledger(dir: &tempfile::TempDir) -> Ledger {
        Ledger::new(Utf8PathBuf::from_path_buf(dir.path().join("usage.jsonl")).expect("utf-8"))
    }

    /// An instant inside the retention window.
    fn base() -> i64 {
        static BASE: std::sync::LazyLock<i64> =
            std::sync::LazyLock::new(|| now_ms().saturating_sub(1_000));
        *BASE
    }

    #[test]
    fn an_unreported_count_survives_a_write_a_read_and_a_json_render() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = ledger(&dir);
        let mut written = UsageRecord::new(base(), "m", Kind::Main);
        written.output_tokens = Some(4);
        ledger.append(&written).expect("append");

        let read = ledger.read().expect("read");
        let summary = summarize(
            &read.records,
            Period::Last24Hours,
            base().saturating_add(1_000),
        );
        let json = to_json(&summary);

        assert_eq!(read.records[0].input_tokens, None);
        assert_eq!(summary.tokens.input_tokens, None);
        let tokens = json.get("tokens").expect("tokens");
        assert!(
            tokens.get("input_tokens").is_none(),
            "an unreported count reached the JSON: {json}"
        );
        assert_eq!(tokens.get("output_tokens"), Some(&json!(4)));
        assert_eq!(summary.total_cost, None);
        assert!(json.get("total_cost").is_none(), "{json}");
    }

    #[test]
    fn a_reported_zero_survives_a_write_a_read_and_a_json_render() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = ledger(&dir);
        let mut written = UsageRecord::new(base(), "m", Kind::Main);
        written.input_tokens = Some(0);
        written.total_cost = Some("0".to_owned());
        ledger.append(&written).expect("append");

        let read = ledger.read().expect("read");
        let summary = summarize(
            &read.records,
            Period::Last24Hours,
            base().saturating_add(1_000),
        );
        let json = to_json(&summary);

        assert_eq!(read.records[0].input_tokens, Some(0));
        assert_eq!(summary.tokens.input_tokens, Some(0));
        assert_eq!(json["tokens"]["input_tokens"], json!(0));
        assert_eq!(summary.total_cost.as_deref(), Some("0"));
        assert_eq!(json["total_cost"], json!("0"));
    }

    #[test]
    fn a_zero_and_an_absent_count_stay_distinguishable_after_a_compaction() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(dir.path().join("usage.jsonl")).expect("utf-8");
        let ledger = Ledger::with_caps(
            path,
            LedgerCaps {
                max_records: 4,
                ..LedgerCaps::default()
            },
        );

        let mut reported = UsageRecord::new(base(), "zero-model", Kind::Main);
        reported.input_tokens = Some(0);
        let mut absent = UsageRecord::new(base().saturating_add(1), "absent-model", Kind::Main);
        absent.output_tokens = Some(5);

        // Enough appends to cross the cap and compact, so the distinction has
        // to survive a rewrite rather than only a straight read.
        for offset in 0..6 {
            ledger
                .append(&UsageRecord::new(
                    base().saturating_add(100).saturating_add(offset),
                    "filler",
                    Kind::Main,
                ))
                .expect("append filler");
        }
        ledger.append(&reported).expect("append zero");
        ledger.append(&absent).expect("append absent");

        let read = ledger.read().expect("read");
        let now = base().saturating_add(1_000);
        let summary = summarize(&read.records, Period::Last24Hours, now);

        let zero_model = summary.by_model.get("zero-model").expect("zero-model");
        assert_eq!(zero_model.tokens.input_tokens, Some(0));
        let absent_model = summary.by_model.get("absent-model").expect("absent-model");
        assert_eq!(absent_model.tokens.input_tokens, None);
        assert_eq!(absent_model.tokens.output_tokens, Some(5));

        let json = to_json(&summary);
        let models = json["by_model"].as_object().expect("models");
        assert_eq!(models["zero-model"]["tokens"]["input_tokens"], json!(0));
        assert!(
            models["absent-model"]["tokens"]
                .get("input_tokens")
                .is_none(),
            "{json}"
        );
    }

    #[test]
    fn a_cost_that_was_never_reported_is_absent_in_json_and_text() {
        let summary = summarize(&[record(1_000, "m")], Period::Last24Hours, 2_000);
        assert_eq!(summary.total_cost, None);

        let json = to_json(&summary);
        let object = json.as_object().expect("object");
        assert!(
            !object.contains_key("total_cost"),
            "an unreported cost must be omitted: {json}"
        );

        let text = render_text(&summary);
        assert!(text.contains("Cost:     not reported"), "{text}");
        assert!(!text.contains("Cost:     0"), "{text}");
    }

    #[test]
    fn a_reported_zero_cost_stays_present_as_zero() {
        let mut reported = record(1_000, "m");
        reported.total_cost = Some("0".to_owned());

        let summary = summarize(&[reported], Period::Last24Hours, 2_000);
        assert_eq!(summary.total_cost.as_deref(), Some("0"));

        let json = to_json(&summary);
        assert_eq!(json.get("total_cost"), Some(&json!("0")));
        assert!(
            render_text(&summary).contains("Cost:     0"),
            "{}",
            render_text(&summary)
        );
    }

    #[test]
    fn an_unreported_token_count_is_absent_in_the_summary_and_its_json() {
        let mut reported = record(1_000, "m");
        reported.output_tokens = Some(0);

        let summary = summarize(&[reported], Period::Last24Hours, 2_000);
        assert_eq!(summary.tokens.input_tokens, None);
        assert_eq!(summary.tokens.output_tokens, Some(0));

        let json = to_json(&summary);
        let tokens = json.get("tokens").expect("tokens");
        assert!(tokens.get("input_tokens").is_none(), "{json}");
        assert_eq!(tokens.get("output_tokens"), Some(&json!(0)));
    }

    #[test]
    fn token_totals_sum_reported_counts_only() {
        let mut first = record(1_000, "m");
        first.input_tokens = Some(10);
        first.output_tokens = Some(0);
        let mut second = record(1_100, "m");
        second.input_tokens = Some(5);

        let summary = summarize(&[first, second], Period::Last24Hours, 2_000);
        assert_eq!(summary.tokens.input_tokens, Some(15));
        assert_eq!(summary.tokens.output_tokens, Some(0));
        assert_eq!(summary.tokens.reasoning_tokens, None);
        assert_eq!(summary.requests, 2);
    }

    #[test]
    fn only_records_inside_the_period_are_counted() {
        let now = 30 * DAY_MS;
        let records = [
            record(now, "m"),
            record(now - DAY_MS, "m"),
            record(now - DAY_MS - 2, "m"),
            record(now - 3 * DAY_MS, "m"),
            record(now - 20 * DAY_MS, "m"),
        ];

        // The window is closed at both ends, so a record exactly on `since_ms`
        // belongs to the period.
        let day = summarize(&records, Period::Last24Hours, now);
        assert_eq!(day.requests, 2);
        assert_eq!(day.since_ms, now - DAY_MS);
        assert_eq!(summarize(&records, Period::Last7Days, now).requests, 4);
        assert_eq!(summarize(&records, Period::Last30Days, now).requests, 5);
    }

    #[test]
    fn a_record_just_outside_the_window_is_excluded() {
        let now = 10 * DAY_MS;
        let records = [record(now - DAY_MS, "m"), record(now - DAY_MS - 1, "m")];
        assert_eq!(summarize(&records, Period::Last24Hours, now).requests, 1);
    }

    #[test]
    fn a_record_after_the_report_instant_is_excluded() {
        let now = 10 * DAY_MS;
        let summary = summarize(
            &[record(now - 1, "m"), record(now + DAY_MS, "m")],
            Period::Last24Hours,
            now,
        );
        assert_eq!(summary.requests, 1);
    }

    #[test]
    fn all_three_periods_are_named_and_parsed() {
        for period in Period::all() {
            let parsed: Period = period.as_str().parse().expect("parse");
            assert_eq!(parsed, *period);
            assert!(period.duration_ms() > 0);
        }
        assert!("1h".parse::<Period>().is_err());
    }

    #[test]
    fn a_period_the_ledger_only_partly_covers_is_reported_as_partial() {
        let now = 30 * DAY_MS;
        let records = [record(now - 9 * DAY_MS, "m")];

        let partial = summarize(&records, Period::Last30Days, now);
        assert!(partial.coverage_is_partial);
        assert_eq!(partial.requests, 1);
        assert_eq!(partial.first_record_at_ms, Some(now - 9 * DAY_MS));
        assert!(
            to_json(&partial)["coverage_is_partial"]
                .as_bool()
                .expect("bool")
        );
        assert!(
            render_text(&partial).contains("Coverage: partial"),
            "{}",
            render_text(&partial)
        );
    }

    #[test]
    fn a_fully_covered_period_is_not_reported_as_partial() {
        let now = 30 * DAY_MS;
        let records = [record(now - 40 * DAY_MS, "m"), record(now - DAY_MS, "m")];

        let full = summarize(&records, Period::Last30Days, now);
        assert!(!full.coverage_is_partial);
        assert_eq!(full.first_record_at_ms, Some(now - 40 * DAY_MS));
        assert!(
            !to_json(&full)["coverage_is_partial"]
                .as_bool()
                .expect("bool")
        );

        let text = render_text(&full);
        assert!(text.contains("Coverage: full"), "{text}");
        assert!(!text.contains("partial"), "{text}");
    }

    #[test]
    fn an_empty_ledger_is_not_reported_as_partial() {
        let summary = summarize(&[], Period::Last24Hours, 1_000);
        assert!(!summary.coverage_is_partial);
        assert_eq!(summary.first_record_at_ms, None);
        assert_eq!(summary.requests, 0);
        assert_eq!(to_json(&summary)["first_record_at_ms"], Value::Null);
        assert!(render_text(&summary).contains("Coverage: full"));
    }

    #[test]
    fn a_new_ledger_inside_a_short_period_is_partial() {
        let now = now_ms();
        let summary = summarize(&[record(now - 60_000, "m")], Period::Last30Days, now);
        assert!(summary.coverage_is_partial);
    }

    #[test]
    fn the_breakdown_splits_by_model_and_helper_kind() {
        let mut main = record(1_000, "alpha");
        main.input_tokens = Some(10);
        main.total_cost = Some("0.5".to_owned());
        let mut review = record(1_100, "beta");
        review.helper = HelperKind::PermissionReview;
        review.input_tokens = Some(3);
        let mut vision = record(1_200, "alpha");
        vision.helper = HelperKind::Vision;
        vision.total_cost = Some("1.25".to_owned());

        let summary = summarize(&[main, review, vision], Period::Last24Hours, 2_000);

        assert_eq!(summary.by_model.len(), 2);
        let alpha = summary.by_model.get("alpha").expect("alpha");
        assert_eq!(alpha.requests, 2);
        assert_eq!(alpha.tokens.input_tokens, Some(10));
        assert_eq!(alpha.total_cost.as_deref(), Some("1.75"));

        let beta = summary.by_model.get("beta").expect("beta");
        assert_eq!(beta.requests, 1);
        assert_eq!(beta.total_cost, None);

        assert_eq!(summary.by_helper.len(), HelperKind::all().len());
        let kind = &summary.by_helper[&HelperKind::PermissionReview];
        assert_eq!(kind.requests, 1);
        assert_eq!(kind.tokens.input_tokens, Some(3));
        assert_eq!(summary.by_helper[&HelperKind::Main].requests, 1);
        assert_eq!(summary.by_helper[&HelperKind::Vision].requests, 1);
        assert_eq!(summary.total_cost.as_deref(), Some("1.75"));
    }

    #[test]
    fn helper_kinds_with_no_records_are_still_listed() {
        let summary = summarize(&[record(1_000, "m")], Period::Last24Hours, 2_000);
        let json = to_json(&summary);
        let helpers = json
            .get("by_helper")
            .and_then(Value::as_object)
            .expect("helpers");
        for kind in HelperKind::all() {
            assert!(
                helpers.contains_key(kind.as_str()),
                "missing {kind}: {json}"
            );
        }
        assert_eq!(helpers.len(), HelperKind::all().len());
    }

    #[test]
    fn request_count_is_summed_rather_than_counted_as_rows() {
        let mut batched = record(1_000, "m");
        batched.request_count = 4;
        let summary = summarize(&[batched, record(1_100, "m")], Period::Last24Hours, 2_000);
        assert_eq!(summary.requests, 5);
    }

    #[test]
    fn costs_sum_exactly_without_floating_point_drift() {
        let costs = ["0.1", "0.2", "0.000001", "9.999999"];
        let records: Vec<UsageRecord> = costs
            .iter()
            .map(|cost| {
                let mut record = record(1_000, "m");
                record.total_cost = Some((*cost).to_owned());
                record
            })
            .collect();

        let summary = summarize(&records, Period::Last24Hours, 2_000);
        assert_eq!(summary.total_cost.as_deref(), Some("10.300000"));
    }

    #[test]
    fn a_malformed_cost_is_ignored_rather_than_summed_as_zero() {
        let mut good = record(1_000, "m");
        good.total_cost = Some("2.5".to_owned());
        let mut bad = record(1_100, "m");
        bad.total_cost = Some("free".to_owned());

        let summary = summarize(&[good, bad], Period::Last24Hours, 2_000);
        assert_eq!(summary.total_cost.as_deref(), Some("2.5"));
    }

    #[test]
    fn a_decimal_renders_a_leading_zero() {
        assert_eq!(Decimal::parse(".5").expect("parse").to_string(), "0.5");
        assert_eq!(Decimal::parse("0007").expect("parse").to_string(), "7");
        assert_eq!(Decimal::parse("1.0").expect("parse").to_string(), "1.0");
        assert!(Decimal::parse("").is_none());
        assert!(Decimal::parse("-1").is_none());
        assert!(Decimal::parse("1.2.3").is_none());
    }

    #[test]
    fn json_carries_the_period_and_window_bounds() {
        let now = 5 * DAY_MS;
        let summary = summarize(&[record(now, "m")], Period::Last24Hours, now);
        let json = to_json(&summary);
        assert_eq!(json["period"], json!("24h"));
        assert_eq!(json["now_ms"], json!(now));
        assert_eq!(json["since_ms"], json!(now - DAY_MS));
        assert_eq!(json["requests"], json!(1));
    }

    #[test]
    fn a_summary_renders_every_section() {
        let mut reported = record(1_000, "alpha");
        reported.input_tokens = Some(0);
        reported.output_tokens = Some(7);
        reported.total_cost = Some("0.25".to_owned());
        let summary = summarize(&[reported], Period::Last7Days, 2_000);

        let text = render_text(&summary);
        assert!(text.contains("Usage for the last 7d"), "{text}");
        assert!(text.contains("alpha: 1 requests"), "{text}");
        assert!(text.contains("input 0"), "{text}");
        assert!(text.contains("output 7"), "{text}");
        assert!(text.contains("Cost:     0.25"), "{text}");
        assert!(text.contains("permission_review"), "{text}");
    }

    #[test]
    fn an_empty_summary_renders_without_a_model_section_value() {
        let text = render_text(&summarize(&[], Period::Last24Hours, 1_000));
        assert!(text.contains("Requests: 0"), "{text}");
        assert!(text.contains("Tokens:   not reported"), "{text}");
        assert!(text.contains("Cost:     not reported"), "{text}");
        assert!(text.contains("none"), "{text}");
    }
}
