//! Outbound adapters: build an upstream request from the IR and read the reply.

pub mod anthropic;
pub mod anthropic_build;
pub mod anthropic_parse;
pub mod chat;
pub mod chat_parse;
pub mod responses;
pub mod responses_parse;

use crate::flavor::Flavor;
use crate::ir::{Effort, Origin, Protocol};
use crate::policy::Ignored;
use crate::toolname::ToolNames;

/// An (upstream, model) pair with its per-target overrides: where one request is going.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The upstream's name.
    pub upstream: String,
    /// The protocol the upstream speaks.
    pub protocol: Protocol,
    /// The upstream's flavor.
    pub flavor: Flavor,
    /// The model name to send upstream.
    pub model: String,
    /// Overrides the flavor's accepted effort levels.
    pub effort_levels: Option<Vec<Effort>>,
    /// Caps the output token limit sent upstream.
    pub max_output_tokens_cap: Option<u32>,
    /// Whether a leading `<think>` block in content is split out as reasoning.
    pub think_tags: bool,
}

impl Target {
    /// A target with defaults, for the common case and for tests.
    #[must_use]
    pub fn new(upstream: &str, protocol: Protocol, flavor: Flavor, model: &str) -> Self {
        Self {
            upstream: upstream.to_owned(),
            protocol,
            flavor,
            model: model.to_owned(),
            effort_levels: None,
            max_output_tokens_cap: None,
            think_tags: true,
        }
    }

    /// Who produced reasoning that came from this target.
    #[must_use]
    pub fn origin(&self) -> Origin {
        Origin {
            upstream: self.upstream.clone(),
            protocol: self.protocol,
        }
    }

    /// The effort levels this target accepts, ascending.
    #[must_use]
    pub fn accepted_efforts(&self) -> &[Effort] {
        self.effort_levels
            .as_deref()
            .unwrap_or_else(|| self.flavor.effort_levels())
    }
}

/// A request ready to send.
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamRequest {
    /// The path to append to the upstream's base URL, e.g. `/v1/chat/completions`.
    pub path: String,
    /// The JSON body.
    pub body: Vec<u8>,
    /// Whether the reply is a stream.
    pub stream: bool,
    /// The tool-name mapping to apply in reverse to the reply.
    pub tool_names: ToolNames,
    /// What was dropped while building, to report to the client.
    pub ignored: Vec<Ignored>,
}

/// What a reply parser needs to know about the request it answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseCtx {
    /// Who the reply comes from; attached to reasoning so signatures can bind to it.
    pub origin: Origin,
    /// The mapping to restore shortened tool names.
    pub tool_names: ToolNames,
    /// Whether a leading `<think>` block in content is split out as reasoning.
    pub think_tags: bool,
}

impl ParseCtx {
    /// The context for a reply to `req` sent to `target`.
    #[must_use]
    pub fn new(target: &Target, req: &UpstreamRequest) -> Self {
        Self {
            origin: target.origin(),
            tool_names: req.tool_names.clone(),
            think_tags: target.think_tags,
        }
    }
}

/// An error answer from an upstream, or a reply that cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamError {
    /// The HTTP status.
    pub status: u16,
    /// The upstream's message, or a description of what was wrong with the reply.
    pub message: String,
    /// The upstream's error code or type, if it gave one.
    pub code: Option<String>,
}

impl UpstreamError {
    /// Reads an error body in any of the common shapes: `{"error": {"message", "code"|"type"}}`,
    /// `{"error": "text"}`, `{"message": "text"}` or `{"detail": "text"}`.
    #[must_use]
    pub fn from_response(status: u16, body: &[u8]) -> Self {
        let json: Option<serde_json::Value> = serde_json::from_slice(body).ok();
        let text = |v: &serde_json::Value| v.as_str().map(str::to_owned);
        let (message, code) = match &json {
            Some(j) => {
                let err = j.get("error");
                let message = err
                    .and_then(|e| e.get("message").and_then(text).or_else(|| text(e)))
                    .or_else(|| j.get("message").and_then(text))
                    .or_else(|| j.get("detail").and_then(text));
                let code = err.and_then(|e| {
                    e.get("code")
                        .and_then(text)
                        .or_else(|| e.get("type").and_then(text))
                });
                (message, code)
            }
            None => (None, None),
        };
        let message = message.unwrap_or_else(|| {
            let raw = String::from_utf8_lossy(body);
            let raw = raw.trim();
            if raw.is_empty() {
                format!("the upstream answered {status}")
            } else {
                raw.chars().take(500).collect()
            }
        });
        Self {
            status,
            message,
            code,
        }
    }

