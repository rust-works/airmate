//! The capability catalog, and filtering targets by what a request needs (§7.2).
//!
//! Each target has a capability record from a built-in, versioned data file, overridable in the
//! configuration. For each request the router derives its **requirements**: tools present, images
//! present, thinking requested, structured output requested, and a minimum context of estimated
//! input tokens plus `max_tokens`. A target that lacks a requirement is skipped, and the skip is
//! recorded. An unknown model is assumed to support everything and to have an unbounded window.

use std::fmt;

use serde::Deserialize;

use airmate_core::facts::Facts;

use super::glob::glob_match;
use super::table::RouteTarget;
use crate::config::schema::{CatalogOverride, Price};

/// What a model can do.
#[derive(Debug, Clone, PartialEq)]
pub struct Capabilities {
    /// The context window, if known.
    pub context_window: Option<u64>,
    /// The most output tokens, if known.
    pub max_output_tokens: Option<u32>,
    /// Tool calling.
    pub tools: bool,
    /// Several tool calls in one turn.
    pub parallel_tools: bool,
    /// Image input.
    pub vision: bool,
    /// Document input.
    pub documents: bool,
    /// Reasoning.
    pub thinking: bool,
    /// A reasoning effort control.
    pub effort: bool,
    /// Structured output.
    pub structured_output: bool,
    /// Streaming.
    pub streaming: bool,
    /// List prices, if known.
    pub price: Option<Price>,
    /// Whether this record came from the catalog (false means the model is unknown).
    pub known: bool,
}

