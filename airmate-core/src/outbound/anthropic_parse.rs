//! The Anthropic outbound adapter: reading Messages replies and streams into the IR.

use std::collections::HashMap;

use serde_json::Value as Json;

use super::anthropic_build::REDACTED_PREFIX;
use super::{ParseCtx, UpstreamError};
use crate::ir::{FinishReason, IrCompletion, Part, ReasoningDisplay, Usage};
use crate::sse::{SseEvent, SseParser};
use crate::stream::{IrEvent, StreamError, StreamErrorKind, StreamParse};

/// Maps an Anthropic `stop_reason`.
#[must_use]
pub fn finish_reason(reason: &str) -> FinishReason {
    match reason {
        "max_tokens" | "model_context_window_exceeded" => FinishReason::Length,
        "tool_use" => FinishReason::ToolCalls,
        "refusal" => FinishReason::Refusal,
        _ => FinishReason::Stop,
    }
}

fn usage(u: &Json) -> Usage {
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

/// Reads a non-streaming Messages reply.
///
/// # Errors
///
/// An [`UpstreamError`] for a non-2xx status, an error body, a server-tool block (which airmate
/// does not emulate) or an unreadable reply.
pub fn parse_response(
    status: u16,
    body: &[u8],
    ctx: &ParseCtx,
) -> Result<IrCompletion, UpstreamError> {
    if !(200..300).contains(&status) {
        return Err(UpstreamError::from_response(status, body));
    }
    let json: Json = serde_json::from_slice(body).map_err(|e| {
        UpstreamError::unreadable(format!("the upstream reply is not valid JSON: {e}"))
    })?;
    if json.get("type").and_then(Json::as_str) == Some("error") {
        return Err(UpstreamError::from_response(502, body));
    }
    let blocks = json
        .get("content")
        .and_then(Json::as_array)
        .ok_or_else(|| UpstreamError::unreadable("the upstream reply has no content"))?;
    let mut content = Vec::new();
    for b in blocks {
        match b.get("type").and_then(Json::as_str) {
            Some("text") => {
                let t = b.get("text").and_then(Json::as_str).unwrap_or("");
                if !t.is_empty() {
                    content.push(Part::Text(t.to_owned()));
                }
            }
            Some("thinking") => content.push(Part::Reasoning {
                text: b
                    .get("thinking")
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .to_owned(),
                display: ReasoningDisplay::Summarized,
                origin: Some(ctx.origin.clone()),
                opaque: b
                    .get("signature")
                    .and_then(Json::as_str)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.as_bytes().to_vec()),
            }),
            Some("redacted_thinking") => {
                let mut opaque = REDACTED_PREFIX.to_vec();
                opaque.extend(
                    b.get("data")
                        .and_then(Json::as_str)
                        .unwrap_or("")
                        .as_bytes(),
                );
                content.push(Part::Reasoning {
                    text: String::new(),
                    display: ReasoningDisplay::Omitted,
                    origin: Some(ctx.origin.clone()),
                    opaque: Some(opaque),
                });
            }
            Some("tool_use") => content.push(Part::ToolCall {
                id: b.get("id").and_then(Json::as_str).unwrap_or("").to_owned(),
                name: ctx
                    .tool_names
                    .restore(b.get("name").and_then(Json::as_str).unwrap_or(""))
                    .to_owned(),
                input: b
                    .get("input")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
            }),
            Some(other) => {
                return Err(UpstreamError::unreadable(format!(
                    "the upstream returned a `{other}` block, which airmate cannot translate"
                )));
            }
            None => {}
        }
    }
    Ok(IrCompletion {
        content,
        finish: finish_reason(
            json.get("stop_reason")
                .and_then(Json::as_str)
                .unwrap_or("end_turn"),
        ),
        usage: json.get("usage").map(usage).unwrap_or_default(),
        upstream_id: json.get("id").and_then(Json::as_str).map(str::to_owned),
        upstream_model: json.get("model").and_then(Json::as_str).map(str::to_owned),
    })
}

