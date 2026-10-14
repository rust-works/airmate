//! The protocol-neutral intermediate representation (IR).
//!
//! The IR is a superset of what the supported protocols can express, so no inbound feature is lost
//! before the outbound adapter decides what it can send. With N inbound and M outbound protocols
//! this costs N + M adapters instead of N x M translators.

use serde_json::Value as Json;

/// The wire protocols airmate speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protocol {
    /// Anthropic Messages (`/v1/messages`).
    Anthropic,
    /// OpenAI Chat Completions (`/v1/chat/completions`).
    Chat,
    /// OpenAI Responses (`/v1/responses`).
    Responses,
    /// Google Gemini `generateContent`.
    Gemini,
    /// AWS Bedrock Converse.
    Bedrock,
}

impl Protocol {
    /// The name used in configuration and logs.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Chat => "chat",
            Self::Responses => "responses",
            Self::Gemini => "gemini",
            Self::Bedrock => "bedrock",
        }
    }

    /// Parses a configuration name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "anthropic" => Self::Anthropic,
            "chat" => Self::Chat,
            "responses" => Self::Responses,
            "gemini" => Self::Gemini,
            "bedrock" => Self::Bedrock,
            _ => return None,
        })
    }
}

/// A reasoning effort level, ordered from least to most.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Effort {
    /// `low`
    Low,
    /// `medium`
    Medium,
    /// `high`
    High,
    /// `xhigh`
    XHigh,
    /// `max`
    Max,
}

impl Effort {
    /// Every level, ascending.
    pub const ALL: [Effort; 5] = [Self::Low, Self::Medium, Self::High, Self::XHigh, Self::Max];

    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Parses a wire name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.as_str() == name)
    }

    /// The highest of `accepted` that is not above `self`, or the lowest accepted level when every
    /// accepted level is above `self`. Levels are clamped *down* to what the target accepts.
    #[must_use]
    pub fn clamp_down(self, accepted: &[Effort]) -> Option<Effort> {
        accepted
            .iter()
            .copied()
            .filter(|e| *e <= self)
            .max()
            .or_else(|| accepted.iter().copied().min())
    }

    /// Buckets a thinking token budget into an effort level.
    #[must_use]
    pub fn from_budget(budget_tokens: u32) -> Effort {
        match budget_tokens {
            0..=2_047 => Self::Low,
            2_048..=8_191 => Self::Medium,
            8_192..=24_575 => Self::High,
            24_576..=49_151 => Self::XHigh,
            _ => Self::Max,
        }
    }
}

/// How reasoning text is shown to the client (`thinking.display`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReasoningDisplay {
    /// Reasoning is summarised (the default).
    #[default]
    Summarized,
    /// Reasoning is returned as empty blocks.
    Omitted,
    /// Reasoning is streamed as it happens.
    Updates,
}

impl ReasoningDisplay {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Summarized => "summarized",
            Self::Omitted => "omitted",
            Self::Updates => "updates",
        }
    }
}

/// The client's reasoning request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReasoningMode {
    /// The client said nothing about reasoning.
    #[default]
    Unspecified,
    /// The client explicitly disabled reasoning.
    Off,
    /// Reasoning on, optionally with a token budget.
    On {
        /// The requested thinking budget.
        budget_tokens: Option<u32>,
    },
    /// The model decides (`adaptive`, `between_tools`).
    Adaptive,
}

/// Reasoning controls on a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Reasoning {
    /// Whether and how reasoning was requested.
    pub mode: ReasoningMode,
    /// An explicit effort, from `output_config.effort` or the OpenAI equivalents.
    pub effort: Option<Effort>,
    /// How reasoning is displayed.
    pub display: ReasoningDisplay,
}

impl Reasoning {
    /// Whether the client asked for reasoning to be returned.
    #[must_use]
    pub fn wanted(&self) -> bool {
        matches!(
            self.mode,
            ReasoningMode::On { .. } | ReasoningMode::Adaptive
        )
    }
}

/// Sampling parameters.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Sampling {
    /// `temperature`
    pub temperature: Option<f64>,
    /// `top_p`
    pub top_p: Option<f64>,
    /// `top_k`
    pub top_k: Option<u32>,
    /// `seed`
    pub seed: Option<i64>,
    /// `frequency_penalty`
    pub frequency_penalty: Option<f64>,
    /// `presence_penalty`
    pub presence_penalty: Option<f64>,
}

/// Which tool the model may call.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ToolChoiceKind {
    /// The model decides.
    #[default]
    Auto,
    /// The model must call some tool.
    Required,
    /// The model must call this tool.
    Named(String),
    /// The model must not call a tool.
    None,
}

