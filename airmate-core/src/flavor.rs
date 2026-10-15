//! Provider flavors.
//!
//! "OpenAI-compatible" servers disagree on details. Each difference is one named method on
//! [`Flavor`], never an `if` scattered through the translators. Every quirk carries a "documented
//! as of" date: it is documented behaviour, and only becomes *verified* once its live conformance
//! test has passed.

use crate::ir::{Effort, Protocol};

/// The date the quirks below were taken from the specification.
pub const DOCUMENTED_AS_OF: &str = "2026-10-14";

/// A provider flavor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Flavor {
    /// Any OpenAI-compatible server (Ollama, LM Studio, vLLM, OpenRouter, ...).
    #[default]
    Generic,
    /// OpenAI itself.
    OpenAi,
    /// Fireworks AI.
    Fireworks,
}

/// Which field carries the output token limit. Exactly one is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenLimitField {
    /// `max_tokens`
    MaxTokens,
    /// `max_completion_tokens`
    MaxCompletionTokens,
    /// `max_output_tokens` (Responses)
    MaxOutputTokens,
}

impl TokenLimitField {
    /// The JSON key.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::MaxTokens => "max_tokens",
            Self::MaxCompletionTokens => "max_completion_tokens",
            Self::MaxOutputTokens => "max_output_tokens",
        }
    }
}

/// How a flavor controls reasoning on Chat Completions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningControl {
    /// `reasoning_effort` only.
    EffortOnly,
    /// `thinking` **or** `reasoning_effort`, never both (Fireworks).
    ThinkingOrEffort,
}

/// How streamed usage is requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamUsage {
    /// Ask with `stream_options.include_usage`.
    IncludeUsageOption,
    /// The server always sends it; the final event folds it in.
    AlwaysSent,
}

impl Flavor {
    /// Parses a configuration name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "generic" => Some(Self::Generic),
            "openai" => Some(Self::OpenAi),
            "fireworks" => Some(Self::Fireworks),
            _ => None,
        }
    }

    /// The configuration name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::OpenAi => "openai",
            Self::Fireworks => "fireworks",
        }
    }

    /// The token-limit field for `protocol`. `reasoning_model` is whether the model belongs to a
    /// reasoning family (see [`is_reasoning_family`]).
    #[must_use]
    pub fn token_limit_field(self, protocol: Protocol, reasoning_model: bool) -> TokenLimitField {
        if protocol == Protocol::Responses {
            return TokenLimitField::MaxOutputTokens;
        }
        match self {
            Self::OpenAi => TokenLimitField::MaxCompletionTokens,
            Self::Fireworks => TokenLimitField::MaxTokens,
            Self::Generic if reasoning_model => TokenLimitField::MaxCompletionTokens,
            Self::Generic => TokenLimitField::MaxTokens,
        }
    }

    /// How usage is obtained on a stream.
    #[must_use]
    pub fn stream_usage(self) -> StreamUsage {
        match self {
            Self::Fireworks => StreamUsage::AlwaysSent,
            Self::Generic | Self::OpenAi => StreamUsage::IncludeUsageOption,
        }
    }

    /// How reasoning is controlled on Chat Completions.
    #[must_use]
    pub fn reasoning_control(self) -> ReasoningControl {
        match self {
            Self::Fireworks => ReasoningControl::ThinkingOrEffort,
            Self::Generic | Self::OpenAi => ReasoningControl::EffortOnly,
        }
    }

    /// The most stop sequences the upstream accepts.
    #[must_use]
    pub fn stop_cap(self) -> Option<usize> {
        match self {
            Self::Generic => None,
            Self::OpenAi | Self::Fireworks => Some(4),
        }
    }

    /// A `context_length_exceeded_behavior` to send, so the upstream errors where Anthropic would
    /// error instead of silently truncating.
    #[must_use]
    pub fn context_overflow_behavior(self) -> Option<&'static str> {
        match self {
            Self::Fireworks => Some("error"),
            Self::Generic | Self::OpenAi => None,
        }
    }

    /// Whether the upstream takes a `prompt_cache_key`.
    #[must_use]
    pub fn sends_prompt_cache_key(self) -> bool {
        true
    }

    /// Whether reasoning is replayed as `reasoning_content` on assistant messages in later turns.
    #[must_use]
    pub fn replays_reasoning_content(self) -> bool {
        matches!(self, Self::Fireworks | Self::Generic)
    }

    /// Whether strict structured output is supported.
    #[must_use]
    pub fn strict_structured_output(self) -> bool {
        matches!(self, Self::OpenAi | Self::Fireworks)
    }

    /// Whether `top_k` is sent.
    #[must_use]
    pub fn sends_top_k(self) -> bool {
        matches!(self, Self::Generic | Self::Fireworks)
    }

    /// The effort levels the flavor accepts, ascending. Overridable per upstream or model.
    #[must_use]
    pub fn effort_levels(self) -> &'static [Effort] {
        match self {
            Self::Generic => &[Effort::Low, Effort::Medium, Effort::High],
            Self::OpenAi => &[Effort::Low, Effort::Medium, Effort::High, Effort::XHigh],
            Self::Fireworks => &Effort::ALL,
        }
    }

    /// Request fields Fireworks documents as unsupported on its Anthropic-compatible surface.
    #[must_use]
    pub fn anthropic_unsupported_request_fields(self) -> &'static [&'static str] {
        match self {
            Self::Fireworks => &["inference_geo"],
            Self::Generic | Self::OpenAi => &[],
        }
    }

    /// Tool fields documented as unsupported on the flavor's Anthropic-compatible surface.
    #[must_use]
    pub fn anthropic_unsupported_tool_fields(self) -> &'static [&'static str] {
        match self {
            Self::Fireworks => &[
                "cache_control",
                "eager_input_streaming",
                "allowed_callers",
                "input_examples",
            ],
            Self::Generic | Self::OpenAi => &[],
        }
    }

    /// The date a flavor's quirks were documented.
    #[must_use]
    pub fn documented_as_of(self) -> &'static str {
        DOCUMENTED_AS_OF
    }
}

