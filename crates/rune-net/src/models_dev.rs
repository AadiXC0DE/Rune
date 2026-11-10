//! Model capacity read from the published models.dev catalog.
//!
//! An OpenAI-compatible listing carries an identifier and little else: the
//! opencode endpoint returns `id`, `object`, `created`, and `owned_by`, and no
//! capacity at all. Reading only that listing left every model budgeted against
//! the compiled default, so a model serving a million tokens reported as though
//! it held a hundred and twenty-eight thousand.
//!
//! models.dev is the catalog the compatible clients read for exactly this
//! reason. It is a single JSON document keyed by provider, and within each
//! provider keyed by model identifier, which is the same identifier the
//! endpoint lists. Only the capacity and the published prices are taken from
//! it: this is a description of a model, not a source of endpoints or
//! credentials, and nothing here is trusted for anything but numbers.

use std::collections::BTreeMap;

/// Largest catalog document accepted.
///
/// The published document is a few megabytes. The ceiling is generous enough to
/// survive it growing and tight enough that a wrong address cannot stream
/// something unbounded into memory.
pub const MAX_CATALOG_BYTES: usize = 32 * 1024 * 1024;

/// Capacity of one model, as the catalog describes it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// Input capacity in tokens.
    pub context: Option<u64>,
    /// Output ceiling in tokens.
    pub output: Option<u64>,
    /// Published prices, when the catalog lists them.
    pub prices: Option<Prices>,
    /// Whether the model takes images, when the catalog lists its inputs.
    pub vision: Option<bool>,
}

impl Limits {
    /// Returns whether nothing usable was described.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.context.is_none()
            && self.output.is_none()
            && self.prices.is_none()
            && self.vision.is_none()
    }
}

/// Published prices of one model, in millionths of a US dollar per million
/// tokens.
///
/// Whole numbers keep the arithmetic exact and the type comparable; a price of
/// $0.28 per million tokens is 280,000. A cache price that is not published is
/// charged at the input price.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct Prices {
    /// Uncached input.
    pub input: u64,
    /// Output, reasoning included.
    pub output: u64,
    /// Input read from a prompt cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<u64>,
    /// Input written to a prompt cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<u64>,
    /// Prices outside the peak window, for a model whose price follows the
    /// clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off_peak: Option<OffPeak>,
    /// When the peak window runs; the other fields are the peak prices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<PeakSchedule>,
}

/// Prices charged outside the peak window, in the same unit as [`Prices`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct OffPeak {
    /// Uncached input.
    pub input: u64,
    /// Output, reasoning included.
    pub output: u64,
    /// Input read from a prompt cache.
    pub cache_read: Option<u64>,
}

/// A recurring window in which the peak price applies.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeakSchedule {
    /// Monday to Friday in UTC, 01:00 to 03:59 and 06:00 to 09:59.
    WeekdaysUtc0104And0610,
}

impl PeakSchedule {
    /// Returns whether the peak price applies at a moment. Public holidays
    /// are not modelled, so a holiday is priced as an ordinary day.
    #[must_use]
    pub fn is_peak(self, at: jiff::Timestamp) -> bool {
        const DAY: i64 = 86_400;
        let seconds = at.as_second();
        // The epoch fell on a Thursday, so this counts Monday as zero.
        let weekday = seconds.div_euclid(DAY).saturating_add(3).rem_euclid(7);
        let hour = seconds.rem_euclid(DAY) / 3_600;
        match self {
            Self::WeekdaysUtc0104And0610 => {
                weekday < 5 && ((1..4).contains(&hour) || (6..10).contains(&hour))
            }
        }
    }
}

impl Prices {
    /// Returns the cost of one request's usage in millionths of a dollar.
    ///
    /// Absent when the usage reports no token counts. The input count is the
    /// whole prompt, so its cached parts are taken out and priced on their own.
    #[must_use]
    pub fn cost(&self, usage: &crate::stream::Usage) -> Option<u64> {
        if usage.input_tokens.is_none() && usage.output_tokens.is_none() {
            return None;
        }
        let read = usage.cache_read_tokens.unwrap_or_default();
        let written = usage.cache_write_tokens.unwrap_or_default();
        let uncached = usage
            .input_tokens
            .unwrap_or_default()
            .saturating_sub(read)
            .saturating_sub(written);
        let charge = |tokens: u64, price: u64| u128::from(tokens).saturating_mul(u128::from(price));
        let total = charge(uncached, self.input)
            .saturating_add(charge(read, self.cache_read.unwrap_or(self.input)))
            .saturating_add(charge(written, self.cache_write.unwrap_or(self.input)))
            .saturating_add(charge(usage.output_tokens.unwrap_or_default(), self.output));
        // Rounded to the nearest millionth of a dollar.
        let micro = total.saturating_add(500_000) / 1_000_000;
        Some(u64::try_from(micro).unwrap_or(u64::MAX))
    }

