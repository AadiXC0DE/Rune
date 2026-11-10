//! How each endpoint wants requests shaped and how each model family behaves.
//!
//! OpenAI-compatible servers differ in many small ways: which field caps the
//! answer, which role carries the instructions, how reasoning is asked for and
//! replayed. Writing those differences down as one table of named rules, with
//! a resolver that reports which rule set each value, keeps them in one place
//! and lets `rune models --explain` show the reason behind every value.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::Write as _;

use rune_core::config::{EditToolSetting, Effort};
use rune_core::error::{Result, RuneError};
use sha2::{Digest as _, Sha256};

pub use rune_core::config::EditTool;

use crate::provider::ALL_EFFORTS;

/// Lists every value of a setting enum with its configuration name.
trait Named: Sized + 'static {
    /// Every configuration name, in declaration order.
    const NAMES: &'static [&'static str];
    /// Reads a configuration name.
    fn from_name(text: &str) -> Option<Self>;
}

macro_rules! named_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($(#[$vmeta])* #[serde(rename = $text)] $variant),+
        }

        impl $name {
            /// Returns the configuration name.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            /// Reads a configuration name, accepting exactly the names `as_str` returns.
            #[must_use]
            pub fn parse(text: &str) -> Option<Self> {
                match text {
                    $($text => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }

        impl Named for $name {
            const NAMES: &'static [&'static str] = &[$($text),+];
            fn from_name(text: &str) -> Option<Self> {
                Self::parse(text)
            }
        }
    };
}

named_enum! {
    /// The request field that caps the answer length.
    MaxTokensField {
        /// `max_tokens`.
        MaxTokens => "max_tokens",
        /// `max_completion_tokens`.
        MaxCompletionTokens => "max_completion_tokens",
    }
}

named_enum! {
    /// The role that carries the instructions.
    SystemRole {
        /// `system`.
        System => "system",
        /// `developer`.
        Developer => "developer",
    }
}

named_enum! {
    /// The content of an assistant message that holds only tool calls.
    EmptyContent {
        /// JSON `null`.
        Null => "null",
        /// An empty string.
        EmptyString => "empty_string",
    }
}

named_enum! {
    /// The shape of tool call identifiers an endpoint accepts.
    CallIdShape {
        /// Any identifier.
        Any => "any",
        /// Exactly nine letters and digits.
        Alnum9 => "alnum9",
        /// At most forty characters.
        Max40 => "max40",
        /// Letters, digits, `_` and `-` only.
        ClaudeSafe => "claude_safe",
    }
}

named_enum! {
    /// How reasoning is asked for.
    ThinkingFormat {
        /// Not at all.
        None => "none",
        /// `reasoning_effort`.
        ReasoningEffort => "reasoning_effort",
        /// OpenRouter's `reasoning` object.
        OpenRouter => "openrouter",
        /// DeepSeek's `thinking` object.
        DeepSeek => "deepseek",
        /// Z.ai's `thinking` object.
        Zai => "zai",
        /// `enable_thinking`.
        QwenEnable => "qwen_enable",
        /// `chat_template_kwargs`.
        ChatTemplate => "chat_template",
        /// MiniMax's `reasoning_split`.
        MiniMax => "minimax",
    }
}

named_enum! {
    /// Where earlier reasoning is sent back.
    ReasoningReplay {
        /// It is not sent back.
        None => "none",
        /// `reasoning_content` on the assistant message.
        ReasoningContent => "reasoning_content",
        /// `reasoning` on the assistant message.
        Reasoning => "reasoning",
        /// `reasoning_details` on the assistant message.
        ReasoningDetails => "reasoning_details",
        /// In the field it arrived in.
        AsObserved => "as_observed",
    }
}

named_enum! {
    /// Which prompt cache markers are sent.
    CacheMarkers {
        /// None.
        None => "none",
        /// Anthropic `cache_control` markers.
        Anthropic => "anthropic",
    }
}

named_enum! {
    /// The JSON Schema subset an endpoint accepts in tool schemas.
    SchemaRules {
        /// Any schema.
        Plain => "plain",
        /// Google's subset: string enums, one type, nullable.
        Gemini => "gemini",
        /// Moonshot's subset: bare references, one item schema.
        Moonshot => "moonshot",
    }
}

named_enum! {
    /// The model family an identifier belongs to.
    Family {
        /// Anthropic Claude.
        Claude => "claude",
        /// OpenAI GPT and o-series.
        Gpt => "gpt",
        /// Google Gemini.
        Gemini => "gemini",
        /// DeepSeek.
        DeepSeek => "deepseek",
        /// Moonshot Kimi.
        Kimi => "kimi",
        /// Z.ai GLM.
        Glm => "glm",
        /// Alibaba Qwen.
        Qwen => "qwen",
        /// MiniMax.
        MiniMax => "minimax",
        /// xAI Grok.
        Grok => "grok",
        /// Mistral.
        Mistral => "mistral",
        /// Meta Llama.
        Llama => "llama",
        /// Anything else.
        Other => "other",
    }
}

named_enum! {
    /// Whether a model reads images.
    Images {
        /// It does.
        Yes => "yes",
        /// It does not.
        No => "no",
        /// Not known.
        Unknown => "unknown",
    }
}

named_enum! {
    /// How a Claude model takes its thinking controls.
    ClaudeThinking {
        /// The model is not a Claude model.
        NotClaude => "not_claude",
        /// A token budget.
        Budget => "budget",
        /// Adaptive thinking.
        Adaptive => "adaptive",
        /// Adaptive thinking that cannot be turned off.
        AdaptiveAlwaysOn => "adaptive_always_on",
        /// Adaptive thinking between tool calls.
        AdaptiveBetweenTools => "adaptive_between_tools",
    }
}

/// How an endpoint wants Chat Completions requests shaped.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize)]
pub struct EndpointCompat {
    /// The field that caps the answer.
    pub max_tokens_field: MaxTokensField,
    /// The role of the instructions.
    pub system_role: SystemRole,
    /// Whether usage is requested in the stream.
    pub stream_usage: bool,
    /// Whether a stream must end with `[DONE]` to count as complete.
    pub needs_done_marker: bool,
    /// The content of an assistant message with only tool calls.
    pub assistant_content_when_only_calls: EmptyContent,
    /// Whether a tool result carries the tool name.
    pub tool_result_name: bool,
    /// Whether tools must be sent whenever the history holds tool calls.
    pub tools_with_tool_history: bool,
    /// Whether an assistant message must follow a tool result.
    pub assistant_after_tool_result: bool,
    /// The tool call identifiers the endpoint accepts.
    pub tool_call_id: CallIdShape,
    /// How reasoning is asked for.
    pub thinking_format: ThinkingFormat,
    /// Where earlier reasoning is sent back.
    pub reasoning_replay: ReasoningReplay,
    /// Whether reasoning is sent back on every assistant message.
    pub reasoning_always: bool,
    /// Which prompt cache markers are sent.
    pub cache_control: CacheMarkers,
    /// Whether `prompt_cache_key` is sent.
    pub prompt_cache_key: bool,
    /// Whether `parallel_tool_calls` is sent.
    pub parallel_tool_calls_field: bool,
    /// Whether tool schemas are sent with `strict`.
    pub strict_schema: bool,
    /// The schema subset tool schemas are rewritten into.
    pub schema_rules: SchemaRules,
}

impl Default for EndpointCompat {
    fn default() -> Self {
        Self {
            max_tokens_field: MaxTokensField::MaxTokens,
            system_role: SystemRole::System,
            stream_usage: true,
            needs_done_marker: false,
            assistant_content_when_only_calls: EmptyContent::Null,
            tool_result_name: false,
            tools_with_tool_history: false,
            assistant_after_tool_result: false,
            tool_call_id: CallIdShape::Any,
            thinking_format: ThinkingFormat::ReasoningEffort,
            reasoning_replay: ReasoningReplay::None,
            reasoning_always: false,
            cache_control: CacheMarkers::None,
            prompt_cache_key: false,
            parallel_tool_calls_field: true,
            strict_schema: false,
            schema_rules: SchemaRules::Plain,
        }
    }
}

/// The names of the endpoint settings, in declaration order.
const ENDPOINT_FIELDS: [&str; 17] = [
    "max_tokens_field",
    "system_role",
    "stream_usage",
    "needs_done_marker",
    "assistant_content_when_only_calls",
    "tool_result_name",
    "tools_with_tool_history",
    "assistant_after_tool_result",
    "tool_call_id",
    "thinking_format",
    "reasoning_replay",
    "reasoning_always",
    "cache_control",
    "prompt_cache_key",
    "parallel_tool_calls_field",
    "strict_schema",
    "schema_rules",
];

impl EndpointCompat {
    /// Returns each setting's name and value, in declaration order.
    #[must_use]
    pub fn fields(&self) -> [(&'static str, String); 17] {
        let values = [
            self.max_tokens_field.as_str().to_owned(),
            self.system_role.as_str().to_owned(),
            self.stream_usage.to_string(),
            self.needs_done_marker.to_string(),
            self.assistant_content_when_only_calls.as_str().to_owned(),
            self.tool_result_name.to_string(),
            self.tools_with_tool_history.to_string(),
            self.assistant_after_tool_result.to_string(),
            self.tool_call_id.as_str().to_owned(),
            self.thinking_format.as_str().to_owned(),
            self.reasoning_replay.as_str().to_owned(),
            self.reasoning_always.to_string(),
            self.cache_control.as_str().to_owned(),
            self.prompt_cache_key.to_string(),
            self.parallel_tool_calls_field.to_string(),
            self.strict_schema.to_string(),
            self.schema_rules.as_str().to_owned(),
        ];
        let mut values = values.into_iter();
        ENDPOINT_FIELDS.map(|name| (name, values.next().unwrap_or_default()))
    }

    /// Sets one setting from its configuration value.
    ///
    /// Returns `None` for an unknown name, and the accepted values when the
    /// value is not one of them.
    fn set(
        &mut self,
        name: &str,
        value: &str,
    ) -> Option<std::result::Result<(), &'static [&'static str]>> {
        Some(match name {
            "max_tokens_field" => set_named(&mut self.max_tokens_field, value),
            "system_role" => set_named(&mut self.system_role, value),
            "stream_usage" => set_flag(&mut self.stream_usage, value),
            "needs_done_marker" => set_flag(&mut self.needs_done_marker, value),
            "assistant_content_when_only_calls" => {
                set_named(&mut self.assistant_content_when_only_calls, value)
            }
            "tool_result_name" => set_flag(&mut self.tool_result_name, value),
            "tools_with_tool_history" => set_flag(&mut self.tools_with_tool_history, value),
            "assistant_after_tool_result" => set_flag(&mut self.assistant_after_tool_result, value),
            "tool_call_id" => set_named(&mut self.tool_call_id, value),
            "thinking_format" => set_named(&mut self.thinking_format, value),
            "reasoning_replay" => set_named(&mut self.reasoning_replay, value),
            "reasoning_always" => set_flag(&mut self.reasoning_always, value),
            "cache_control" => set_named(&mut self.cache_control, value),
            "prompt_cache_key" => set_flag(&mut self.prompt_cache_key, value),
            "parallel_tool_calls_field" => set_flag(&mut self.parallel_tool_calls_field, value),
            "strict_schema" => set_flag(&mut self.strict_schema, value),
            "schema_rules" => set_named(&mut self.schema_rules, value),
            _ => return None,
        })
    }
}

/// Stores a named value, or returns the accepted names.
fn set_named<T: Named>(
    slot: &mut T,
    value: &str,
) -> std::result::Result<(), &'static [&'static str]> {
    *slot = T::from_name(value).ok_or(T::NAMES)?;
    Ok(())
}

