//! The facts routing needs about a request.
//!
//! The router decides from these, so a passthrough request only has to be peeked at, not fully
//! parsed: its body is forwarded as the client sent it.

use crate::estimate::estimate_input_tokens;
use crate::ir::{Effort, IrRequest, Protocol, ResponseFormat};

/// What routing needs to know about a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    /// The protocol the request arrived on.
    pub inbound: Protocol,
    /// The model as the client named it.
    pub model: String,
    /// Whether the client wants a stream.
    pub stream: bool,
    /// Whether tools are defined.
    pub tools: bool,
    /// Whether any image is present.
    pub images: bool,
    /// Whether reasoning was requested.
    pub thinking: bool,
    /// Whether structured output was requested.
    pub structured_output: bool,
    /// The requested effort, if any.
    pub effort: Option<Effort>,
    /// The estimated input tokens.
    pub est_input_tokens: u64,
    /// The output token limit.
    pub max_tokens: u32,
}

impl Facts {
    /// The facts of a parsed request.
    #[must_use]
    pub fn from_ir(req: &IrRequest) -> Self {
        Self {
            inbound: req.extensions.inbound,
            model: req.model.clone(),
            stream: req.stream,
            tools: !req.tools.is_empty(),
            images: req.has_images(),
            thinking: req.reasoning.wanted(),
            structured_output: !matches!(req.response_format, ResponseFormat::Text),
            effort: req.reasoning.effort,
            est_input_tokens: estimate_input_tokens(req),
            max_tokens: req.max_output_tokens,
        }
    }

    /// The context the request needs: estimated input plus the output limit.
    #[must_use]
    pub fn min_context(&self) -> u64 {
        self.est_input_tokens + u64::from(self.max_tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{ImageSource, Part, Reasoning, ReasoningMode, ToolDef, Turn};
    use serde_json::json;

    #[test]
    fn facts_reflect_the_request() {
        let mut r = IrRequest::new("claude-x", Protocol::Anthropic);
        r.max_output_tokens = 500;
        r.stream = true;
        r.reasoning = Reasoning {
            mode: ReasoningMode::Adaptive,
            effort: Some(Effort::High),
            ..Reasoning::default()
        };
        r.response_format = ResponseFormat::JsonSchema {
            schema: json!({}),
            strict: true,
        };
        r.tools.push(ToolDef {
            name: "t".into(),
            description: None,
            input_schema: json!({}),
            strict: None,
        });
        r.turns
            .push(Turn::User(vec![Part::Image(ImageSource::Url("u".into()))]));
        let f = Facts::from_ir(&r);
        assert_eq!(f.model, "claude-x");
        assert!(f.stream && f.tools && f.images && f.thinking && f.structured_output);
        assert_eq!(f.effort, Some(Effort::High));
        assert_eq!(f.max_tokens, 500);
        assert_eq!(f.min_context(), f.est_input_tokens + 500);
        assert_eq!(f.inbound, Protocol::Anthropic);
    }

    #[test]
    fn a_plain_request_needs_nothing_special() {
        let f = Facts::from_ir(&IrRequest::new("m", Protocol::Chat));
        assert!(!f.stream && !f.tools && !f.images && !f.thinking && !f.structured_output);
        assert_eq!(f.effort, None);
    }
}