    /// Returns the cost of one request's usage at a moment, which differs
    /// from [`Prices::cost`] only outside the peak window of a scheduled
    /// model.
    #[must_use]
    pub fn cost_at(&self, usage: &crate::stream::Usage, at: jiff::Timestamp) -> Option<u64> {
        match (self.schedule, self.off_peak) {
            (Some(schedule), Some(off_peak)) if !schedule.is_peak(at) => Self {
                input: off_peak.input,
                output: off_peak.output,
                cache_read: off_peak.cache_read,
                cache_write: None,
                off_peak: None,
                schedule: None,
            }
            .cost(usage),
            _ => self.cost(usage),
        }
    }
}

/// DeepSeek's peak and off-peak prices. The catalog lists one figure per
/// model, and that figure is the off-peak one, so it cannot stand as the list
/// price.
const DEEPSEEK_PRICES: [(&[&str], Prices); 2] = [
    (
        &[
            "deepseek-flash",
            "deepseek-v4-flash",
            "deepseek-v4-flash-vision-exp",
        ],
        Prices {
            input: 300_000,
            output: 1_200_000,
            cache_read: Some(6_000),
            cache_write: None,
            off_peak: Some(OffPeak {
                input: 150_000,
                output: 600_000,
                cache_read: Some(3_000),
            }),
            schedule: Some(PeakSchedule::WeekdaysUtc0104And0610),
        },
    ),
    (
        &["deepseek-v4-pro"],
        Prices {
            input: 1_320_000,
            output: 3_960_000,
            cache_read: Some(44_000),
            cache_write: None,
            off_peak: Some(OffPeak {
                input: 660_000,
                output: 1_980_000,
                cache_read: Some(22_000),
            }),
            schedule: Some(PeakSchedule::WeekdaysUtc0104And0610),
        },
    ),
];

/// Renders millionths of a dollar as a decimal amount, such as `0.001523`.
#[must_use]
pub fn dollars(micro: u64) -> String {
    format!("{}.{:06}", micro / 1_000_000, micro % 1_000_000)
}

/// Reads a price in dollars per million tokens as millionths of a dollar.
fn micro_price(value: Option<&serde_json::Value>) -> Option<u64> {
    let dollars = value?.as_f64()?;
    if !dollars.is_finite() || dollars < 0.0 {
        return None;
    }
    // A catalog price has at most a few decimal places, so rounding to whole
    // millionths loses nothing a bill would show.
    let micro = (dollars * 1_000_000.0).round();
    (micro <= 1e15).then_some(micro as u64)
}

/// Capacity for every model of one provider.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct ProviderLimits {
    /// Model identifier to its capacity.
    pub models: BTreeMap<String, Limits>,
}

impl ProviderLimits {
    /// Returns the capacity described for a model, when there is one.
    #[must_use]
    pub fn get(&self, model: &str) -> Option<Limits> {
        self.models.get(model).copied().filter(|l| !l.is_empty())
    }

    /// Returns how many models were described.
    #[must_use]
    pub fn len(&self) -> usize {
        self.models.len()
    }

    /// Returns whether no model was described.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }
}

/// Maps a provider name to its key in the catalog.
///
/// The keys are not always the name this program uses, so they are read from
/// the provider table. A compatible or local endpoint is unknown to the
/// catalog, and a name the table does not list yields nothing rather than a
/// guess.
#[must_use]
pub fn catalog_key(provider: &str) -> Option<&'static str> {
    crate::providers::lookup(provider).and_then(|entry| entry.models_dev_key)
}