/// Tool selection plus the parallelism switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolChoice {
    /// Which tool.
    pub kind: ToolChoiceKind,
    /// Whether parallel tool calls are allowed (`false` for `disable_parallel_tool_use`).
    pub parallel: bool,
}

impl Default for ToolChoice {
    fn default() -> Self {
        Self {
            kind: ToolChoiceKind::Auto,
            parallel: true,
        }
    }
}

/// A function tool definition.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    /// The tool name as the client sent it.
    pub name: String,
    /// The description, with any input examples appended.
    pub description: Option<String>,
    /// The JSON Schema of the input.
    pub input_schema: Json,
    /// Whether the client asked for strict schema adherence.
    pub strict: Option<bool>,
}

/// The requested response shape.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum ResponseFormat {
    /// Free text.
    #[default]
    Text,
    /// Output constrained by a JSON Schema.
    JsonSchema {
        /// The schema.
        schema: Json,
        /// Strict adherence where the upstream supports it.
        strict: bool,
    },
}

/// Where a system message sits: `after_turns` is how many turns precede it (0 is the leading system
/// prompt).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemPart {
    /// The text.
    pub text: String,
    /// The number of turns that precede this part.
    pub after_turns: usize,
    /// A per-turn effort carried by a mid-conversation system message.
    pub effort: Option<Effort>,
}

/// Where an image comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSource {
    /// Inline base64 data.
    Base64 {
        /// The media type, e.g. `image/png`.
        media_type: String,
        /// The base64 payload.
        data: String,
    },
    /// A URL passed to the upstream, never fetched by the gateway.
    Url(String),
}

/// Where a document comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocumentSource {
    /// Plain text.
    Text(String),
    /// A URL.
    Url(String),
    /// A base64 PDF.
    Pdf(String),
    /// A file id.
    File(String),
}

/// The upstream that produced a piece of reasoning.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Origin {
    /// The upstream name.
    pub upstream: String,
    /// The protocol family of the upstream.
    pub protocol: Protocol,
}

/// A part of a turn.
#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    /// Text.
    Text(String),
    /// An image.
    Image(ImageSource),
    /// A document.
    Document(DocumentSource),
    /// A tool call made by the assistant.
    ToolCall {
        /// The call id.
        id: String,
        /// The tool name as the client knows it.
        name: String,
        /// The decoded arguments.
        input: Json,
    },
    /// Reasoning produced by the assistant.
    Reasoning {
        /// The reasoning text (empty when hidden).
        text: String,
        /// How it was displayed.
        display: ReasoningDisplay,
        /// Who produced it, when known from a verified signature.
        origin: Option<Origin>,
        /// Opaque upstream state (the Responses `encrypted_content`).
        opaque: Option<Vec<u8>>,
    },
}

/// A tool result.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    /// The id of the call this answers.
    pub call_id: String,
    /// The content (text and images).
    pub content: Vec<Part>,
    /// Whether the tool reported an error.
    pub is_error: bool,
}

/// One turn of the conversation.
#[derive(Debug, Clone, PartialEq)]
pub enum Turn {
    /// A user turn.
    User(Vec<Part>),
    /// An assistant turn.
    Assistant(Vec<Part>),
    /// Tool results, placed directly after the assistant turn that called them.
    ToolResults(Vec<ToolResult>),
}

/// Typed, inbound-specific extras.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extensions {
    /// Which protocol the request arrived on.
    pub inbound: Protocol,
}

/// The protocol-neutral request.
#[derive(Debug, Clone, PartialEq)]
pub struct IrRequest {
    /// The model as the client named it.
    pub model: String,
    /// System text, positioned.
    pub system: Vec<SystemPart>,
    /// The conversation.
    pub turns: Vec<Turn>,
    /// Function tools.
    pub tools: Vec<ToolDef>,
    /// Tool selection.
    pub tool_choice: ToolChoice,
    /// The output token limit.
    pub max_output_tokens: u32,
    /// Sampling parameters.
    pub sampling: Sampling,
    /// Stop sequences.
    pub stop: Vec<String>,
    /// Reasoning controls.
    pub reasoning: Reasoning,
    /// The response shape.
    pub response_format: ResponseFormat,
    /// Whether the client wants a stream.
    pub stream: bool,
    /// Whether any `cache_control` was seen.
    pub cache_hints: bool,
    /// Typed extras.
    pub extensions: Extensions,
}

impl IrRequest {
    /// An empty request for `inbound`, for builders and tests.
    #[must_use]
    pub fn new(model: impl Into<String>, inbound: Protocol) -> Self {
        Self {
            model: model.into(),
            system: Vec::new(),
            turns: Vec::new(),
            tools: Vec::new(),
            tool_choice: ToolChoice::default(),
            max_output_tokens: 0,
            sampling: Sampling::default(),
            stop: Vec::new(),
            reasoning: Reasoning::default(),
            response_format: ResponseFormat::Text,
            stream: false,
            cache_hints: false,
            extensions: Extensions { inbound },
        }
    }