impl Capabilities {
    /// An unknown model: everything is assumed to work and the window is unbounded.
    #[must_use]
    pub fn unknown() -> Self {
        Self {
            context_window: None,
            max_output_tokens: None,
            tools: true,
            parallel_tools: true,
            vision: true,
            documents: true,
            thinking: true,
            effort: true,
            structured_output: true,
            streaming: true,
            price: None,
            known: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct RawEntry {
    #[serde(rename = "match")]
    pattern: String,
    context_window: Option<u64>,
    max_output_tokens: Option<u32>,
    tools: Option<bool>,
    parallel_tools: Option<bool>,
    vision: Option<bool>,
    documents: Option<bool>,
    thinking: Option<bool>,
    effort: Option<bool>,
    structured_output: Option<bool>,
    streaming: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct RawCatalog {
    as_of: String,
    entries: Vec<RawEntry>,
}

/// The capability catalog.
#[derive(Debug, Clone)]
pub struct Catalog {
    as_of: String,
    builtin: Vec<RawEntry>,
    overrides: Vec<CatalogOverride>,
}

const BUILTIN: &str = include_str!("catalog.yaml");

impl Catalog {
    /// The built-in catalog with `overrides` applied on top.
    ///
    /// # Panics
    ///
    /// If the shipped data file is malformed; a test guards against that.
    #[must_use]
    pub fn new(overrides: &[CatalogOverride]) -> Self {
        let raw: RawCatalog =
            serde_norway::from_str(BUILTIN).expect("the built-in catalog is valid");
        Self {
            as_of: raw.as_of,
            builtin: raw.entries,
            overrides: overrides.to_vec(),
        }
    }

    /// When the built-in data was written.
    #[must_use]
    pub fn as_of(&self) -> &str {
        &self.as_of
    }

    /// What `model` can do: the first matching built-in entry, then the first matching override
    /// applied over it.
    #[must_use]
    pub fn lookup(&self, model: &str) -> Capabilities {
        let base = self.builtin.iter().find(|e| glob_match(&e.pattern, model));
        let over = self
            .overrides
            .iter()
            .find(|o| glob_match(&o.pattern, model));
        if base.is_none() && over.is_none() {
            return Capabilities::unknown();
        }
        let mut c = Capabilities::unknown();
        c.known = true;
        // Anything a known entry does not state is conservatively off, except streaming.
        c.tools = true;
        c.parallel_tools = true;
        c.structured_output = true;
        c.vision = false;
        c.documents = false;
        c.thinking = false;
        c.effort = false;
        if let Some(e) = base {
            c.context_window = e.context_window;
            c.max_output_tokens = e.max_output_tokens;
            c.tools = e.tools.unwrap_or(c.tools);
            c.parallel_tools = e.parallel_tools.unwrap_or(c.parallel_tools);
            c.vision = e.vision.unwrap_or(c.vision);
            c.documents = e.documents.unwrap_or(c.documents);
            c.thinking = e.thinking.unwrap_or(c.thinking);
            c.effort = e.effort.unwrap_or(c.effort);
            c.structured_output = e.structured_output.unwrap_or(c.structured_output);
            c.streaming = e.streaming.unwrap_or(c.streaming);
        }
        if let Some(o) = over {
            c.context_window = o.context_window.or(c.context_window);
            c.max_output_tokens = o.max_output_tokens.or(c.max_output_tokens);
            c.tools = o.tools.unwrap_or(c.tools);
            c.parallel_tools = o.parallel_tools.unwrap_or(c.parallel_tools);
            c.vision = o.vision.unwrap_or(c.vision);
            c.documents = o.documents.unwrap_or(c.documents);
            c.thinking = o.thinking.unwrap_or(c.thinking);
            c.effort = o.effort.unwrap_or(c.effort);
            c.structured_output = o.structured_output.unwrap_or(c.structured_output);
            c.streaming = o.streaming.unwrap_or(c.streaming);
            c.price = o.price;
        }
        c
    }

    /// The capabilities of a target: its model's, with the target's own price taking precedence.
    #[must_use]
    pub fn for_target(&self, t: &RouteTarget) -> Capabilities {
        let mut c = self.lookup(&t.model);
        if t.price.is_some() {
            c.price = t.price;
        }
        c
    }
}

/// Why a target was not used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The request has tools and the model cannot call them.
    Tools,
    /// The request has images and the model cannot read them.
    Vision,
    /// The request asks for reasoning and the model cannot reason.
    Thinking,
    /// The request asks for structured output and the model cannot produce it.
    StructuredOutput,
    /// The request does not fit the context window.
    Context {
        /// Estimated input plus `max_tokens`.
        need: u64,
        /// The window.
        have: u64,
    },
    /// The request asks for streaming and the model cannot stream.
    Streaming,
    /// The circuit is open.
    CircuitOpen,
    /// The upstream failed its health check.
    Unhealthy,
    /// The upstream's concurrency limit is reached.
    Concurrency,
    /// An upstream or client rate limit is reached.
    RateLimit,
    /// The client's budget is exhausted.
    Budget,
    /// The upstream asked to be left alone for a while.
    CoolingDown,
}

impl SkipReason {
    /// The short code used in `x-airmate-skipped` and the log.
    #[must_use]
    pub fn code(&self) -> String {
        match self {
            Self::Tools => "tools".to_owned(),
            Self::Vision => "vision".to_owned(),
            Self::Thinking => "thinking".to_owned(),
            Self::StructuredOutput => "structured-output".to_owned(),
            Self::Context { need, have } => format!("context({need}>{have})"),
            Self::Streaming => "streaming".to_owned(),
            Self::CircuitOpen => "circuit-open".to_owned(),
            Self::Unhealthy => "unhealthy".to_owned(),
            Self::Concurrency => "concurrency".to_owned(),
            Self::RateLimit => "rate-limit".to_owned(),
            Self::Budget => "budget".to_owned(),
            Self::CoolingDown => "cooling-down".to_owned(),
        }
    }

    /// A sentence for an error message.
    #[must_use]
    pub fn explain(&self) -> String {
        match self {
            Self::Tools => "it cannot call tools".to_owned(),
            Self::Vision => "it cannot read images".to_owned(),
            Self::Thinking => "it cannot reason".to_owned(),
            Self::StructuredOutput => "it cannot produce structured output".to_owned(),
            Self::Context { need, have } => {
                format!("the request needs about {need} tokens of context and it has {have}")
            }
            Self::Streaming => "it cannot stream".to_owned(),
            Self::CircuitOpen => "its circuit breaker is open".to_owned(),
            Self::Unhealthy => "its upstream failed its health check".to_owned(),
            Self::Concurrency => "its concurrency limit is reached".to_owned(),
            Self::RateLimit => "a rate limit is reached".to_owned(),
            Self::Budget => "the client's budget is exhausted".to_owned(),
            Self::CoolingDown => "its upstream asked for a pause".to_owned(),
        }
    }

    /// Whether the skip is about what the model can do (as opposed to its state right now).
    #[must_use]
    pub fn is_capability(&self) -> bool {
        matches!(
            self,
            Self::Tools
                | Self::Vision
                | Self::Thinking
                | Self::StructuredOutput
                | Self::Context { .. }
                | Self::Streaming
        )
    }
}

/// A skipped target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skip {
    /// `upstream/model`.
    pub target: String,
    /// Why.
    pub reason: SkipReason,
}

impl fmt::Display for Skip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.target, self.reason.code())
    }
}