/// Stores a boolean value, or returns the accepted spellings.
fn set_flag(slot: &mut bool, value: &str) -> std::result::Result<(), &'static [&'static str]> {
    *slot = match value {
        "true" => true,
        "false" => false,
        _ => return Err(&["true", "false"]),
    };
    Ok(())
}

/// Sampling values a model family is published with.
#[derive(Clone, Copy, PartialEq, Debug, Default, serde::Serialize)]
pub struct Sampling {
    /// Temperature.
    pub temperature: Option<f64>,
    /// Nucleus sampling cutoff.
    pub top_p: Option<f64>,
    /// Top-k cutoff.
    pub top_k: Option<u32>,
}

impl Sampling {
    /// Returns the set parts as `temperature=1.0 top_p=0.95 top_k=64`, or `none`.
    fn describe(&self) -> String {
        let parts: Vec<String> = [
            self.temperature
                .map(|value| format!("temperature={value:?}")),
            self.top_p.map(|value| format!("top_p={value:?}")),
            self.top_k.map(|value| format!("top_k={value}")),
        ]
        .into_iter()
        .flatten()
        .collect();
        if parts.is_empty() {
            "none".to_owned()
        } else {
            parts.join(" ")
        }
    }
}

/// How a model family behaves.
#[derive(Clone, Copy, PartialEq, Debug, serde::Serialize)]
pub struct ModelTraits {
    /// The family.
    pub family: Family,
    /// The edit tool the model is trained to use.
    pub edit_tool: EditTool,
    /// The prompt overlay for the family.
    pub prompt_overlay: &'static str,
    /// The effort used when none is configured.
    pub default_effort: Effort,
    /// The efforts the model accepts.
    pub efforts: &'static [Effort],
    /// Published sampling values.
    pub sampling: Sampling,
    /// Whether the model reads images.
    pub images: Images,
    /// How a Claude model takes its thinking controls.
    pub claude_thinking: ClaudeThinking,
    /// Whether the model accepts a forced tool choice.
    pub forced_tool_choice: bool,
}