    /// Whether any turn carries an image.
    #[must_use]
    pub fn has_images(&self) -> bool {
        fn part_is_image(p: &Part) -> bool {
            matches!(p, Part::Image(_))
        }
        self.turns.iter().any(|t| match t {
            Turn::User(parts) | Turn::Assistant(parts) => parts.iter().any(part_is_image),
            Turn::ToolResults(rs) => rs.iter().any(|r| r.content.iter().any(part_is_image)),
        })
    }
}

/// Why a completion stopped, in the protocol-neutral vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    /// A natural stop.
    Stop,
    /// The output limit was reached.
    Length,
    /// The model called tools.
    ToolCalls,
    /// The content was refused or filtered.
    Refusal,
}

/// Token usage, Anthropic-shaped: `input_tokens` excludes cached tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Output tokens, including reasoning.
    pub output_tokens: u64,
    /// Input tokens read from the cache.
    pub cache_read_input_tokens: u64,
    /// Input tokens written to the cache (no OpenAI-protocol upstream reports this).
    pub cache_creation_input_tokens: u64,
    /// Reasoning tokens, a subset of `output_tokens`.
    pub reasoning_tokens: u64,
}

/// The protocol-neutral result of a non-streaming call.
#[derive(Debug, Clone, PartialEq)]
pub struct IrCompletion {
    /// Output content.
    pub content: Vec<Part>,
    /// Why generation stopped.
    pub finish: FinishReason,
    /// Usage.
    pub usage: Usage,
    /// The upstream's response id.
    pub upstream_id: Option<String>,
    /// The upstream's model name.
    pub upstream_model: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_clamps_down_to_the_accepted_set() {
        let generic = [Effort::Low, Effort::Medium, Effort::High];
        assert_eq!(Effort::Max.clamp_down(&generic), Some(Effort::High));
        assert_eq!(Effort::XHigh.clamp_down(&generic), Some(Effort::High));
        assert_eq!(Effort::Medium.clamp_down(&generic), Some(Effort::Medium));
        assert_eq!(Effort::Low.clamp_down(&generic), Some(Effort::Low));
    }

    #[test]
    fn effort_clamps_up_only_when_nothing_lower_is_accepted() {
        let high_only = [Effort::High];
        assert_eq!(Effort::Low.clamp_down(&high_only), Some(Effort::High));
        assert_eq!(Effort::Low.clamp_down(&[]), None);
    }

    #[test]
    fn effort_orders_and_round_trips_names() {
        assert!(Effort::Low < Effort::Medium && Effort::XHigh < Effort::Max);
        for e in Effort::ALL {
            assert_eq!(Effort::parse(e.as_str()), Some(e));
        }
        assert_eq!(Effort::parse("extreme"), None);
    }

    #[test]
    fn budgets_bucket_into_efforts() {
        assert_eq!(Effort::from_budget(1_024), Effort::Low);
        assert_eq!(Effort::from_budget(4_096), Effort::Medium);
        assert_eq!(Effort::from_budget(16_000), Effort::High);
        assert_eq!(Effort::from_budget(32_000), Effort::XHigh);
        assert_eq!(Effort::from_budget(100_000), Effort::Max);
    }

    #[test]
    fn protocol_names_round_trip() {
        for p in [
            Protocol::Anthropic,
            Protocol::Chat,
            Protocol::Responses,
            Protocol::Gemini,
            Protocol::Bedrock,
        ] {
            assert_eq!(Protocol::parse(p.as_str()), Some(p));
        }
        assert_eq!(Protocol::parse("grpc"), None);
    }

    #[test]
    fn reasoning_wanted_only_when_on_or_adaptive() {
        let mut r = Reasoning::default();
        assert!(!r.wanted());
        r.mode = ReasoningMode::Off;
        assert!(!r.wanted());
        r.mode = ReasoningMode::On {
            budget_tokens: Some(1024),
        };
        assert!(r.wanted());
        r.mode = ReasoningMode::Adaptive;
        assert!(r.wanted());
    }

    #[test]
    fn has_images_looks_inside_tool_results() {
        let mut r = IrRequest::new("m", Protocol::Anthropic);
        assert!(!r.has_images());
        r.turns.push(Turn::ToolResults(vec![ToolResult {
            call_id: "c".into(),
            content: vec![Part::Image(ImageSource::Url("https://x/y.png".into()))],
            is_error: false,
        }]));
        assert!(r.has_images());
    }

    #[test]
    fn default_tool_choice_allows_parallel_calls() {
        let tc = ToolChoice::default();
        assert_eq!(tc.kind, ToolChoiceKind::Auto);
        assert!(tc.parallel);
    }
}