/// Reads the capacity for one provider out of a catalog document.
///
/// Returns `None` when the document is not the shape this reads or the provider
/// is absent from it. A model the catalog does not describe is simply missing
/// from the result, which leaves the figure already in force rather than
/// inventing one.
#[must_use]
pub fn parse(body: &str, key: &str) -> Option<ProviderLimits> {
    let document: serde_json::Value = serde_json::from_str(body).ok()?;
    let provider = document.get(key)?;
    let models = provider.get("models")?.as_object()?;

    let mut limits = ProviderLimits::default();
    for (id, entry) in models {
        let limit = entry.get("limit");
        let context = limit
            .and_then(|l| l.get("context"))
            .and_then(serde_json::Value::as_u64)
            .filter(|value| *value > 0);
        let output = limit
            .and_then(|l| l.get("output"))
            .and_then(serde_json::Value::as_u64)
            .filter(|value| *value > 0);
        let cost = entry.get("cost");
        let prices = match (
            micro_price(cost.and_then(|c| c.get("input"))),
            micro_price(cost.and_then(|c| c.get("output"))),
        ) {
            (Some(input), Some(output)) => Some(Prices {
                input,
                output,
                cache_read: micro_price(cost.and_then(|c| c.get("cache_read"))),
                cache_write: micro_price(cost.and_then(|c| c.get("cache_write"))),
                off_peak: None,
                schedule: None,
            }),
            _ => None,
        };
        let vision = entry
            .get("modalities")
            .and_then(|m| m.get("input"))
            .and_then(serde_json::Value::as_array)
            .map(|inputs| inputs.iter().any(|input| input == "image"));
        let entry = Limits {
            context,
            output,
            prices,
            vision,
        };
        if entry.is_empty() {
            continue;
        }
        limits.models.insert(id.clone(), entry);
    }
    if key == "deepseek" {
        for (ids, prices) in DEEPSEEK_PRICES {
            for id in ids {
                if let Some(entry) = limits.models.get_mut(*id) {
                    entry.prices = Some(prices);
                }
            }
        }
    }
    Some(limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A document shaped like the published one, trimmed to what is read.
    const DOC: &str = r#"{
        "opencode-go": {
            "id": "opencode-go",
            "name": "OpenCode Go",
            "api": "https://opencode.ai/zen/go/v1",
            "env": ["OPENCODE_API_KEY"],
            "models": {
                "grok-4.7": { "id": "grok-4.7", "name": "Grok 4.7",
                              "limit": { "context": 500000, "output": 500000 },
                              "cost": { "input": 0.28, "output": 1.1, "cache_read": 0.028 } },
                "glm-5.3-flash": { "id": "glm-5.3-flash", "name": "GLM-5.3-Flash",
                                   "limit": { "context": 1000000, "output": 131072 } }
            }
        },
        "other": { "id": "other", "models": { "m": { "limit": { "context": 4096 } } } }
    }"#;

    #[test]
    fn the_capacity_of_a_listed_model_is_read() {
        let limits = parse(DOC, "opencode-go").expect("parsed");
        assert_eq!(limits.len(), 2);
        let grok = limits.get("grok-4.7").expect("grok");
        assert_eq!(grok.context, Some(500_000));
        assert_eq!(grok.output, Some(500_000));
        assert_eq!(
            limits.get("glm-5.3-flash").and_then(|l| l.context),
            Some(1_000_000)
        );
    }

    #[test]
    fn published_prices_are_read_in_millionths_of_a_dollar() {
        let limits = parse(DOC, "opencode-go").expect("parsed");
        let prices = limits
            .get("grok-4.7")
            .and_then(|l| l.prices)
            .expect("prices");
        assert_eq!(
            prices,
            Prices {
                input: 280_000,
                output: 1_100_000,
                cache_read: Some(28_000),
                cache_write: None,
                off_peak: None,
                schedule: None,
            }
        );
        assert_eq!(limits.get("glm-5.3-flash").and_then(|l| l.prices), None);
    }

    #[test]
    fn a_request_is_priced_with_its_cached_input_apart() {
        let prices = Prices {
            input: 280_000,
            output: 1_100_000,
            cache_read: Some(28_000),
            cache_write: None,
            off_peak: None,
            schedule: None,
        };
        let usage = crate::stream::Usage {
            input_tokens: Some(10_000),
            cache_read_tokens: Some(8_000),
            output_tokens: Some(500),
            ..crate::stream::Usage::default()
        };
        // 2,000 uncached at 0.28, 8,000 cached at 0.028, 500 out at 1.10, per
        // million: 560 + 224 + 550 millionths.
        assert_eq!(prices.cost(&usage), Some(1_334));
        assert_eq!(dollars(1_334), "0.001334");
        assert_eq!(prices.cost(&crate::stream::Usage::default()), None);
    }

    #[test]
    fn a_cached_gemini_prompt_is_priced_at_the_cache_rate() {
        let doc = r#"{"google": {"id": "google", "models": {
            "gemini-3.8-flash": { "id": "gemini-3.8-flash",
                                  "limit": { "context": 1048576, "output": 65536 },
                                  "cost": { "input": 0.75, "output": 3.75, "cache_read": 0.075 } }
        }}}"#;
        let prices = parse(doc, "google")
            .and_then(|limits| limits.get("gemini-3.8-flash"))
            .and_then(|limits| limits.prices)
            .expect("prices");
        let usage = crate::stream::Usage {
            input_tokens: Some(1_000),
            cache_read_tokens: Some(800),
            ..crate::stream::Usage::default()
        };
        // 200 uncached at 0.75 and 800 cached at 0.075, per million: 150 + 60
        // millionths.
        assert_eq!(prices.cost(&usage), Some(210));
    }

    /// The catalog's DeepSeek entries, which list the off-peak figure only.
    const DEEPSEEK_DOC: &str = r#"{"deepseek": {"id": "deepseek", "models": {
        "deepseek-flash": { "limit": { "context": 1000000, "output": 384000 },
                            "cost": { "input": 0.15, "output": 0.6, "cache_read": 0.003 } },
        "deepseek-v4-pro": { "limit": { "context": 1000000, "output": 384000 },
                             "cost": { "input": 0.66, "output": 1.98, "cache_read": 0.022 } }
    }}}"#;

    fn at(moment: &str) -> jiff::Timestamp {
        moment.parse().expect("timestamp")
    }

    #[test]
    fn deepseek_prices_follow_the_peak_window() {
        let schedule = PeakSchedule::WeekdaysUtc0104And0610;
        assert!(schedule.is_peak(at("2026-11-09T03:59:59Z")));
        assert!(!schedule.is_peak(at("2026-11-09T04:00:00Z")));
        assert!(schedule.is_peak(at("2026-11-09T01:00:00Z")));
        assert!(!schedule.is_peak(at("2026-11-09T00:59:59Z")));
        assert!(schedule.is_peak(at("2026-11-10T09:59:59Z")));
        assert!(!schedule.is_peak(at("2026-11-10T10:00:00Z")));
        assert!(!schedule.is_peak(at("2026-11-14T02:00:00Z")));

        let limits = parse(DEEPSEEK_DOC, "deepseek").expect("parsed");
        let flash = limits
            .get("deepseek-flash")
            .and_then(|l| l.prices)
            .expect("prices");
        assert_eq!(flash.input, 300_000);
        assert_eq!(flash.schedule, Some(schedule));
        let pro = limits
            .get("deepseek-v4-pro")
            .and_then(|l| l.prices)
            .expect("prices");
        assert_eq!(pro.output, 3_960_000);

        let usage = crate::stream::Usage {
            input_tokens: Some(100_000),
            output_tokens: Some(10_000),
            ..crate::stream::Usage::default()
        };
        // 100,000 in at 0.30 and 10,000 out at 1.20 per million: 30,000 +
        // 12,000 millionths at peak, half that off peak.
        let peak = flash.cost_at(&usage, at("2026-11-09T02:00:00Z"));
        let saturday = flash.cost_at(&usage, at("2026-11-14T02:00:00Z"));
        assert_eq!(peak, Some(42_000));
        assert_eq!(saturday, Some(21_000));
        assert_eq!(flash.cost(&usage), peak);
        // A model without a schedule costs the same at any hour.
        let other = parse(DOC, "opencode-go")
            .and_then(|l| l.get("grok-4.7"))
            .and_then(|l| l.prices)
            .expect("prices");
        assert_eq!(
            other.cost_at(&usage, at("2026-11-14T02:00:00Z")),
            other.cost(&usage)
        );
    }

    #[test]
    fn cache_hits_are_priced_as_cache_reads() {
        let flash = parse(DEEPSEEK_DOC, "deepseek")
            .and_then(|l| l.get("deepseek-flash"))
            .and_then(|l| l.prices)
            .expect("prices");
        let usage = crate::stream::Usage {
            input_tokens: Some(1_000_000),
            cache_read_tokens: Some(800_000),
            ..crate::stream::Usage::default()
        };
        // 200,000 uncached at 0.30 and 800,000 cached at 0.006 per million
        // at peak; 0.15 and 0.003 off peak.
        assert_eq!(
            flash.cost_at(&usage, at("2026-11-09T02:00:00Z")),
            Some(64_800)
        );
        assert_eq!(
            flash.cost_at(&usage, at("2026-11-14T02:00:00Z")),
            Some(32_400)
        );
    }

    #[test]
    fn a_model_the_catalog_does_not_describe_yields_nothing() {
        // A figure that is absent must not become a number here, or a model the
        // catalog has never heard of would be budgeted against a guess.
        let limits = parse(DOC, "opencode-go").expect("parsed");
        assert_eq!(limits.get("never-heard-of-it"), None);
    }

    #[test]
    fn a_provider_that_is_absent_is_not_a_failure() {
        assert!(parse(DOC, "not-a-provider").is_none());
    }

    #[test]
    fn a_body_that_is_not_the_catalog_is_refused() {
        assert!(parse("<html>nope</html>", "opencode-go").is_none());
        assert!(parse("{}", "opencode-go").is_none());
        assert!(parse("", "opencode-go").is_none());
        // The right provider key with no model table is not usable either.
        assert!(parse(r#"{"p":{}}"#, "p").is_none());
        assert!(parse(r#"{"p":{"models":[]}}"#, "p").is_none());
    }

    #[test]
    fn an_entry_with_no_capacity_is_left_out() {
        let body = r#"{"p":{"models":{"a":{"limit":{}},"b":{"limit":{"context":0}},
                                       "c":{"id":"c","limit":{"context":8192}}}}}"#;
        let limits = parse(body, "p").expect("parsed");
        assert_eq!(limits.len(), 1);
        assert_eq!(limits.get("c").and_then(|l| l.context), Some(8192));
    }

    #[test]
    fn a_zero_capacity_is_treated_as_absent() {
        // A window of nothing would make every request look oversized.
        let body = r#"{"p":{"models":{"a":{"limit":{"context":0,"output":0}}}}}"#;
        let limits = parse(body, "p").expect("parsed");
        assert!(limits.is_empty());
    }

    #[test]
    fn only_a_context_or_only_an_output_is_kept() {
        let body = r#"{"p":{"models":{"a":{"limit":{"context":4096}},
                                       "b":{"limit":{"output":512}}}}}"#;
        let limits = parse(body, "p").expect("parsed");
        assert_eq!(limits.get("a").and_then(|l| l.context), Some(4096));
        assert_eq!(limits.get("a").and_then(|l| l.output), None);
        assert_eq!(limits.get("b").and_then(|l| l.output), Some(512));
        assert_eq!(limits.get("b").and_then(|l| l.context), None);
    }

    #[test]
    fn the_keys_this_build_knows_are_the_ones_the_catalog_publishes() {
        for (ours, theirs) in [
            ("anthropic", "anthropic"),
            ("openai", "openai"),
            ("responses", "openai"),
            ("opencode", "opencode"),
            ("opencode-go", "opencode-go"),
        ] {
            assert_eq!(catalog_key(ours), Some(theirs), "{ours}");
        }
        // A compatible endpoint is not described by any published catalog.
        assert_eq!(catalog_key("chat_completions"), None);
        assert_eq!(catalog_key("my-self-hosted"), None);
    }

    #[test]
    fn image_input_is_read_from_the_modalities() {
        let doc = r#"{ "p": { "models": {
            "sees": { "limit": { "context": 1000 },
                      "modalities": { "input": ["text", "image"], "output": ["text"] } },
            "blind": { "limit": { "context": 1000 },
                       "modalities": { "input": ["text"], "output": ["text"] } },
            "silent": { "limit": { "context": 1000 } },
            "only": { "modalities": { "input": ["image"] } }
        } } }"#;
        let limits = parse(doc, "p").expect("parsed");
        assert_eq!(limits.get("sees").expect("sees").vision, Some(true));
        assert_eq!(limits.get("blind").expect("blind").vision, Some(false));
        assert_eq!(limits.get("silent").expect("silent").vision, None);
        assert_eq!(limits.get("only").expect("only").vision, Some(true));
    }
}