// ---- streaming ------------------------------------------------------------------------------

#[derive(Debug)]
enum Kind {
    Text,
    Reasoning { signature: String },
    Redacted(String),
    Tool,
    Skipped,
}

/// A parser for Anthropic Messages server-sent events.
#[derive(Debug)]
pub struct AnthropicStream {
    sse: SseParser,
    ctx: ParseCtx,
    blocks: HashMap<u64, Kind>,
    usage: Usage,
    stop_reason: Option<String>,
    started: bool,
    terminal: bool,
}

impl AnthropicStream {
    /// A parser for the reply to a request described by `ctx`.
    #[must_use]
    pub fn new(ctx: ParseCtx) -> Self {
        Self {
            sse: SseParser::new(),
            ctx,
            blocks: HashMap::new(),
            usage: Usage::default(),
            stop_reason: None,
            started: false,
            terminal: false,
        }
    }

    fn fail(&mut self, kind: StreamErrorKind, message: String, out: &mut Vec<IrEvent>) {
        out.push(IrEvent::Error(StreamError { kind, message }));
        self.terminal = true;
    }

    fn event(&mut self, ty: &str, j: &Json, out: &mut Vec<IrEvent>) {
        let index = j.get("index").and_then(Json::as_u64).unwrap_or(0);
        let block = u32::try_from(index).unwrap_or(u32::MAX);
        match ty {
            "message_start" => {
                self.started = true;
                let m = j.get("message").unwrap_or(&Json::Null);
                if let Some(u) = m.get("usage") {
                    self.usage = usage(u);
                }
                out.push(IrEvent::Start {
                    id: m.get("id").and_then(Json::as_str).map(str::to_owned),
                    model: m.get("model").and_then(Json::as_str).map(str::to_owned),
                });
            }
            "content_block_start" => {
                let b = j.get("content_block").unwrap_or(&Json::Null);
                let kind = match b.get("type").and_then(Json::as_str) {
                    Some("text") => {
                        if let Some(t) = b
                            .get("text")
                            .and_then(Json::as_str)
                            .filter(|t| !t.is_empty())
                        {
                            out.push(IrEvent::TextDelta {
                                block,
                                text: t.to_owned(),
                            });
                        }
                        Kind::Text
                    }
                    Some("thinking") => {
                        if let Some(t) = b
                            .get("thinking")
                            .and_then(Json::as_str)
                            .filter(|t| !t.is_empty())
                        {
                            out.push(IrEvent::ReasoningDelta {
                                block,
                                text: t.to_owned(),
                            });
                        }
                        Kind::Reasoning {
                            signature: b
                                .get("signature")
                                .and_then(Json::as_str)
                                .unwrap_or("")
                                .to_owned(),
                        }
                    }
                    Some("redacted_thinking") => Kind::Redacted(
                        b.get("data")
                            .and_then(Json::as_str)
                            .unwrap_or("")
                            .to_owned(),
                    ),
                    Some("tool_use") => {
                        let name = b.get("name").and_then(Json::as_str).unwrap_or("");
                        out.push(IrEvent::ToolCallStart {
                            block,
                            id: b.get("id").and_then(Json::as_str).unwrap_or("").to_owned(),
                            name: self.ctx.tool_names.restore(name).to_owned(),
                        });
                        Kind::Tool
                    }
                    Some(other) => {
                        self.fail(
                            StreamErrorKind::Protocol,
                            format!("the upstream streamed a `{other}` block, which airmate cannot translate"),
                            out,
                        );
                        Kind::Skipped
                    }
                    None => Kind::Skipped,
                };
                self.blocks.insert(index, kind);
            }
            "content_block_delta" => {
                let d = j.get("delta").unwrap_or(&Json::Null);
                match (
                    self.blocks.get_mut(&index),
                    d.get("type").and_then(Json::as_str),
                ) {
                    (Some(Kind::Text), Some("text_delta")) => {
                        let t = d.get("text").and_then(Json::as_str).unwrap_or("");
                        if !t.is_empty() {
                            out.push(IrEvent::TextDelta {
                                block,
                                text: t.to_owned(),
                            });
                        }
                    }
                    (Some(Kind::Reasoning { .. }), Some("thinking_delta")) => {
                        let t = d.get("thinking").and_then(Json::as_str).unwrap_or("");
                        if !t.is_empty() {
                            out.push(IrEvent::ReasoningDelta {
                                block,
                                text: t.to_owned(),
                            });
                        }
                    }
                    (Some(Kind::Reasoning { signature }), Some("signature_delta")) => {
                        signature.push_str(d.get("signature").and_then(Json::as_str).unwrap_or(""));
                    }
                    (Some(Kind::Tool), Some("input_json_delta")) => {
                        let f = d.get("partial_json").and_then(Json::as_str).unwrap_or("");
                        if !f.is_empty() {
                            out.push(IrEvent::ToolCallArgs {
                                block,
                                fragment: f.to_owned(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => match self.blocks.remove(&index) {
                Some(Kind::Text) => out.push(IrEvent::TextEnd { block }),
                Some(Kind::Reasoning { signature }) => out.push(IrEvent::ReasoningSealed {
                    block,
                    opaque: (!signature.is_empty()).then(|| signature.into_bytes()),
                }),
                Some(Kind::Redacted(data)) => {
                    let mut opaque = REDACTED_PREFIX.to_vec();
                    opaque.extend(data.as_bytes());
                    out.push(IrEvent::ReasoningSealed {
                        block,
                        opaque: Some(opaque),
                    });
                }
                Some(Kind::Tool) => out.push(IrEvent::ToolCallEnd { block }),
                Some(Kind::Skipped) | None => {}
            },
            "message_delta" => {
                if let Some(r) = j.pointer("/delta/stop_reason").and_then(Json::as_str) {
                    self.stop_reason = Some(r.to_owned());
                }
                if let Some(u) = j.get("usage") {
                    let u = usage(u);
                    self.usage.output_tokens = u.output_tokens.max(self.usage.output_tokens);
                    if u.input_tokens > 0 {
                        self.usage.input_tokens = u.input_tokens;
                    }
                    if u.cache_read_input_tokens > 0 {
                        self.usage.cache_read_input_tokens = u.cache_read_input_tokens;
                    }
                    if u.cache_creation_input_tokens > 0 {
                        self.usage.cache_creation_input_tokens = u.cache_creation_input_tokens;
                    }
                    if u.reasoning_tokens > 0 {
                        self.usage.reasoning_tokens = u.reasoning_tokens;
                    }
                }
            }
            "message_stop" => {
                out.push(IrEvent::Usage(self.usage));
                out.push(IrEvent::Finish(finish_reason(
                    self.stop_reason.as_deref().unwrap_or("end_turn"),
                )));
                self.terminal = true;
            }
            "error" => {
                let message = j
                    .pointer("/error/message")
                    .and_then(Json::as_str)
                    .unwrap_or("the upstream reported an error")
                    .to_owned();
                self.fail(StreamErrorKind::Upstream, message, out);
            }
            _ => {}
        }
    }

    fn handle(&mut self, events: Vec<SseEvent>, out: &mut Vec<IrEvent>) {
        for ev in events {
            if self.terminal {
                return;
            }
            if ev.data.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Json>(&ev.data) {
                Ok(j) => {
                    let ty = j
                        .get("type")
                        .and_then(Json::as_str)
                        .map(str::to_owned)
                        .or(ev.event)
                        .unwrap_or_default();
                    self.event(&ty, &j, out);
                }
                Err(e) => self.fail(
                    StreamErrorKind::Protocol,
                    format!("the upstream sent an unreadable stream event: {e}"),
                    out,
                ),
            }
        }
    }
}

impl StreamParse for AnthropicStream {
    fn push(&mut self, bytes: &[u8]) -> Vec<IrEvent> {
        let mut out = Vec::new();
        if self.terminal {
            return out;
        }
        match self.sse.push(bytes) {
            Ok(events) => self.handle(events, &mut out),
            Err(e) => self.fail(StreamErrorKind::TooLarge, e.to_string(), &mut out),
        }
        out
    }

    fn finish(&mut self) -> Vec<IrEvent> {
        let mut out = Vec::new();
        if self.terminal {
            return out;
        }
        match self.sse.finish() {
            Ok(events) => self.handle(events, &mut out),
            Err(e) => self.fail(StreamErrorKind::TooLarge, e.to_string(), &mut out),
        }
        if !self.terminal {
            out.push(IrEvent::Error(StreamError::truncated()));
            self.terminal = true;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Origin, Protocol};
    use crate::sse::format_event;
    use crate::toolname::ToolNames;
    use serde_json::json;

    fn ctx() -> ParseCtx {
        ParseCtx {
            origin: Origin {
                upstream: "fw".into(),
                protocol: Protocol::Anthropic,
            },
            tool_names: ToolNames::new(),
            think_tags: true,
        }
    }

    fn body(v: Json) -> Vec<u8> {
        serde_json::to_vec(&v).unwrap()
    }

    #[test]
    fn parses_every_block_type_and_usage() {
        let r = body(
            json!({"id": "msg_1", "type": "message", "model": "glm", "stop_reason": "tool_use",
            "content": [
                {"type": "thinking", "thinking": "hm", "signature": "SIG"},
                {"type": "redacted_thinking", "data": "ENC"},
                {"type": "text", "text": "ok"},
                {"type": "text", "text": ""},
                {"type": "tool_use", "id": "t1", "name": "read", "input": {"p": 1}}],
            "usage": {"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 3, "cache_creation_input_tokens": 2, "output_tokens_details": {"thinking_tokens": 4}}}),
        );
        let c = parse_response(200, &r, &ctx()).unwrap();
        assert!(
            matches!(&c.content[0], Part::Reasoning { text, opaque: Some(o), origin: Some(og), .. } if text == "hm" && o == b"SIG" && og.upstream == "fw")
        );
        assert!(
            matches!(&c.content[1], Part::Reasoning { display: ReasoningDisplay::Omitted, opaque: Some(o), .. } if o == b"redacted:ENC")
        );
        assert_eq!(c.content[2], Part::Text("ok".into()));
        assert_eq!(
            c.content[3],
            Part::ToolCall {
                id: "t1".into(),
                name: "read".into(),
                input: json!({"p": 1})
            }
        );
        assert_eq!(c.content.len(), 4);
        assert_eq!(c.finish, FinishReason::ToolCalls);
        assert_eq!(
            c.usage,
            Usage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_input_tokens: 3,
                cache_creation_input_tokens: 2,
                reasoning_tokens: 4
            }
        );
        assert_eq!(
            (c.upstream_id.as_deref(), c.upstream_model.as_deref()),
            (Some("msg_1"), Some("glm"))
        );
    }

    #[test]
    fn stop_reasons_map() {
        for (s, want) in [
            ("end_turn", FinishReason::Stop),
            ("stop_sequence", FinishReason::Stop),
            ("pause_turn", FinishReason::Stop),
            ("max_tokens", FinishReason::Length),
            ("model_context_window_exceeded", FinishReason::Length),
            ("tool_use", FinishReason::ToolCalls),
            ("refusal", FinishReason::Refusal),
        ] {
            assert_eq!(finish_reason(s), want, "{s}");
        }
    }

    #[test]
    fn errors_server_tools_and_garbage_are_reported() {
        assert_eq!(
            parse_response(
                529,
                br#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#,
                &ctx()
            )
            .unwrap_err()
            .status,
            529
        );
        assert_eq!(
            parse_response(
                200,
                br#"{"type":"error","error":{"type":"api_error","message":"in-band"}}"#,
                &ctx()
            )
            .unwrap_err()
            .message,
            "in-band"
        );
        let server_tool = body(
            json!({"content": [{"type": "server_tool_use", "id": "x"}], "stop_reason": "end_turn"}),
        );
        assert!(parse_response(200, &server_tool, &ctx())
            .unwrap_err()
            .message
            .contains("server_tool_use"));
        assert!(parse_response(200, b"nope", &ctx()).is_err());
        assert!(parse_response(200, b"{}", &ctx())
            .unwrap_err()
            .message
            .contains("no content"));
    }

    #[test]
    fn tool_names_are_restored() {
        let mut names = ToolNames::new();
        let long = "z".repeat(90);
        let short = names.shorten(&long);
        let mut c = ctx();
        c.tool_names = names;
        let r = body(
            json!({"content": [{"type": "tool_use", "id": "t", "name": short, "input": {}}], "stop_reason": "tool_use"}),
        );
        assert_eq!(
            parse_response(200, &r, &c).unwrap().content[0],
            Part::ToolCall {
                id: "t".into(),
                name: long,
                input: json!({})
            }
        );
    }

    // ---- streaming ----

    fn ev(ty: &str, mut d: Json) -> String {
        d["type"] = json!(ty);
        String::from_utf8(format_event(Some(ty), &d.to_string())).unwrap()
    }

    fn wire() -> String {
        [
            ev("message_start", json!({"message": {"id": "m1", "model": "glm", "usage": {"input_tokens": 12, "output_tokens": 1}}})),
            ev("ping", json!({})),
            ev("content_block_start", json!({"index": 0, "content_block": {"type": "thinking", "thinking": ""}})),
            ev("content_block_delta", json!({"index": 0, "delta": {"type": "thinking_delta", "thinking": "hmm"}})),
            ev("content_block_delta", json!({"index": 0, "delta": {"type": "signature_delta", "signature": "SI"}})),
            ev("content_block_delta", json!({"index": 0, "delta": {"type": "signature_delta", "signature": "G"}})),
            ev("content_block_stop", json!({"index": 0})),
            ev("content_block_start", json!({"index": 1, "content_block": {"type": "text", "text": ""}})),
            ev("content_block_delta", json!({"index": 1, "delta": {"type": "text_delta", "text": "Hello"}})),
            ev("content_block_stop", json!({"index": 1})),
            ev("content_block_start", json!({"index": 2, "content_block": {"type": "tool_use", "id": "t1", "name": "read", "input": {}}})),
            ev("content_block_delta", json!({"index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"p\":"}})),
            ev("content_block_delta", json!({"index": 2, "delta": {"type": "input_json_delta", "partial_json": "1}"}})),
            ev("content_block_stop", json!({"index": 2})),
            ev("content_block_start", json!({"index": 3, "content_block": {"type": "redacted_thinking", "data": "RD"}})),
            ev("content_block_stop", json!({"index": 3})),
            ev("message_delta", json!({"delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 9}})),
            ev("message_stop", json!({})),
        ]
        .concat()
    }

    fn run_split(w: &str, size: usize) -> Vec<IrEvent> {
        let mut p = AnthropicStream::new(ctx());
        let mut out = Vec::new();
        for c in w.as_bytes().chunks(size.max(1)) {
            out.extend(p.push(c));
        }
        out.extend(p.finish());
        out
    }

    #[test]
    fn a_full_stream_becomes_ir_events() {
        let w = wire();
        let events = run_split(&w, w.len());
        assert!(
            matches!(&events[0], IrEvent::Start { id: Some(i), model: Some(m) } if i == "m1" && m == "glm")
        );
        assert!(events.contains(&IrEvent::ReasoningDelta {
            block: 0,
            text: "hmm".into()
        }));
        assert!(
            events.contains(&IrEvent::ReasoningSealed {
                block: 0,
                opaque: Some(b"SIG".to_vec())
            }),
            "the signature deltas are joined"
        );
        assert!(events.contains(&IrEvent::TextDelta {
            block: 1,
            text: "Hello".into()
        }));
        assert!(events.contains(&IrEvent::TextEnd { block: 1 }));
        assert!(events.contains(&IrEvent::ToolCallStart {
            block: 2,
            id: "t1".into(),
            name: "read".into()
        }));
        assert!(events.contains(&IrEvent::ToolCallArgs {
            block: 2,
            fragment: "1}".into()
        }));
        assert!(events.contains(&IrEvent::ToolCallEnd { block: 2 }));
        assert!(events.contains(&IrEvent::ReasoningSealed {
            block: 3,
            opaque: Some(b"redacted:RD".to_vec())
        }));
        let n = events.len();
        assert_eq!(
            events[n - 2],
            IrEvent::Usage(Usage {
                input_tokens: 12,
                output_tokens: 9,
                ..Usage::default()
            })
        );
        assert_eq!(events[n - 1], IrEvent::Finish(FinishReason::ToolCalls));
    }

    #[test]
    fn every_chunk_size_gives_the_same_events() {
        let w = wire();
        let whole = run_split(&w, w.len());
        for size in [1, 2, 5, 17, 100] {
            assert_eq!(run_split(&w, size), whole, "chunk size {size}");
        }
    }

    #[test]
    fn a_cut_off_stream_is_truncated_and_errors_end_it() {
        let w = wire();
        let cut = &w[..w.find("event: message_delta").unwrap()];
        assert_eq!(
            *run_split(cut, cut.len()).last().unwrap(),
            IrEvent::Error(StreamError::truncated())
        );
        let err = ev(
            "error",
            json!({"error": {"type": "overloaded_error", "message": "busy"}}),
        );
        assert_eq!(
            *run_split(&err, err.len()).last().unwrap(),
            IrEvent::Error(StreamError {
                kind: StreamErrorKind::Upstream,
                message: "busy".into()
            })
        );
        assert!(matches!(
            run_split("data: {oops\n\n", 100).last(),
            Some(IrEvent::Error(StreamError {
                kind: StreamErrorKind::Protocol,
                ..
            }))
        ));
        let server_block = ev(
            "content_block_start",
            json!({"index": 0, "content_block": {"type": "server_tool_use"}}),
        );
        assert!(matches!(
            run_split(&server_block, 1000).last(),
            Some(IrEvent::Error(StreamError {
                kind: StreamErrorKind::Protocol,
                ..
            }))
        ));
        assert_eq!(
            run_split("", 1),
            vec![IrEvent::Error(StreamError::truncated())]
        );
    }

    #[test]
    fn nothing_follows_the_terminal_event() {
        let mut p = AnthropicStream::new(ctx());
        let _ = p.push(ev("message_stop", json!({})).as_bytes());
        assert!(p
            .push(
                ev(
                    "content_block_start",
                    json!({"index": 0, "content_block": {"type": "text"}})
                )
                .as_bytes()
            )
            .is_empty());
        assert!(p.finish().is_empty());
        let mut p = AnthropicStream::new(ctx());
        p.sse = SseParser::with_limits(8, 8);
        assert!(matches!(
            p.push(b"data: 0123456789abc\n\n").last(),
            Some(IrEvent::Error(StreamError {
                kind: StreamErrorKind::TooLarge,
                ..
            }))
        ));
    }

    #[test]
    fn tool_names_are_restored_on_the_stream() {
        let mut names = ToolNames::new();
        let long = "w".repeat(90);
        let short = names.shorten(&long);
        let mut c = ctx();
        c.tool_names = names;
        let w = ev(
            "content_block_start",
            json!({"index": 0, "content_block": {"type": "tool_use", "id": "t", "name": short}}),
        );
        let mut p = AnthropicStream::new(c);
        assert!(p.push(w.as_bytes()).contains(&IrEvent::ToolCallStart {
            block: 0,
            id: "t".into(),
            name: long
        }));
    }
}