/// The first requirement of `facts` that `caps` cannot meet.
#[must_use]
pub fn unmet(caps: &Capabilities, facts: &Facts) -> Option<SkipReason> {
    if facts.tools && !caps.tools {
        return Some(SkipReason::Tools);
    }
    if facts.images && !caps.vision {
        return Some(SkipReason::Vision);
    }
    if facts.thinking && !caps.thinking {
        return Some(SkipReason::Thinking);
    }
    if facts.structured_output && !caps.structured_output {
        return Some(SkipReason::StructuredOutput);
    }
    if facts.stream && !caps.streaming {
        return Some(SkipReason::Streaming);
    }
    if let Some(have) = caps.context_window {
        let need = facts.min_context();
        if need > have {
            return Some(SkipReason::Context { need, have });
        }
    }
    None
}

/// Splits `targets` into those that can serve `facts` and the skips.
#[must_use]
pub fn filter<'a>(
    targets: &'a [RouteTarget],
    facts: &Facts,
    catalog: &Catalog,
) -> (Vec<&'a RouteTarget>, Vec<Skip>) {
    let mut ok = Vec::new();
    let mut skipped = Vec::new();
    for t in targets {
        match unmet(&catalog.for_target(t), facts) {
            None => ok.push(t),
            Some(reason) => skipped.push(Skip {
                target: t.label(),
                reason,
            }),
        }
    }
    (ok, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use airmate_core::ir::Protocol;

    fn facts() -> Facts {
        Facts {
            inbound: Protocol::Anthropic,
            model: "claude-x".into(),
            stream: false,
            tools: false,
            images: false,
            thinking: false,
            structured_output: false,
            effort: None,
            est_input_tokens: 1000,
            max_tokens: 1000,
        }
    }

    #[test]
    fn the_shipped_catalog_parses_and_is_dated() {
        let c = Catalog::new(&[]);
        assert_eq!(c.as_of(), "2026-10-14");
        for m in [
            "gpt-5",
            "gpt-4.1-mini",
            "gpt-4o",
            "o3-mini",
            "claude-sonnet-4",
        ] {
            assert!(c.lookup(m).known, "{m}");
        }
    }

    #[test]
    fn an_unknown_model_is_assumed_to_support_everything() {
        let c = Catalog::new(&[]).lookup("qwen3-coder");
        assert!(
            !c.known && c.tools && c.vision && c.thinking && c.structured_output && c.streaming
        );
        assert_eq!(
            (c.context_window, c.max_output_tokens, c.price),
            (None, None, None)
        );
    }

    #[test]
    fn a_known_model_has_only_what_the_catalog_says() {
        let c = Catalog::new(&[]).lookup("gpt-4.1-mini");
        assert!(c.known && c.vision && c.tools && !c.thinking && !c.effort);
        assert_eq!(c.context_window, Some(1_047_576));
        let c = Catalog::new(&[]).lookup("gpt-5-mini");
        assert!(c.thinking && c.effort);
        assert_eq!(c.price, None, "no prices are shipped");
    }

    #[test]
    fn overrides_win_field_by_field_and_can_describe_unknown_models() {
        let o = CatalogOverride {
            pattern: "gpt-4.1*".into(),
            context_window: Some(8000),
            vision: Some(false),
            ..CatalogOverride::default()
        };
        let c = Catalog::new(&[o]).lookup("gpt-4.1");
        assert_eq!(c.context_window, Some(8000));
        assert!(
            !c.vision && c.tools,
            "fields the override does not set keep the built-in value"
        );
        let local = CatalogOverride {
            pattern: "qwen3*".into(),
            context_window: Some(32768),
            tools: Some(false),
            price: Some(Price {
                input_per_mtok: 0.1,
                output_per_mtok: 0.2,
                cache_read_per_mtok: None,
            }),
            ..CatalogOverride::default()
        };
        let c = Catalog::new(&[local]).lookup("qwen3-coder");
        assert!(c.known && !c.tools && c.context_window == Some(32768));
        assert_eq!(c.price.unwrap().input_per_mtok, 0.1);
    }

    #[test]
    fn requirements_that_a_model_lacks_are_reported_in_a_fixed_order() {
        let mut caps = Capabilities::unknown();
        caps.tools = false;
        caps.vision = false;
        let mut f = facts();
        assert_eq!(unmet(&caps, &f), None);
        f.tools = true;
        f.images = true;
        assert_eq!(unmet(&caps, &f), Some(SkipReason::Tools));
        caps.tools = true;
        assert_eq!(unmet(&caps, &f), Some(SkipReason::Vision));
        caps.vision = true;
        f.thinking = true;
        caps.thinking = false;
        assert_eq!(unmet(&caps, &f), Some(SkipReason::Thinking));
        caps.thinking = true;
        f.structured_output = true;
        caps.structured_output = false;
        assert_eq!(unmet(&caps, &f), Some(SkipReason::StructuredOutput));
        caps.structured_output = true;
        f.stream = true;
        caps.streaming = false;
        assert_eq!(unmet(&caps, &f), Some(SkipReason::Streaming));
    }

    #[test]
    fn the_context_window_must_hold_input_plus_max_tokens() {
        let mut caps = Capabilities::unknown();
        caps.context_window = Some(2000);
        let mut f = facts();
        assert_eq!(unmet(&caps, &f), None, "1000 + 1000 fits exactly");
        f.max_tokens = 1001;
        assert_eq!(
            unmet(&caps, &f),
            Some(SkipReason::Context {
                need: 2001,
                have: 2000
            })
        );
        caps.context_window = None;
        assert_eq!(unmet(&caps, &f), None, "an unbounded window fits anything");
    }

    #[test]
    fn skip_codes_and_explanations_are_readable() {
        assert_eq!(
            SkipReason::Context { need: 5, have: 3 }.code(),
            "context(5>3)"
        );
        for r in [
            SkipReason::Tools,
            SkipReason::Vision,
            SkipReason::Thinking,
            SkipReason::StructuredOutput,
            SkipReason::Streaming,
            SkipReason::CircuitOpen,
            SkipReason::Unhealthy,
            SkipReason::Concurrency,
            SkipReason::RateLimit,
            SkipReason::Budget,
            SkipReason::CoolingDown,
        ] {
            assert!(!r.code().is_empty() && !r.explain().is_empty());
        }
        assert!(SkipReason::Vision.is_capability() && !SkipReason::CircuitOpen.is_capability());
        let s = Skip {
            target: "fw/glm".into(),
            reason: SkipReason::Vision,
        };
        assert_eq!(s.to_string(), "fw/glm:vision");
    }
}