impl Default for ModelTraits {
    fn default() -> Self {
        Self {
            family: Family::Other,
            edit_tool: EditTool::Replace,
            prompt_overlay: Family::Other.as_str(),
            default_effort: Effort::Auto,
            efforts: ALL_EFFORTS,
            sampling: Sampling::default(),
            images: Images::Unknown,
            claude_thinking: ClaudeThinking::NotClaude,
            forced_tool_choice: true,
        }
    }
}

impl ModelTraits {
    /// Returns each trait's name and value, in declaration order.
    #[must_use]
    pub fn fields(&self) -> Vec<(&'static str, String)> {
        let edit_tool = match self.edit_tool {
            EditTool::Replace => "replace",
            EditTool::Patch => "patch",
        };
        vec![
            ("family", self.family.as_str().to_owned()),
            ("edit_tool", edit_tool.to_owned()),
            ("prompt_overlay", self.prompt_overlay.to_owned()),
            ("default_effort", self.default_effort.as_str().to_owned()),
            (
                "efforts",
                self.efforts
                    .iter()
                    .map(|effort| effort.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            ("sampling", self.sampling.describe()),
            ("images", self.images.as_str().to_owned()),
            ("claude_thinking", self.claude_thinking.as_str().to_owned()),
            ("forced_tool_choice", self.forced_tool_choice.to_string()),
        ]
    }
}

/// What a rule matches on.
enum Key {
    /// The canonical preset name.
    Preset(&'static str),
    /// The endpoint host.
    Host(&'static str),
    /// The model family.
    Family(Family),
    /// The endpoint host and a model id prefix.
    HostAndModelPrefix(&'static str, &'static str),
}

/// One named change to the endpoint settings.
struct EndpointRule {
    key: Key,
    apply: fn(&mut EndpointCompat),
}

/// What a resolution matches the rules against.
struct Target {
    preset: String,
    host: String,
    model: String,
    family: Family,
}

impl Key {
    fn matches(&self, target: &Target) -> bool {
        match self {
            Self::Preset(name) => target.preset == *name,
            Self::Host(host) => target.host == *host,
            Self::Family(family) => target.family == *family,
            Self::HostAndModelPrefix(host, prefix) => {
                target.host == *host && target.model.starts_with(prefix)
            }
        }
    }

    fn name(&self) -> String {
        match self {
            Self::Preset(name) => format!("preset {name}"),
            Self::Host(host) => format!("host {host}"),
            Self::Family(family) => format!("model family {}", family.as_str()),
            Self::HostAndModelPrefix(host, prefix) => format!("host {host} with model {prefix}"),
        }
    }
}

fn openai(compat: &mut EndpointCompat) {
    compat.system_role = SystemRole::Developer;
    compat.max_tokens_field = MaxTokensField::MaxCompletionTokens;
    compat.tool_call_id = CallIdShape::Max40;
    compat.prompt_cache_key = true;
}

fn deepseek(compat: &mut EndpointCompat) {
    compat.thinking_format = ThinkingFormat::DeepSeek;
    compat.reasoning_replay = ReasoningReplay::ReasoningContent;
    compat.reasoning_always = true;
}

fn openrouter(compat: &mut EndpointCompat) {
    compat.thinking_format = ThinkingFormat::OpenRouter;
    compat.reasoning_replay = ReasoningReplay::ReasoningDetails;
    compat.prompt_cache_key = true;
}

fn anthropic_markers(compat: &mut EndpointCompat) {
    compat.cache_control = CacheMarkers::Anthropic;
}

fn zai(compat: &mut EndpointCompat) {
    compat.thinking_format = ThinkingFormat::Zai;
    compat.reasoning_replay = ReasoningReplay::ReasoningContent;
}

fn moonshot(compat: &mut EndpointCompat) {
    compat.reasoning_replay = ReasoningReplay::ReasoningContent;
    compat.reasoning_always = true;
    compat.schema_rules = SchemaRules::Moonshot;
}

fn gemini_schemas(compat: &mut EndpointCompat) {
    compat.schema_rules = SchemaRules::Gemini;
}

fn minimax(compat: &mut EndpointCompat) {
    compat.thinking_format = ThinkingFormat::MiniMax;
    compat.reasoning_replay = ReasoningReplay::AsObserved;
}

fn cache_key(compat: &mut EndpointCompat) {
    compat.prompt_cache_key = true;
}

fn qwen(compat: &mut EndpointCompat) {
    compat.thinking_format = ThinkingFormat::QwenEnable;
}

fn chat_template(compat: &mut EndpointCompat) {
    compat.thinking_format = ThinkingFormat::ChatTemplate;
}

fn mistral(compat: &mut EndpointCompat) {
    compat.tool_call_id = CallIdShape::Alnum9;
    compat.assistant_after_tool_result = true;
    compat.thinking_format = ThinkingFormat::None;
}

fn claude(compat: &mut EndpointCompat) {
    compat.tool_call_id = CallIdShape::ClaudeSafe;
}

/// Builds one rule.
const fn on(key: Key, apply: fn(&mut EndpointCompat)) -> EndpointRule {
    EndpointRule { key, apply }
}

/// The endpoint rules, applied in order; a later rule overwrites an earlier one.
const ENDPOINT_RULES: &[EndpointRule] = &[
    on(Key::Host("api.openai.com"), openai),
    on(Key::Preset("deepseek"), deepseek),
    on(Key::Host("api.deepseek.com"), deepseek),
    on(Key::Preset("openrouter"), openrouter),
    on(Key::Host("openrouter.ai"), openrouter),
    on(
        Key::HostAndModelPrefix("openrouter.ai", "anthropic/"),
        anthropic_markers,
    ),
    on(Key::Preset("zai"), zai),
    on(Key::Host("api.z.ai"), zai),
    on(Key::Host("open.bigmodel.cn"), zai),
    on(Key::Preset("moonshot"), moonshot),
    on(Key::Host("api.moonshot.ai"), moonshot),
    on(Key::Host("api.moonshot.cn"), moonshot),
    on(Key::Preset("minimax"), minimax),
    on(Key::Host("api.minimax.io"), minimax),
    on(Key::Preset("xai"), cache_key),
    on(Key::Host("api.x.ai"), cache_key),
    on(
        Key::Host("generativelanguage.googleapis.com"),
        gemini_schemas,
    ),
    on(Key::Host("dashscope.aliyuncs.com"), qwen),
    on(Key::Host("dashscope-intl.aliyuncs.com"), qwen),
    on(Key::Preset("llamacpp"), chat_template),
    on(Key::Family(Family::Mistral), mistral),
    on(Key::Host("api.mistral.ai"), mistral),
    on(Key::Family(Family::Claude), claude),
];

/// Returns the canonical preset name for a provider name or alias.
///
/// A name that is neither a preset nor an alias is returned lower-cased.
#[must_use]
pub fn canonical_preset(provider: &str) -> String {
    let wanted = provider.trim().to_ascii_lowercase();
    crate::providers::lookup(&wanted).map_or(wanted, |entry| entry.name.to_owned())
}

/// Returns the family of a model id.
fn family_of(model: &str) -> Family {
    let id = model.to_ascii_lowercase();
    let bare = id.strip_prefix("openai/").unwrap_or(&id);
    let has = |needles: &[&str]| needles.iter().any(|needle| id.contains(needle));
    if has(&["claude"]) {
        Family::Claude
    } else if has(&["gpt-", "codex"]) || ["o1", "o3", "o4"].iter().any(|p| bare.starts_with(p)) {
        Family::Gpt
    } else if has(&["gemini"]) {
        Family::Gemini
    } else if has(&["deepseek"]) {
        Family::DeepSeek
    } else if has(&["kimi", "moonshot"]) {
        Family::Kimi
    } else if has(&["glm"]) {
        Family::Glm
    } else if has(&["qwen", "qwq"]) {
        Family::Qwen
    } else if has(&["minimax"]) {
        Family::MiniMax
    } else if has(&["grok"]) {
        Family::Grok
    } else if has(&[
        "mistral",
        "devstral",
        "codestral",
        "magistral",
        "pixtral",
        "mixtral",
        "ministral",
    ]) {
        Family::Mistral
    } else if has(&["llama"]) {
        Family::Llama
    } else {
        Family::Other
    }
}

/// The value one setting resolved to and the rule that set it.
#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize)]
pub struct Applied {
    /// The setting name.
    pub field: &'static str,
    /// Its final value.
    pub value: String,
    /// The rule that set the value, or `default`.
    pub rule: String,
}

/// The resolved settings for one endpoint and model, with provenance.
#[derive(Clone, PartialEq, Debug)]
pub struct Resolution {
    /// The endpoint settings.
    pub endpoint: EndpointCompat,
    /// The model traits.
    pub model: ModelTraits,
    /// One entry per field of both, the endpoint settings first.
    pub applied: Vec<Applied>,
}

impl Resolution {
    /// Returns the provenance of the endpoint settings.
    #[must_use]
    pub fn compat_rows(&self) -> &[Applied] {
        self.applied
            .get(..ENDPOINT_FIELDS.len())
            .unwrap_or_default()
    }

    /// Returns the provenance of the model traits.
    #[must_use]
    pub fn trait_rows(&self) -> &[Applied] {
        self.applied
            .get(ENDPOINT_FIELDS.len()..)
            .unwrap_or_default()
    }
}

/// Starts provenance with every field set by `default`.
fn defaults(fields: &[(&'static str, String)]) -> Vec<Applied> {
    fields
        .iter()
        .map(|(field, value)| Applied {
            field,
            value: value.clone(),
            rule: "default".to_owned(),
        })
        .collect()
}

/// Names `rule` on every field whose value changed.
fn record(applied: &mut [Applied], after: &[(&'static str, String)], rule: &str) {
    for (entry, (_, value)) in applied.iter_mut().zip(after) {
        if entry.value != *value {
            entry.value.clone_from(value);
            rule.clone_into(&mut entry.rule);
        }
    }
}

/// Returns the traits of a model and the rule behind each.
fn traits_for(model: &str) -> (ModelTraits, Vec<Applied>) {
    let id = model.to_ascii_lowercase();
    let family = family_of(&id);
    let mut traits = ModelTraits::default();
    let mut applied = defaults(&traits.fields());

    traits.family = family;
    traits.prompt_overlay = family.as_str();
    match family {
        Family::Gpt => {
            if ["gpt-5", "gpt-6", "codex", "gpt-oss"]
                .iter()
                .any(|marker| id.contains(marker))
            {
                traits.edit_tool = EditTool::Patch;
            }
            traits.images = Images::Yes;
        }
        Family::Claude => traits.images = Images::Yes,
        Family::Gemini => {
            traits.images = Images::Yes;
            traits.sampling = Sampling {
                temperature: Some(1.0),
                top_p: Some(0.95),
                top_k: Some(64),
            };
        }
        Family::DeepSeek => traits.images = Images::No,
        Family::Qwen => traits.sampling.temperature = Some(0.55),
        Family::Glm => {
            if ["glm-4.6", "glm-4.7", "glm-5"]
                .iter()
                .any(|version| id.contains(version))
            {
                traits.sampling.temperature = Some(1.0);
            }
        }
        Family::MiniMax => {
            traits.sampling.temperature = Some(1.0);
            traits.sampling.top_p = Some(0.95);
        }
        Family::Kimi | Family::Grok | Family::Mistral | Family::Llama | Family::Other => {}
    }
    record(
        &mut applied,
        &traits.fields(),
        &format!("model family {}", family.as_str()),
    );
    if family == Family::Claude {
        let rule = claude_controls(&id, &mut traits);
        record(&mut applied, &traits.fields(), &rule);
    }
    (traits, applied)
}

/// The tier of a Claude model.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    /// Fable.
    Fable,
    /// Mythos.
    Mythos,
    /// Opus.
    Opus,
    /// Sonnet.
    Sonnet,
    /// Haiku.
    Haiku,
}

impl Tier {
    /// Every tier.
    const ALL: [Self; 5] = [
        Self::Fable,
        Self::Mythos,
        Self::Opus,
        Self::Sonnet,
        Self::Haiku,
    ];

    /// Returns the name used in model ids.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fable => "fable",
            Self::Mythos => "mythos",
            Self::Opus => "opus",
            Self::Sonnet => "sonnet",
            Self::Haiku => "haiku",
        }
    }
}

/// Returns the tier and version of a Claude model id.
///
/// The version is the first one or two numbers next to the tier word, after it
/// (`claude-opus-5-5`) or, when none follows, before it
/// (`claude-3-5-sonnet-20241022`). A number of more than two digits is a date
/// and ends the version.
#[must_use]
pub fn claude_version(model: &str) -> Option<(Tier, u32, u32)> {
    let id = model.to_ascii_lowercase();
    let id = id
        .strip_prefix("anthropic/")
        .or_else(|| id.strip_prefix("anthropic."))
        .unwrap_or(&id);
    let tokens: Vec<&str> = id
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();
    let (at, tier) = tokens.iter().enumerate().find_map(|(at, token)| {
        Tier::ALL
            .into_iter()
            .find(|tier| tier.as_str() == *token)
            .map(|tier| (at, tier))
    })?;
    let number = |token: &&str| -> Option<u32> {
        if token.len() <= 2 && token.bytes().all(|byte| byte.is_ascii_digit()) {
            token.parse().ok()
        } else {
            None
        }
    };
    let mut version: Vec<u32> = tokens
        .get(at.saturating_add(1)..)
        .unwrap_or_default()
        .iter()
        .map_while(number)
        .take(2)
        .collect();
    if version.is_empty() {
        version = tokens
            .get(..at)
            .unwrap_or_default()
            .iter()
            .rev()
            .map_while(number)
            .take(2)
            .collect();
        version.reverse();
    }
    let major = *version.first()?;
    let minor = version.get(1).copied().unwrap_or(0);
    Some((tier, major, minor))
}

/// Every effort except `xhigh`.
const EFFORTS_WITHOUT_XHIGH: &[Effort] = &[
    Effort::Auto,
    Effort::None,
    Effort::Minimal,
    Effort::Low,
    Effort::Medium,
    Effort::High,
    Effort::Max,
];

/// Sets the thinking controls of a Claude model and returns the rule name.
fn claude_controls(id: &str, traits: &mut ModelTraits) -> String {
    let Some((tier, major, minor)) = claude_version(id) else {
        traits.claude_thinking = ClaudeThinking::AdaptiveAlwaysOn;
        traits.forced_tool_choice = false;
        return "model claude".to_owned();
    };
    let version = (major, minor);
    let (thinking, forced, efforts) = match tier {
        Tier::Fable | Tier::Mythos if version >= (5, 1) => {
            (ClaudeThinking::AdaptiveAlwaysOn, false, ALL_EFFORTS)
        }
        Tier::Fable | Tier::Mythos => (ClaudeThinking::AdaptiveAlwaysOn, true, ALL_EFFORTS),
        Tier::Opus if version >= (5, 5) => (ClaudeThinking::AdaptiveAlwaysOn, false, ALL_EFFORTS),
        Tier::Sonnet if version >= (5, 5) => {
            (ClaudeThinking::AdaptiveBetweenTools, false, ALL_EFFORTS)
        }
        Tier::Opus | Tier::Sonnet | Tier::Haiku if major >= 5 => {
            (ClaudeThinking::Adaptive, true, ALL_EFFORTS)
        }
        Tier::Opus if version == (4, 7) || version == (4, 8) => {
            (ClaudeThinking::Adaptive, true, ALL_EFFORTS)
        }
        Tier::Opus | Tier::Sonnet if version == (4, 6) => {
            (ClaudeThinking::Adaptive, true, EFFORTS_WITHOUT_XHIGH)
        }
        _ => (ClaudeThinking::Budget, true, ALL_EFFORTS),
    };
    traits.claude_thinking = thinking;
    traits.forced_tool_choice = forced;
    traits.efforts = efforts;
    format!("model {} {major}.{minor}", tier.as_str())
}

/// Returns the host of a base URL, lower-cased.
fn host_from_url(base_url: &str) -> String {
    let lower = base_url.trim().to_ascii_lowercase();
    let rest = lower
        .split_once("://")
        .map_or(lower.as_str(), |(_, rest)| rest);
    crate::transport::host_of(rest).to_owned()
}

/// Resolves the settings for a provider, endpoint and model, naming the rule
/// behind every value.
#[must_use]
pub fn explain(provider: &str, base_url: Option<&str>, model: &str) -> Resolution {
    let target = Target {
        preset: canonical_preset(provider),
        host: base_url.map(host_from_url).unwrap_or_default(),
        model: model.to_ascii_lowercase(),
        family: family_of(model),
    };
    let mut endpoint = EndpointCompat::default();
    let mut applied = defaults(&endpoint.fields());
    for rule in ENDPOINT_RULES {
        if rule.key.matches(&target) {
            (rule.apply)(&mut endpoint);
            record(&mut applied, &endpoint.fields(), &rule.key.name());
        }
    }
    let (model_traits, traits_applied) = traits_for(model);
    applied.extend(traits_applied);
    Resolution {
        endpoint,
        model: model_traits,
        applied,
    }
}

/// Resolves the settings for a provider, endpoint and model.
#[must_use]
pub fn resolve(
    provider: &str,
    base_url: Option<&str>,
    model: &str,
) -> (EndpointCompat, ModelTraits) {
    let resolution = explain(provider, base_url, model);
    (resolution.endpoint, resolution.model)
}

/// Applies the user's `[providers.<name>.compat]` settings over a resolution.
pub fn apply_overrides(
    resolution: &mut Resolution,
    name: &str,
    overrides: &BTreeMap<String, String>,
) -> Result<()> {
    let rule = format!("user [providers.{name}.compat]");
    for (key, value) in overrides {
        let field = format!("providers.{name}.compat.{key}");
        let Some(outcome) = resolution.endpoint.set(key, value) else {
            let closest = ENDPOINT_FIELDS
                .iter()
                .map(|candidate| (distance(key, candidate), *candidate))
                .min();
            let hint = match closest {
                Some((gap, candidate)) if gap <= 3 => format!(
                    "did you mean `{candidate}`? run `rune models --explain` to see every setting"
                ),
                _ => "run `rune models --explain` to see every setting".to_owned(),
            };
            return Err(RuneError::invalid_field(
                field,
                format!("`{key}` is not a compatibility setting"),
            )
            .with_hint(hint));
        };
        if let Err(accepted) = outcome {
            return Err(RuneError::invalid_field(
                field,
                format!("`{value}` is not a value of `{key}`"),
            )
            .with_hint(format!("use one of: {}", accepted.join(", "))));
        }
        if let Some(entry) = resolution
            .applied
            .iter_mut()
            .find(|entry| entry.field == key)
        {
            entry.value.clone_from(value);
            rule.clone_into(&mut entry.rule);
        }
    }
    Ok(())
}

/// Returns the edit distance between two names.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, left) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i.saturating_add(1);
        for (j, right) in b.iter().enumerate() {
            let next = j.saturating_add(1);
            let above = row[next];
            let substitution = diagonal.saturating_add(usize::from(left != *right));
            row[next] = above
                .saturating_add(1)
                .min(row[j].saturating_add(1))
                .min(substitution);
            diagonal = above;
        }
    }
    row[b.len()]
}

/// The edit tool a model is trained to use. `Patch` for GPT-family ids that
/// contain `gpt-5`, `gpt-6`, `codex` or `gpt-oss`; otherwise `Replace`.
#[must_use]
pub fn edit_tool_for(model_id: &str) -> EditTool {
    traits_for(model_id).0.edit_tool
}

/// Applies the user's setting over the model's default.
#[must_use]
pub fn edit_tool(setting: EditToolSetting, model_id: &str) -> EditTool {
    match setting {
        EditToolSetting::Auto => edit_tool_for(model_id),
        EditToolSetting::Replace => EditTool::Replace,
        EditToolSetting::Patch => EditTool::Patch,
    }
}

/// Returns a tool call identifier in the shape an endpoint accepts.
///
/// An identifier that already fits is returned unchanged. The mapping depends
/// on the identifier alone, so a call and its result always agree and a call
/// maps the same way on every request of a session.
#[must_use]
pub fn shape_call_id(id: &str, shape: CallIdShape) -> Cow<'_, str> {
    let safe = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    match shape {
        CallIdShape::Any => Cow::Borrowed(id),
        CallIdShape::ClaudeSafe if id.chars().all(safe) => Cow::Borrowed(id),
        CallIdShape::ClaudeSafe => {
            Cow::Owned(id.chars().map(|c| if safe(c) { c } else { '_' }).collect())
        }
        CallIdShape::Max40 if id.len() <= 40 => Cow::Borrowed(id),
        CallIdShape::Max40 => {
            let mut cut = 31;
            while !id.is_char_boundary(cut) {
                cut = cut.saturating_sub(1);
            }
            let mut shaped = id.get(..cut).unwrap_or_default().to_owned();
            shaped.push('_');
            for byte in Sha256::digest(id.as_bytes()).iter().take(4) {
                let _ = write!(shaped, "{byte:02x}");
            }
            Cow::Owned(shaped)
        }
        CallIdShape::Alnum9 if id.len() == 9 && id.bytes().all(|b| b.is_ascii_alphanumeric()) => {
            Cow::Borrowed(id)
        }
        CallIdShape::Alnum9 => {
            const DIGITS: &[u8; 62] =
                b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
            let mut number = Sha256::digest(id.as_bytes())
                .iter()
                .take(8)
                .fold(0_u64, |acc, byte| (acc << 8) | u64::from(*byte));
            let mut reversed = Vec::new();
            loop {
                reversed.push(DIGITS[(number % 62) as usize]);
                number /= 62;
                if number == 0 {
                    break;
                }
            }
            while reversed.len() < 9 {
                reversed.push(b'0');
            }
            Cow::Owned(
                reversed
                    .iter()
                    .rev()
                    .take(9)
                    .map(|byte| char::from(*byte))
                    .collect(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_id_shapes_are_stable_and_distinct() {
        assert_eq!(shape_call_id("call:1/x", CallIdShape::Any), "call:1/x");
        assert_eq!(
            shape_call_id("toolu_01-Ab", CallIdShape::ClaudeSafe),
            "toolu_01-Ab"
        );
        assert_eq!(
            shape_call_id("call:1/x.y", CallIdShape::ClaudeSafe),
            "call_1_x_y"
        );

        let short = "a".repeat(40);
        assert_eq!(shape_call_id(&short, CallIdShape::Max40), short.as_str());
        // A multi-byte character starting at byte 30 spans byte 31.
        let wide = format!("{}é{}", "a".repeat(30), "b".repeat(9));
        assert_eq!(wide.len(), 41);
        let shaped = shape_call_id(&wide, CallIdShape::Max40);
        assert_eq!(shaped.len(), 39);
        assert!(
            shaped.starts_with(&format!("{}_", "a".repeat(30))),
            "{shaped}"
        );
        let prefix = "p".repeat(31);
        let first = format!("{prefix}{}", "x".repeat(19));
        let second = format!("{prefix}{}", "y".repeat(19));
        let (first, second) = (
            shape_call_id(&first, CallIdShape::Max40),
            shape_call_id(&second, CallIdShape::Max40),
        );
        assert_eq!(first.len(), 40);
        assert_eq!(second.len(), 40);
        assert_ne!(first, second);

        assert_eq!(shape_call_id("Ab3dE6gH9", CallIdShape::Alnum9), "Ab3dE6gH9");
        let nine = shape_call_id("call_0123456789", CallIdShape::Alnum9);
        assert_eq!(nine.len(), 9);
        assert!(nine.bytes().all(|b| b.is_ascii_alphanumeric()), "{nine}");
        assert_ne!(nine, shape_call_id("call_0123456788", CallIdShape::Alnum9));

        for shape in [
            CallIdShape::ClaudeSafe,
            CallIdShape::Max40,
            CallIdShape::Alnum9,
        ] {
            let id = "call.with:odd/characters-and-a-long-tail-0123456789";
            assert_eq!(shape_call_id(id, shape), shape_call_id(id, shape));
        }
    }

    fn value(resolution: &Resolution, field: &str) -> String {
        resolution
            .applied
            .iter()
            .find(|entry| entry.field == field)
            .map_or_else(|| panic!("no field {field}"), |entry| entry.value.clone())
    }

    fn rule(resolution: &Resolution, field: &str) -> String {
        resolution
            .applied
            .iter()
            .find(|entry| entry.field == field)
            .map_or_else(|| panic!("no field {field}"), |entry| entry.rule.clone())
    }

    const OPENAI: Option<&str> = Some("https://api.openai.com/v1");
    const ROUTER: Option<&str> = Some("https://openrouter.ai/api/v1");
    const GENERIC: Option<&str> = Some("https://example.test/v1");

    /// `(provider, base_url, model, field, expected value)`.
    #[rustfmt::skip]
    const ROWS: &[(&str, Option<&str>, &str, &str, &str)] = &[
        ("chat_completions", OPENAI, "gpt-5", "system_role", "developer"),
        ("chat_completions", OPENAI, "gpt-5", "max_tokens_field", "max_completion_tokens"),
        ("chat_completions", OPENAI, "gpt-5", "tool_call_id", "max40"),
        ("chat_completions", OPENAI, "gpt-5", "prompt_cache_key", "true"),
        ("deepseek", None, "deepseek-flash", "thinking_format", "deepseek"),
        ("deepseek", None, "deepseek-flash", "reasoning_replay", "reasoning_content"),
        ("chat_completions", Some("https://api.deepseek.com"), "deepseek-flash", "reasoning_always", "true"),
        ("openrouter", None, "qwen/qwen3-coder", "thinking_format", "openrouter"),
        ("chat_completions", ROUTER, "deepseek/deepseek-flash", "reasoning_replay", "reasoning_details"),
        ("chat_completions", ROUTER, "deepseek/deepseek-flash", "prompt_cache_key", "true"),
        ("chat_completions", ROUTER, "anthropic/claude-sonnet-5-5", "cache_control", "anthropic"),
        ("chat_completions", ROUTER, "deepseek/deepseek-flash", "cache_control", "none"),
        ("zai", None, "glm-5", "thinking_format", "zai"),
        ("glm", None, "glm-5.3", "reasoning_replay", "reasoning_content"),
        ("chat_completions", Some("https://api.z.ai/api/paas/v4"), "glm-5", "thinking_format", "zai"),
        ("chat_completions", Some("https://open.bigmodel.cn/api/paas/v4"), "glm-5", "thinking_format", "zai"),
        ("moonshot", None, "kimi-k3", "reasoning_replay", "reasoning_content"),
        ("kimi", None, "kimi-k3", "reasoning_always", "true"),
        ("chat_completions", Some("https://api.moonshot.ai/v1"), "kimi-k3", "reasoning_always", "true"),
        ("chat_completions", Some("https://api.moonshot.cn/v1"), "kimi-k3", "reasoning_replay", "reasoning_content"),
        ("minimax", None, "minimax-m3", "thinking_format", "minimax"),
        ("chat_completions", Some("https://api.minimax.io/v1"), "minimax-m3", "reasoning_replay", "as_observed"),
        ("xai", None, "grok-5", "prompt_cache_key", "true"),
        ("chat_completions", Some("https://api.x.ai/v1"), "grok-5", "prompt_cache_key", "true"),
        ("chat_completions", Some("https://dashscope.aliyuncs.com/compatible-mode/v1"), "qwen3-coder", "thinking_format", "qwen_enable"),
        ("chat_completions", Some("https://dashscope-intl.aliyuncs.com/compatible-mode/v1"), "qwen3-coder", "thinking_format", "qwen_enable"),
        ("llamacpp", None, "qwen3-coder", "thinking_format", "chat_template"),
        ("mistral", None, "devstral-medium", "tool_call_id", "alnum9"),
        ("chat_completions", Some("https://api.mistral.ai/v1"), "my-model", "assistant_after_tool_result", "true"),
        ("chat_completions", Some("https://api.mistral.ai/v1"), "my-model", "thinking_format", "none"),
        ("chat_completions", GENERIC, "mistral-large-latest", "tool_call_id", "alnum9"),
        ("chat_completions", GENERIC, "claude-sonnet-5-5", "tool_call_id", "claude_safe"),
        ("groq", Some("https://api.groq.com/openai/v1"), "llama-4", "thinking_format", "reasoning_effort"),
        ("vercel", Some("https://ai-gateway.vercel.sh/v1"), "my-model", "prompt_cache_key", "false"),
        ("lmstudio", Some("http://127.0.0.1:1234/v1"), "my-model", "thinking_format", "reasoning_effort"),
        ("ollama", Some("http://localhost:11434/v1"), "my-model", "reasoning_replay", "none"),
        ("chat_completions", GENERIC, "my-model", "tool_call_id", "any"),
        ("chat_completions", GENERIC, "claude-opus-5-5", "family", "claude"),
        ("chat_completions", GENERIC, "claude-opus-5-5", "claude_thinking", "adaptive_always_on"),
        ("chat_completions", GENERIC, "claude-haiku-4-5", "claude_thinking", "budget"),
        ("chat_completions", GENERIC, "gpt-5.1-codex", "family", "gpt"),
        ("chat_completions", GENERIC, "gpt-5.1-codex", "edit_tool", "patch"),
        ("chat_completions", GENERIC, "openai/o3-mini", "family", "gpt"),
        ("chat_completions", GENERIC, "o4-mini", "edit_tool", "replace"),
        ("chat_completions", GENERIC, "gpt-4.1", "images", "yes"),
        ("chat_completions", GENERIC, "gemini-3.5-pro", "family", "gemini"),
        ("chat_completions", GENERIC, "gemini-3.5-pro", "sampling", "temperature=1.0 top_p=0.95 top_k=64"),
        ("chat_completions", GENERIC, "deepseek-flash", "family", "deepseek"),
        ("chat_completions", GENERIC, "deepseek-flash", "images", "no"),
        ("chat_completions", GENERIC, "moonshot-v1-8k", "family", "kimi"),
        ("chat_completions", GENERIC, "kimi-k3", "sampling", "none"),
        ("chat_completions", GENERIC, "glm-4.7", "family", "glm"),
        ("chat_completions", GENERIC, "glm-4.7", "sampling", "temperature=1.0"),
        ("chat_completions", GENERIC, "glm-4.5", "sampling", "none"),
        ("chat_completions", GENERIC, "qwq-32b", "family", "qwen"),
        ("chat_completions", GENERIC, "qwen3-coder", "sampling", "temperature=0.55"),
        ("chat_completions", GENERIC, "MiniMax-M3", "family", "minimax"),
        ("chat_completions", GENERIC, "minimax-m3", "sampling", "temperature=1.0 top_p=0.95"),
        ("chat_completions", GENERIC, "grok-5", "family", "grok"),
        ("chat_completions", GENERIC, "codestral-latest", "family", "mistral"),
        ("chat_completions", GENERIC, "ministral-8b", "family", "mistral"),
        ("chat_completions", GENERIC, "llama-4-maverick", "family", "llama"),
        ("chat_completions", GENERIC, "my-model", "family", "other"),
        ("chat_completions", GENERIC, "my-model", "prompt_overlay", "other"),
        ("chat_completions", GENERIC, "qwen3-coder", "prompt_overlay", "qwen"),
    ];

    #[test]
    fn the_table_resolves_every_preset_and_family() {
        for (provider, base_url, model, field, expected) in ROWS {
            let resolution = explain(provider, *base_url, model);
            assert_eq!(
                value(&resolution, field),
                *expected,
                "{provider} {base_url:?} {model} {field}"
            );
        }
        assert_eq!(
            rule(&explain("glm", None, "glm-5.3"), "thinking_format"),
            "preset zai"
        );
        assert_eq!(
            rule(
                &explain(
                    "chat_completions",
                    Some("https://example.test/v1"),
                    "mistral-large-latest"
                ),
                "tool_call_id"
            ),
            "model family mistral"
        );
    }

    #[test]
    fn claude_versions_parse_from_every_id_form() {
        let cases = [
            ("claude-opus-5-5", Some((Tier::Opus, 5, 5))),
            ("claude-sonnet-5-5", Some((Tier::Sonnet, 5, 5))),
            ("claude-opus-4-6", Some((Tier::Opus, 4, 6))),
            ("claude-haiku-4-5", Some((Tier::Haiku, 4, 5))),
            ("anthropic/claude-sonnet-5.5", Some((Tier::Sonnet, 5, 5))),
            ("claude-opus-4-5-20251101", Some((Tier::Opus, 4, 5))),
            ("claude-fable-5-1", Some((Tier::Fable, 5, 1))),
            ("claude-mythos-5-1", Some((Tier::Mythos, 5, 1))),
            ("claude-sonnet-5", Some((Tier::Sonnet, 5, 0))),
            ("claude-3-5-sonnet-20241022", Some((Tier::Sonnet, 3, 5))),
            ("anthropic.claude-opus-4-7-v1:0", Some((Tier::Opus, 4, 7))),
            ("claude-sonnet-4@20250514", Some((Tier::Sonnet, 4, 0))),
            ("claude-mythos-preview", None),
            ("claude", None),
        ];
        for (id, expected) in cases {
            assert_eq!(claude_version(id), expected, "{id}");
        }
        let resolution = explain("anthropic", None, "claude-sonnet-5-5");
        assert_eq!(rule(&resolution, "claude_thinking"), "model sonnet 5.5");
        assert_eq!(rule(&resolution, "forced_tool_choice"), "model sonnet 5.5");
        assert_eq!(
            rule(
                &explain("anthropic", None, "claude-mythos-preview"),
                "claude_thinking"
            ),
            "model claude"
        );
    }

    #[test]
    fn an_unknown_endpoint_keeps_the_shapes_sent_today() {
        let (compat, traits) = resolve(
            "chat_completions",
            Some("https://example.test/v1"),
            "my-model",
        );
        assert_eq!(compat, EndpointCompat::default());
        assert_eq!(traits.family, Family::Other);
        assert_eq!(traits, ModelTraits::default());
    }

    #[test]
    fn hosts_match_exactly_and_ignore_case() {
        let (spoofed, _) = resolve(
            "chat_completions",
            Some("https://api.deepseek.com.evil.test"),
            "my-model",
        );
        assert_eq!(spoofed, EndpointCompat::default());
        let (upper, _) = resolve(
            "chat_completions",
            Some("HTTPS://API.DEEPSEEK.COM/"),
            "my-model",
        );
        assert_eq!(upper.thinking_format, ThinkingFormat::DeepSeek);
    }

    #[test]
    fn every_field_names_the_rule_that_set_it() {
        let resolution = explain(
            "chat_completions",
            Some("https://api.deepseek.com"),
            "deepseek-flash",
        );
        let expected: Vec<&str> = ENDPOINT_FIELDS
            .iter()
            .copied()
            .chain(
                ModelTraits::default()
                    .fields()
                    .iter()
                    .map(|(name, _)| *name),
            )
            .collect();
        let named: Vec<&str> = resolution.applied.iter().map(|entry| entry.field).collect();
        assert_eq!(named, expected);
        assert_eq!(resolution.compat_rows().len(), 17);
        assert_eq!(resolution.trait_rows().len(), 9);
        assert!(
            resolution
                .applied
                .iter()
                .all(|entry| !entry.rule.is_empty())
        );
        assert_eq!(
            rule(&resolution, "thinking_format"),
            "host api.deepseek.com"
        );
        assert_eq!(rule(&resolution, "stream_usage"), "default");
        assert_eq!(rule(&resolution, "images"), "model family deepseek");
    }

    #[test]
    fn every_name_reads_back_as_its_value() {
        for name in ThinkingFormat::NAMES {
            let parsed = ThinkingFormat::parse(name).expect("known name");
            assert_eq!(parsed.as_str(), *name);
            assert_eq!(serde_json::to_value(parsed).expect("json"), *name);
        }
        assert_eq!(CallIdShape::Alnum9.as_str(), "alnum9");
        assert_eq!(CallIdShape::ClaudeSafe.as_str(), "claude_safe");
        assert_eq!(ThinkingFormat::parse("open_router"), None);
    }

    #[test]
    fn an_override_is_applied_and_named() {
        let mut resolution = explain("deepseek", None, "deepseek-flash");
        assert_eq!(rule(&resolution, "thinking_format"), "preset deepseek");
        let overrides = BTreeMap::from([
            ("thinking_format".to_owned(), "none".to_owned()),
            ("strict_schema".to_owned(), "true".to_owned()),
        ]);
        apply_overrides(&mut resolution, "deepseek", &overrides).expect("valid overrides");
        assert_eq!(resolution.endpoint.thinking_format, ThinkingFormat::None);
        assert!(resolution.endpoint.strict_schema);
        assert_eq!(value(&resolution, "thinking_format"), "none");
        assert_eq!(
            rule(&resolution, "thinking_format"),
            "user [providers.deepseek.compat]"
        );
        assert_eq!(rule(&resolution, "reasoning_replay"), "preset deepseek");
    }

    #[test]
    fn an_unknown_override_suggests_the_closest_name() {
        let mut resolution = explain("zai", None, "glm-5");
        let overrides = BTreeMap::from([("thinking_formt".to_owned(), "zai".to_owned())]);
        let error = apply_overrides(&mut resolution, "zai", &overrides).expect_err("unknown key");
        assert_eq!(
            error.message(),
            "`thinking_formt` is not a compatibility setting"
        );
        assert_eq!(
            error.hint(),
            Some(
                "did you mean `thinking_format`? run `rune models --explain` to see every setting"
            )
        );

        let overrides = BTreeMap::from([("colour".to_owned(), "red".to_owned())]);
        let error = apply_overrides(&mut resolution, "zai", &overrides).expect_err("unknown key");
        assert_eq!(
            error.hint(),
            Some("run `rune models --explain` to see every setting")
        );
    }

    #[test]
    fn an_override_value_outside_the_field_is_refused() {
        let mut resolution = explain("moonshot", None, "kimi-k3");
        let overrides = BTreeMap::from([("reasoning_always".to_owned(), "yes".to_owned())]);
        let error =
            apply_overrides(&mut resolution, "moonshot", &overrides).expect_err("bad value");
        assert_eq!(
            error.message(),
            "`yes` is not a value of `reasoning_always`"
        );
        assert_eq!(error.hint(), Some("use one of: true, false"));

        let overrides = BTreeMap::from([("tool_call_id".to_owned(), "short".to_owned())]);
        let error =
            apply_overrides(&mut resolution, "moonshot", &overrides).expect_err("bad value");
        assert_eq!(
            error.hint(),
            Some("use one of: any, alnum9, max40, claude_safe")
        );
    }

    #[test]
    fn edit_tool_follows_the_model_id() {
        for (id, expected) in [
            ("gpt-5", EditTool::Patch),
            ("gpt-5.1-codex", EditTool::Patch),
            ("GPT-6-Luna", EditTool::Patch),
            ("openai/gpt-oss-120b", EditTool::Patch),
            ("codex-mini-latest", EditTool::Patch),
            ("gpt-4.1", EditTool::Replace),
            ("claude-opus-5-5", EditTool::Replace),
            ("deepseek-chat", EditTool::Replace),
            ("", EditTool::Replace),
        ] {
            assert_eq!(edit_tool_for(id), expected, "{id}");
        }
    }

    #[test]
    fn the_setting_overrides_the_model() {
        assert_eq!(
            edit_tool(EditToolSetting::Replace, "gpt-5"),
            EditTool::Replace
        );
        assert_eq!(
            edit_tool(EditToolSetting::Patch, "claude-opus-5-5"),
            EditTool::Patch
        );
        assert_eq!(edit_tool(EditToolSetting::Auto, "gpt-5"), EditTool::Patch);
    }
}
