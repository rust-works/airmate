//! The Anthropic outbound adapter: passthrough.
//!
//! When both sides speak the Anthropic protocol the body is forwarded as the client sent it. The
//! model is mapped, the provider's documented unsupported fields are stripped and reported as
//! ignored, and only what routing needs is parsed.

use serde_json::{Map, Value as Json};

use super::{Target, UpstreamRequest};
use crate::estimate::{TOKENS_PER_IMAGE, TOKENS_PER_MESSAGE, TOKENS_PER_TOOL};
use crate::facts::Facts;
use crate::ir::{Effort, Protocol, Usage};
use crate::policy::{Policy, PolicyError};
use crate::sse::{format_event, SseParser};
use crate::stream::StreamError;
use crate::toolname::ToolNames;

/// The Messages path.
pub const PATH: &str = "/v1/messages";
/// The token-counting path.
pub const COUNT_TOKENS_PATH: &str = "/v1/messages/count_tokens";

fn bad(field: &str, message: &str) -> PolicyError {
    PolicyError {
        field: field.to_owned(),
        message: message.to_owned(),
    }
}

fn parse(body: &[u8]) -> Result<Map<String, Json>, PolicyError> {
    match serde_json::from_slice::<Json>(body) {
        Ok(Json::Object(m)) => Ok(m),
        Ok(_) => Err(bad("body", "the request body must be a JSON object")),
        Err(e) => Err(PolicyError {
            field: "body".to_owned(),
            message: format!("the request body is not valid JSON: {e}"),
        }),
    }
}

fn estimate_value(v: &Json) -> u64 {
    match v {
        Json::String(s) => s.chars().count().div_ceil(4) as u64,
        Json::Array(a) => a.iter().map(estimate_value).sum(),
        Json::Object(o) => {
            if o.get("type").and_then(Json::as_str) == Some("image") {
                return TOKENS_PER_IMAGE;
            }
            o.values().map(estimate_value).sum()
        }
        _ => 0,
    }
}

fn has_image(v: &Json) -> bool {
    match v {
        Json::Array(a) => a.iter().any(has_image),
        Json::Object(o) => {
            o.get("type").and_then(Json::as_str) == Some("image") || o.values().any(has_image)
        }
        _ => false,
    }
}

/// Reads only what routing needs from an Anthropic request body.
///
/// # Errors
///
/// A [`PolicyError`] when the body is not a JSON object with a `model`.
pub fn peek(body: &[u8]) -> Result<Facts, PolicyError> {
    let root = Json::Object(parse(body)?);
    let obj = &root;
    let model = obj
        .get("model")
        .and_then(Json::as_str)
        .ok_or_else(|| bad("model", "`model` is required"))?
        .to_owned();
    let messages = obj.get("messages").and_then(Json::as_array);
    let tools = obj.get("tools").and_then(Json::as_array);
    let mut est = obj.get("system").map_or(0, estimate_value);
    for m in messages.into_iter().flatten() {
        est += TOKENS_PER_MESSAGE + m.get("content").map_or(0, estimate_value);
    }
    for t in tools.into_iter().flatten() {
        est += TOKENS_PER_TOOL + estimate_value(t);
    }
    let thinking = matches!(
        obj.pointer("/thinking/type").and_then(Json::as_str),
        Some("enabled" | "adaptive" | "between_tools")
    );
    Ok(Facts {
        inbound: Protocol::Anthropic,
        model,
        stream: obj.get("stream").and_then(Json::as_bool).unwrap_or(false),
        tools: tools.is_some_and(|t| !t.is_empty()),
        images: messages.is_some_and(|m| m.iter().any(has_image)),
        thinking,
        structured_output: obj.pointer("/output_config/format").is_some(),
        effort: obj
            .pointer("/output_config/effort")
            .and_then(Json::as_str)
            .and_then(Effort::parse),
        est_input_tokens: est,
        max_tokens: obj
            .get("max_tokens")
            .and_then(Json::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0),
    })
}

/// Builds the forwarded request: the model mapped, documented-unsupported fields stripped.
///
/// # Errors
///
/// A [`PolicyError`] when the body is not a JSON object.
pub fn build_passthrough(body: &[u8], target: &Target) -> Result<UpstreamRequest, PolicyError> {
    build_at(body, target, PATH)
}

/// Like [`build_passthrough`] for the token-counting endpoint.
///
/// # Errors
///
/// A [`PolicyError`] when the body is not a JSON object.
pub fn build_count_tokens(body: &[u8], target: &Target) -> Result<UpstreamRequest, PolicyError> {
    build_at(body, target, COUNT_TOKENS_PATH)
}