/// Whether `model` belongs to a family that takes `max_completion_tokens` and accepts only the
/// default sampling parameters (OpenAI GPT-5 and the o-series).
#[must_use]
pub fn is_reasoning_family(model: &str) -> bool {
    let name = model.rsplit('/').next().unwrap_or(model);
    if name.starts_with("gpt-5") {
        return true;
    }
    let mut chars = name.chars();
    chars.next() == Some('o') && chars.next().is_some_and(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for f in [Flavor::Generic, Flavor::OpenAi, Flavor::Fireworks] {
            assert_eq!(Flavor::parse(f.as_str()), Some(f));
        }
        assert_eq!(Flavor::parse("azure"), None);
        assert_eq!(Flavor::default(), Flavor::Generic);
    }

    #[test]
    fn exactly_one_token_limit_field_per_target() {
        use Protocol::{Chat, Responses};
        use TokenLimitField::*;
        assert_eq!(
            Flavor::OpenAi.token_limit_field(Chat, false),
            MaxCompletionTokens
        );
        assert_eq!(Flavor::Fireworks.token_limit_field(Chat, true), MaxTokens);
        assert_eq!(Flavor::Generic.token_limit_field(Chat, false), MaxTokens);
        assert_eq!(
            Flavor::Generic.token_limit_field(Chat, true),
            MaxCompletionTokens
        );
        for f in [Flavor::Generic, Flavor::OpenAi, Flavor::Fireworks] {
            assert_eq!(f.token_limit_field(Responses, false), MaxOutputTokens);
        }
        assert_eq!(MaxTokens.key(), "max_tokens");
        assert_eq!(MaxCompletionTokens.key(), "max_completion_tokens");
        assert_eq!(MaxOutputTokens.key(), "max_output_tokens");
    }

    #[test]
    fn the_quirk_table_matches_the_specification() {
        assert_eq!(Flavor::Fireworks.stream_usage(), StreamUsage::AlwaysSent);
        assert_eq!(
            Flavor::OpenAi.stream_usage(),
            StreamUsage::IncludeUsageOption
        );
        assert_eq!(
            Flavor::Fireworks.reasoning_control(),
            ReasoningControl::ThinkingOrEffort
        );
        assert_eq!(
            Flavor::Generic.reasoning_control(),
            ReasoningControl::EffortOnly
        );
        assert_eq!(Flavor::Generic.stop_cap(), None);
        assert_eq!(Flavor::OpenAi.stop_cap(), Some(4));
        assert_eq!(Flavor::Fireworks.stop_cap(), Some(4));
        assert_eq!(Flavor::Fireworks.context_overflow_behavior(), Some("error"));
        assert_eq!(Flavor::OpenAi.context_overflow_behavior(), None);
        assert!(Flavor::Generic.sends_prompt_cache_key());
        assert!(Flavor::Fireworks.replays_reasoning_content());
        assert!(!Flavor::OpenAi.replays_reasoning_content());
        assert!(Flavor::OpenAi.strict_structured_output());
        assert!(!Flavor::Generic.strict_structured_output());
        assert!(Flavor::Generic.sends_top_k() && Flavor::Fireworks.sends_top_k());
        assert!(!Flavor::OpenAi.sends_top_k());
    }

    #[test]
    fn effort_sets_are_ascending_and_per_flavor() {
        assert_eq!(
            Flavor::Generic.effort_levels(),
            &[Effort::Low, Effort::Medium, Effort::High]
        );
        assert_eq!(Flavor::OpenAi.effort_levels().last(), Some(&Effort::XHigh));
        assert_eq!(Flavor::Fireworks.effort_levels().len(), 5);
        for f in [Flavor::Generic, Flavor::OpenAi, Flavor::Fireworks] {
            assert!(f.effort_levels().windows(2).all(|w| w[0] < w[1]));
        }
    }

    #[test]
    fn fireworks_documents_what_its_anthropic_surface_lacks() {
        assert_eq!(
            Flavor::Fireworks.anthropic_unsupported_request_fields(),
            &["inference_geo"]
        );
        assert!(Flavor::Fireworks
            .anthropic_unsupported_tool_fields()
            .contains(&"allowed_callers"));
        assert!(Flavor::OpenAi
            .anthropic_unsupported_tool_fields()
            .is_empty());
        assert!(Flavor::Generic
            .anthropic_unsupported_request_fields()
            .is_empty());
    }

    #[test]
    fn quirks_carry_a_documented_date() {
        assert_eq!(Flavor::Fireworks.documented_as_of(), DOCUMENTED_AS_OF);
    }

    #[test]
    fn reasoning_families_are_recognised() {
        for m in [
            "gpt-5",
            "gpt-5-mini",
            "o1",
            "o3-mini",
            "o4-mini",
            "openai/gpt-5.1",
            "accounts/x/models/o3",
        ] {
            assert!(is_reasoning_family(m), "{m}");
        }
        for m in [
            "gpt-4o",
            "gpt-4.1",
            "qwen3-coder",
            "llama3",
            "omni-1",
            "o",
            "",
        ] {
            assert!(!is_reasoning_family(m), "{m}");
        }
    }
}