    /// An unreadable reply (invalid JSON or the wrong shape).
    #[must_use]
    pub fn unreadable(message: impl Into<String>) -> Self {
        Self {
            status: 502,
            message: message.into(),
            code: None,
        }
    }

    /// Whether this is a context-length overflow.
    #[must_use]
    pub fn is_context_overflow(&self) -> bool {
        if self.code.as_deref() == Some("context_length_exceeded") {
            return true;
        }
        let m = self.message.to_ascii_lowercase();
        [
            "context length",
            "context_length",
            "maximum context",
            "context window",
            "too many tokens",
            "prompt is too long",
            "reduce the length",
        ]
        .iter()
        .any(|needle| m.contains(needle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_bodies_are_read_in_every_common_shape() {
        let e = UpstreamError::from_response(400, br#"{"error":{"message":"bad","code":"x"}}"#);
        assert_eq!((e.message.as_str(), e.code.as_deref()), ("bad", Some("x")));
        let e = UpstreamError::from_response(
            400,
            br#"{"error":{"message":"bad","type":"invalid_request_error"}}"#,
        );
        assert_eq!(e.code.as_deref(), Some("invalid_request_error"));
        assert_eq!(
            UpstreamError::from_response(500, br#"{"error":"flat"}"#).message,
            "flat"
        );
        assert_eq!(
            UpstreamError::from_response(500, br#"{"message":"m"}"#).message,
            "m"
        );
        assert_eq!(
            UpstreamError::from_response(500, br#"{"detail":"d"}"#).message,
            "d"
        );
        assert_eq!(
            UpstreamError::from_response(502, b"<html>gateway</html>").message,
            "<html>gateway</html>"
        );
        assert_eq!(
            UpstreamError::from_response(503, b"").message,
            "the upstream answered 503"
        );
        assert_eq!(UpstreamError::from_response(400, b"{}").message, "{}");
    }

    #[test]
    fn long_raw_bodies_are_truncated() {
        let body = "x".repeat(2000);
        assert_eq!(
            UpstreamError::from_response(500, body.as_bytes())
                .message
                .len(),
            500
        );
    }

    #[test]
    fn context_overflow_is_recognised_by_code_or_message() {
        let by_code = UpstreamError {
            status: 400,
            message: "boom".into(),
            code: Some("context_length_exceeded".into()),
        };
        assert!(by_code.is_context_overflow());
        for m in [
            "This model's maximum context length is 8192 tokens",
            "Input is too long for the context window",
            "too many tokens",
            "Please reduce the length of the messages",
        ] {
            assert!(
                UpstreamError {
                    status: 400,
                    message: m.into(),
                    code: None
                }
                .is_context_overflow(),
                "{m}"
            );
        }
        assert!(!UpstreamError {
            status: 400,
            message: "bad json".into(),
            code: None
        }
        .is_context_overflow());
    }

    #[test]
    fn unreadable_replies_are_bad_gateway() {
        assert_eq!(UpstreamError::unreadable("nope").status, 502);
    }

    #[test]
    fn the_parse_context_carries_origin_and_names() {
        let t = Target::new("up", Protocol::Chat, Flavor::Generic, "m");
        let req = UpstreamRequest {
            path: String::new(),
            body: vec![],
            stream: false,
            tool_names: ToolNames::new(),
            ignored: vec![],
        };
        let ctx = ParseCtx::new(&t, &req);
        assert_eq!(ctx.origin, t.origin());
        assert!(ctx.think_tags);
        assert!(ctx.tool_names.is_empty());
        assert_eq!(t.accepted_efforts(), Flavor::Generic.effort_levels());
    }
}