fn build_at(body: &[u8], target: &Target, path: &str) -> Result<UpstreamRequest, PolicyError> {
    let mut obj = parse(body)?;
    let mut ignored = Policy::collector();
    obj.insert("model".to_owned(), Json::String(target.model.clone()));
    for field in target.flavor.anthropic_unsupported_request_fields() {
        if obj.remove(*field).is_some() {
            ignored.ignore(field, "the upstream documents this field as unsupported");
        }
    }
    let strip = target.flavor.anthropic_unsupported_tool_fields();
    if let Some(Json::Array(tools)) = obj.get_mut("tools") {
        for tool in tools {
            if let Json::Object(t) = tool {
                for field in strip {
                    if t.remove(*field).is_some() {
                        ignored.ignore(
                            &format!("tools.{field}"),
                            "the upstream documents this tool field as unsupported",
                        );
                    }
                }
            }
        }
    }
    let stream = obj.get("stream").and_then(Json::as_bool).unwrap_or(false);
    Ok(UpstreamRequest {
        path: path.to_owned(),
        body: serde_json::to_vec(&Json::Object(obj)).expect("a JSON value serialises"),
        stream,
        tool_names: ToolNames::new(),
        ignored: ignored.into_ignored(),
    })
}

/// What a passthrough reply says, for logging and the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReplySummary {
    /// Token usage.
    pub usage: Usage,
    /// The `stop_reason`.
    pub stop_reason: Option<String>,
}

fn usage_of(u: &Json) -> Usage {
    let n = |k: &str| u.get(k).and_then(Json::as_u64).unwrap_or(0);
    Usage {
        input_tokens: n("input_tokens"),
        output_tokens: n("output_tokens"),
        cache_read_input_tokens: n("cache_read_input_tokens"),
        cache_creation_input_tokens: n("cache_creation_input_tokens"),
        reasoning_tokens: u
            .pointer("/output_tokens_details/thinking_tokens")
            .and_then(Json::as_u64)
            .unwrap_or(0),
    }
}

/// Rewrites a non-streaming reply so it echoes the model the client named, and summarises it.
/// A body that is not JSON is returned unchanged.
#[must_use]
pub fn rewrite_response(body: &[u8], client_model: &str) -> (Vec<u8>, ReplySummary) {
    let Ok(Json::Object(mut obj)) = serde_json::from_slice::<Json>(body) else {
        return (body.to_vec(), ReplySummary::default());
    };
    let summary = ReplySummary {
        usage: obj.get("usage").map(usage_of).unwrap_or_default(),
        stop_reason: obj
            .get("stop_reason")
            .and_then(Json::as_str)
            .map(str::to_owned),
    };
    if obj.contains_key("model") {
        obj.insert("model".to_owned(), Json::String(client_model.to_owned()));
    }
    (
        serde_json::to_vec(&Json::Object(obj)).expect("a JSON value serialises"),
        summary,
    )
}

/// Re-frames a passthrough stream: `message_start` echoes the client's model name, and a stream
/// that ends without `message_stop` ends with an `error` event instead.
#[derive(Debug)]
pub struct PassthroughStream {
    sse: SseParser,
    client_model: String,
    saw_stop: bool,
    errored: bool,
    summary: ReplySummary,
}

impl PassthroughStream {
    /// A rewriter echoing `client_model`.
    #[must_use]
    pub fn new(client_model: &str) -> Self {
        Self {
            sse: SseParser::new(),
            client_model: client_model.to_owned(),
            saw_stop: false,
            errored: false,
            summary: ReplySummary::default(),
        }
    }

    /// What the stream has said so far.
    #[must_use]
    pub fn summary(&self) -> &ReplySummary {
        &self.summary
    }

    /// Whether the stream ended properly (`message_stop`).
    #[must_use]
    pub fn completed(&self) -> bool {
        self.saw_stop
    }

    fn handle(&mut self, events: Vec<crate::sse::SseEvent>) -> Vec<u8> {
        let mut out = Vec::new();
        for ev in events {
            let name = ev.event.clone();
            let mut data = ev.data;
            if let Ok(mut json) = serde_json::from_str::<Json>(&data) {
                let ty = json.get("type").and_then(Json::as_str).map(str::to_owned);
                match ty.as_deref() {
                    Some("message_start") => {
                        if let Some(u) = json.pointer("/message/usage") {
                            self.summary.usage = usage_of(u);
                        }
                        if let Some(m) = json.pointer_mut("/message/model") {
                            *m = Json::String(self.client_model.clone());
                        }
                        data = json.to_string();
                    }
                    Some("message_delta") => {
                        if let Some(u) = json.get("usage") {
                            let u = usage_of(u);
                            self.summary.usage.output_tokens =
                                u.output_tokens.max(self.summary.usage.output_tokens);
                            if u.input_tokens > 0 {
                                self.summary.usage.input_tokens = u.input_tokens;
                            }
                        }
                        if let Some(r) = json.pointer("/delta/stop_reason").and_then(Json::as_str) {
                            self.summary.stop_reason = Some(r.to_owned());
                        }
                    }
                    Some("message_stop") => self.saw_stop = true,
                    Some("error") => self.errored = true,
                    _ => {}
                }
            }
            out.extend(format_event(name.as_deref(), &data));
        }
        out
    }

    /// Feeds upstream bytes; returns the bytes to send the client.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        match self.sse.push(bytes) {
            Ok(events) => self.handle(events),
            Err(e) => self.error_event(&e.to_string()),
        }
    }

    /// Ends the stream; returns any final bytes, including an `error` event if it was cut off.
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = match self.sse.finish() {
            Ok(events) => self.handle(events),
            Err(e) => self.error_event(&e.to_string()),
        };
        if !self.saw_stop && !self.errored {
            out.extend(self.error_event(&StreamError::truncated().message));
        }
        out
    }

    fn error_event(&mut self, message: &str) -> Vec<u8> {
        self.errored = true;
        let data = serde_json::json!({"type": "error", "error": {"type": "api_error", "message": message}});
        format_event(Some("error"), &data.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flavor::Flavor;
    use serde_json::json;

    fn fw() -> Target {
        Target::new(
            "fw",
            Protocol::Anthropic,
            Flavor::Fireworks,
            "accounts/fireworks/models/glm-5p1",
        )
    }

    fn body() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "model": "claude-opus-4", "max_tokens": 64, "stream": true,
            "inference_geo": "us",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "12345678"}, {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}]}],
            "tools": [{"name": "t", "description": "dd", "input_schema": {"type": "object"}, "cache_control": {"type": "ephemeral"}, "eager_input_streaming": true, "allowed_callers": ["direct"], "input_examples": [{}]}],
            "thinking": {"type": "enabled", "budget_tokens": 1024},
            "output_config": {"effort": "high", "format": {"type": "json_schema", "schema": {}}},
            "safeguards": {"anything": true}
        })).unwrap()
    }

    #[test]
    fn peek_reads_routing_facts_without_judging_the_rest() {
        let f = peek(&body()).unwrap();
        assert_eq!(f.model, "claude-opus-4");
        assert!(f.stream && f.tools && f.images && f.thinking && f.structured_output);
        assert_eq!(f.effort, Some(Effort::High));
        assert_eq!(f.max_tokens, 64);
        assert!(f.est_input_tokens >= TOKENS_PER_IMAGE + TOKENS_PER_MESSAGE + TOKENS_PER_TOOL);
        assert_eq!(f.inbound, Protocol::Anthropic);
    }

    #[test]
    fn peek_on_a_bare_request() {
        let f = peek(br#"{"model":"m"}"#).unwrap();
        assert!(!f.stream && !f.tools && !f.images && !f.thinking);
        assert_eq!(f.max_tokens, 0);
        assert_eq!(
            peek(br#"{"model":"m","system":"abcdefgh"}"#)
                .unwrap()
                .est_input_tokens,
            2
        );
        assert!(peek(b"[]").is_err() && peek(b"x").is_err());
        assert_eq!(peek(b"{}").unwrap_err().field, "model");
    }

    #[test]
    fn passthrough_maps_the_model_and_strips_documented_unsupported_fields() {
        let u = build_passthrough(&body(), &fw()).unwrap();
        let b: Json = serde_json::from_slice(&u.body).unwrap();
        assert_eq!(b["model"], "accounts/fireworks/models/glm-5p1");
        assert!(b.get("inference_geo").is_none());
        let tool = b["tools"][0].as_object().unwrap();
        for stripped in [
            "cache_control",
            "eager_input_streaming",
            "allowed_callers",
            "input_examples",
        ] {
            assert!(!tool.contains_key(stripped), "{stripped}");
        }
        assert_eq!(tool["description"], "dd");
        // Everything else, including a field the IR parser would reject, is forwarded untouched.
        assert!(b.get("safeguards").is_some());
        assert_eq!(
            b["thinking"],
            json!({"type": "enabled", "budget_tokens": 1024})
        );
        let names: Vec<_> = u.ignored.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "inference_geo",
                "tools.cache_control",
                "tools.eager_input_streaming",
                "tools.allowed_callers",
                "tools.input_examples"
            ]
        );
        assert_eq!(u.path, "/v1/messages");
        assert!(u.stream);
    }

    #[test]
    fn other_flavors_forward_everything() {
        let t = Target::new("a", Protocol::Anthropic, Flavor::Generic, "claude-x");
        let u = build_passthrough(&body(), &t).unwrap();
        assert!(u.ignored.is_empty());
        let b: Json = serde_json::from_slice(&u.body).unwrap();
        assert!(b.get("inference_geo").is_some());
        assert_eq!(
            build_count_tokens(&body(), &t).unwrap().path,
            "/v1/messages/count_tokens"
        );
        assert!(build_passthrough(b"[1]", &t).is_err());
    }

    #[test]
    fn non_streaming_replies_echo_the_client_model() {
        let reply = json!({"id": "msg_1", "model": "glm-5p1", "stop_reason": "end_turn", "content": [], "usage": {"input_tokens": 3, "output_tokens": 4, "cache_read_input_tokens": 1}});
        let (out, s) = rewrite_response(&serde_json::to_vec(&reply).unwrap(), "claude-opus-4");
        let o: Json = serde_json::from_slice(&out).unwrap();
        assert_eq!(o["model"], "claude-opus-4");
        assert_eq!(s.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(
            (
                s.usage.input_tokens,
                s.usage.output_tokens,
                s.usage.cache_read_input_tokens
            ),
            (3, 4, 1)
        );
        let (raw, s) = rewrite_response(b"not json", "m");
        assert_eq!(raw, b"not json");
        assert_eq!(s, ReplySummary::default());
        let (out, _) = rewrite_response(br#"{"error":"x"}"#, "m");
        assert!(!String::from_utf8(out).unwrap().contains("\"model\""));
    }

    fn stream_wire() -> String {
        [
            format_event(Some("message_start"), &json!({"type": "message_start", "message": {"id": "m", "model": "glm", "usage": {"input_tokens": 12, "output_tokens": 1}}}).to_string()),
            format_event(Some("content_block_delta"), &json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hi"}}).to_string()),
            format_event(Some("message_delta"), &json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 9}}).to_string()),
            format_event(Some("message_stop"), &json!({"type": "message_stop"}).to_string()),
        ]
        .into_iter()
        .map(|b| String::from_utf8(b).unwrap())
        .collect()
    }

    #[test]
    fn streams_are_forwarded_with_the_model_rewritten_and_summarised() {
        let wire = stream_wire();
        for size in [wire.len(), 1, 7] {
            let mut p = PassthroughStream::new("claude-opus-4");
            let mut out = Vec::new();
            for c in wire.as_bytes().chunks(size) {
                out.extend(p.push(c));
            }
            out.extend(p.finish());
            let text = String::from_utf8(out).unwrap();
            assert!(text.contains("\"model\":\"claude-opus-4\""), "{size}");
            assert!(!text.contains("glm"));
            assert!(text.contains("event: message_stop"));
            assert!(!text.contains("event: error"));
            assert!(p.completed());
            assert_eq!(p.summary().stop_reason.as_deref(), Some("end_turn"));
            assert_eq!(
                (
                    p.summary().usage.input_tokens,
                    p.summary().usage.output_tokens
                ),
                (12, 9)
            );
        }
    }

    #[test]
    fn a_cut_off_stream_ends_with_an_error_event_not_a_stop() {
        let wire = stream_wire();
        let cut = &wire[..wire.find("event: message_delta").unwrap()];
        let mut p = PassthroughStream::new("m");
        let mut out = p.push(cut.as_bytes());
        out.extend(p.finish());
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("event: error") && text.contains("before the response was complete"));
        assert!(!text.contains("message_stop") && !p.completed());
        assert!(p.finish().is_empty(), "the error is reported once");
    }

    #[test]
    fn an_upstream_error_event_is_not_followed_by_a_second_one() {
        let wire = format_event(
            Some("error"),
            &json!({"type": "error", "error": {"type": "overloaded_error", "message": "busy"}})
                .to_string(),
        );
        let mut p = PassthroughStream::new("m");
        let mut out = p.push(&wire);
        out.extend(p.finish());
        assert_eq!(
            String::from_utf8(out)
                .unwrap()
                .matches("event: error")
                .count(),
            1
        );
    }

    #[test]
    fn oversized_input_ends_the_stream_with_an_error() {
        let mut p = PassthroughStream::new("m");
        p.sse = SseParser::with_limits(8, 8);
        let out = String::from_utf8(p.push(b"data: 0123456789abcdef\n\n")).unwrap();
        assert!(out.contains("event: error"));
        let mut p = PassthroughStream::new("m");
        p.sse = SseParser::with_limits(8, 8);
        assert!(String::from_utf8(p.finish())
            .unwrap()
            .contains("event: error"));
    }

    #[test]
    fn non_json_events_pass_through_verbatim() {
        let mut p = PassthroughStream::new("m");
        let out = p.push(b"event: ping\ndata: hello\n\n");
        assert_eq!(out, b"event: ping\ndata: hello\n\n");
    }
}
