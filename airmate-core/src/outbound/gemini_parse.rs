//! The Google Gemini `generateContent` outbound adapter: reading replies.
//!
//! Both the whole reply and each server-sent chunk of `streamGenerateContent?alt=sse` are a
//! `GenerateContentResponse`; the stream delivers one per event, with the final one carrying the
//! finish reason. The stream has no end marker, so a stream that stops before a finish reason is
//! truncated.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::Value as Json;

use super::{ParseCtx, UpstreamError};
use crate::ir::{FinishReason, IrCompletion, Part, ReasoningDisplay, Usage};
use crate::sse::SseParser;
use crate::stream::{IrEvent, StreamError, StreamErrorKind, StreamParse};

/// Maps a Gemini `finishReason`.
#[must_use]
pub fn finish_reason(reason: &str) -> FinishReason {
    match reason {
        "MAX_TOKENS" => FinishReason::Length,
        "SAFETY"
        | "RECITATION"
        | "BLOCKLIST"
        | "PROHIBITED_CONTENT"
        | "SPII"
        | "IMAGE_SAFETY"
        | "IMAGE_PROHIBITED_CONTENT"
        | "LANGUAGE" => FinishReason::Refusal,
        _ => FinishReason::Stop,
    }
}

/// Maps `usageMetadata` to the Anthropic shape: `input_tokens` excludes cached tokens, and the
/// output includes the thinking tokens Gemini counts separately.
#[must_use]
pub fn usage(u: &Json) -> Usage {
    let n = |k: &str| u.get(k).and_then(Json::as_u64).unwrap_or(0);
    let cached = n("cachedContentTokenCount");
    let thoughts = n("thoughtsTokenCount");
    Usage {
        input_tokens: n("promptTokenCount").saturating_sub(cached),
        output_tokens: n("candidatesTokenCount") + thoughts,
        cache_read_input_tokens: cached,
        cache_creation_input_tokens: 0,
        reasoning_tokens: thoughts,
    }
}

fn signature(part: &Json) -> Option<Vec<u8>> {
    part.get("thoughtSignature")
        .and_then(Json::as_str)
        .and_then(|s| STANDARD.decode(s).ok())
}

/// A reason a prompt was refused, if the reply carries one instead of candidates.
fn block_reason(json: &Json) -> Option<String> {
    let r = json
        .pointer("/promptFeedback/blockReason")
        .and_then(Json::as_str)?;
    Some(format!(
        "the prompt was blocked by Gemini ({})",
        r.to_ascii_lowercase().replace('_', " ")
    ))
}

/// Reads a non-streaming reply.
///
/// # Errors
///
/// An [`UpstreamError`] for a non-2xx status or a reply that is not a valid response.
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
    if let Some(err) = json.get("error").filter(|e| !e.is_null()) {
        let raw = serde_json::to_vec(&serde_json::json!({ "error": err })).unwrap_or_default();
        return Err(UpstreamError::from_response(502, &raw));
    }
    let usage = json.get("usageMetadata").map(usage).unwrap_or_default();
    let mut c = IrCompletion {
        content: Vec::new(),
        finish: FinishReason::Stop,
        usage,
        upstream_id: json
            .get("responseId")
            .and_then(Json::as_str)
            .map(str::to_owned),
        upstream_model: json
            .get("modelVersion")
            .and_then(Json::as_str)
            .map(str::to_owned),
    };
    let Some(candidate) = json.pointer("/candidates/0") else {
        let why = block_reason(&json)
            .ok_or_else(|| UpstreamError::unreadable("the upstream reply has no candidates"))?;
        c.content.push(Part::Text(why));
        c.finish = FinishReason::Refusal;
        return Ok(c);
    };

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut sig: Option<Vec<u8>> = None;
    let mut calls = Vec::new();
    let parts = candidate.pointer("/content/parts").and_then(Json::as_array);
    for (i, p) in parts.into_iter().flatten().enumerate() {
        if sig.is_none() {
            sig = signature(p);
        }
        if let Some(fc) = p.get("functionCall") {
            let name = fc.get("name").and_then(Json::as_str).unwrap_or("");
            calls.push(Part::ToolCall {
                id: fc
                    .get("id")
                    .and_then(Json::as_str)
                    .map_or_else(|| format!("call_{i}"), str::to_owned),
                name: ctx.tool_names.restore(name).to_owned(),
                input: fc
                    .get("args")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
            });
        } else if let Some(t) = p.get("text").and_then(Json::as_str) {
            if p.get("thought").and_then(Json::as_bool) == Some(true) {
                if !reasoning.is_empty() {
                    reasoning.push('\n');
                }
                reasoning.push_str(t);
            } else {
                text.push_str(t);
            }
        }
    }
    if !reasoning.is_empty() || sig.is_some() {
        c.content.push(Part::Reasoning {
            text: reasoning,
            display: ReasoningDisplay::Summarized,
            origin: Some(ctx.origin.clone()),
            opaque: sig,
        });
    }
    if !text.is_empty() {
        c.content.push(Part::Text(text));
    }
    let has_calls = !calls.is_empty();
    c.content.extend(calls);
    let reason = candidate
        .get("finishReason")
        .and_then(Json::as_str)
        .map_or(FinishReason::Stop, finish_reason);
    c.finish = if reason == FinishReason::Stop && has_calls {
        FinishReason::ToolCalls
    } else {
        reason
    };
    Ok(c)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seg {
    Text,
    Reasoning,
}

/// A parser for `streamGenerateContent?alt=sse`.
#[derive(Debug)]
pub struct GeminiStream {
    sse: SseParser,
    ctx: ParseCtx,
    started: bool,
    next_block: u32,
    seg: Option<(Seg, u32)>,
    signature: Option<Vec<u8>>,
    saw_tool_calls: bool,
    finish: Option<FinishReason>,
    usage: Option<Usage>,
    terminal: bool,
}

impl GeminiStream {
    /// A parser for the reply to a request described by `ctx`.
    #[must_use]
    pub fn new(ctx: ParseCtx) -> Self {
        Self {
            sse: SseParser::new(),
            ctx,
            started: false,
            next_block: 0,
            seg: None,
            signature: None,
            saw_tool_calls: false,
            finish: None,
            usage: None,
            terminal: false,
        }
    }

    fn new_block(&mut self) -> u32 {
        let b = self.next_block;
        self.next_block += 1;
        b
    }

    fn close_segment(&mut self, out: &mut Vec<IrEvent>) {
        match self.seg.take() {
            Some((Seg::Text, block)) => out.push(IrEvent::TextEnd { block }),
            Some((Seg::Reasoning, block)) => out.push(IrEvent::ReasoningSealed {
                block,
                opaque: self.signature.take(),
            }),
            None => {}
        }
    }

    fn piece(&mut self, kind: Seg, text: &str, out: &mut Vec<IrEvent>) {
        if text.is_empty() {
            return;
        }
        if self.seg.map(|(k, _)| k) != Some(kind) {
            self.close_segment(out);
            let block = self.new_block();
            self.seg = Some((kind, block));
        }
        let block = self.seg.map_or(0, |(_, b)| b);
        out.push(match kind {
            Seg::Text => IrEvent::TextDelta {
                block,
                text: text.to_owned(),
            },
            Seg::Reasoning => IrEvent::ReasoningDelta {
                block,
                text: text.to_owned(),
            },
        });
    }

    fn call(&mut self, fc: &Json, out: &mut Vec<IrEvent>) {
        self.close_segment(out);
        let block = self.new_block();
        let name = self
            .ctx
            .tool_names
            .restore(fc.get("name").and_then(Json::as_str).unwrap_or(""))
            .to_owned();
        let id = fc
            .get("id")
            .and_then(Json::as_str)
            .map_or_else(|| format!("call_{block}"), str::to_owned);
        let args = fc
            .get("args")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        self.saw_tool_calls = true;
        out.push(IrEvent::ToolCallStart { block, id, name });
        out.push(IrEvent::ToolCallArgs {
            block,
            fragment: args.to_string(),
        });
        out.push(IrEvent::ToolCallEnd { block });
    }

    fn complete(&mut self, out: &mut Vec<IrEvent>) {
        self.close_segment(out);
        if let Some(u) = self.usage.take() {
            out.push(IrEvent::Usage(u));
        }
        let mut reason = self.finish.unwrap_or(FinishReason::Stop);
        if reason == FinishReason::Stop && self.saw_tool_calls {
            reason = FinishReason::ToolCalls;
        }
        out.push(IrEvent::Finish(reason));
        self.terminal = true;
    }

    fn fail(&mut self, kind: StreamErrorKind, message: String, out: &mut Vec<IrEvent>) {
        out.push(IrEvent::Error(StreamError { kind, message }));
        self.terminal = true;
    }

    fn chunk(&mut self, json: &Json, out: &mut Vec<IrEvent>) {
        if let Some(err) = json.get("error").filter(|e| !e.is_null()) {
            let message = err
                .get("message")
                .and_then(Json::as_str)
                .unwrap_or("the upstream reported an error")
                .to_owned();
            self.fail(StreamErrorKind::Upstream, message, out);
            return;
        }
        if !self.started {
            self.started = true;
            out.push(IrEvent::Start {
                id: json
                    .get("responseId")
                    .and_then(Json::as_str)
                    .map(str::to_owned),
                model: json
                    .get("modelVersion")
                    .and_then(Json::as_str)
                    .map(str::to_owned),
            });
        }
        if let Some(u) = json.get("usageMetadata").filter(|u| u.is_object()) {
            self.usage = Some(usage(u));
        }
        let Some(candidate) = json.pointer("/candidates/0") else {
            if let Some(why) = block_reason(json) {
                out.push(IrEvent::Refusal { text: why });
                self.finish = Some(FinishReason::Refusal);
                self.complete(out);
            }
            return;
        };
        for p in candidate
            .pointer("/content/parts")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
        {
            if self.signature.is_none() {
                self.signature = signature(p);
            }
            if let Some(fc) = p.get("functionCall") {
                self.call(fc, out);
            } else if let Some(t) = p.get("text").and_then(Json::as_str) {
                let kind = if p.get("thought").and_then(Json::as_bool) == Some(true) {
                    Seg::Reasoning
                } else {
                    Seg::Text
                };
                self.piece(kind, t, out);
            }
        }
        if let Some(reason) = candidate.get("finishReason").and_then(Json::as_str) {
            self.finish = Some(finish_reason(reason));
            self.complete(out);
        }
    }

    fn handle(&mut self, events: Vec<crate::sse::SseEvent>, out: &mut Vec<IrEvent>) {
        for ev in events {
            if self.terminal {
                return;
            }
            let data = ev.data.trim();
            if data.is_empty() {
                continue;
            }
            match serde_json::from_str::<Json>(data) {
                Ok(j) => self.chunk(&j, out),
                Err(e) => self.fail(
                    StreamErrorKind::Protocol,
                    format!("the upstream sent an unreadable stream chunk: {e}"),
                    out,
                ),
            }
        }
    }
}

impl StreamParse for GeminiStream {
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
    use crate::toolname::ToolNames;
    use serde_json::json;

    fn ctx() -> ParseCtx {
        ParseCtx {
            origin: Origin {
                upstream: "g".into(),
                protocol: Protocol::Gemini,
            },
            tool_names: ToolNames::new(),
            think_tags: true,
        }
    }

    fn body(v: &Json) -> Vec<u8> {
        serde_json::to_vec(v).unwrap()
    }

    fn reply(parts: Json, finish: &str) -> Json {
        json!({
            "candidates": [{"content": {"role": "model", "parts": parts}, "finishReason": finish, "index": 0}],
            "usageMetadata": {"promptTokenCount": 100, "cachedContentTokenCount": 30, "candidatesTokenCount": 20, "thoughtsTokenCount": 5, "totalTokenCount": 125},
            "modelVersion": "gemini-2.5-pro", "responseId": "resp-1"
        })
    }

    #[test]
    fn text_usage_and_ids_are_read() {
        let c = parse_response(
            200,
            &body(&reply(json!([{"text": "hel"}, {"text": "lo"}]), "STOP")),
            &ctx(),
        )
        .unwrap();
        assert_eq!(c.content, vec![Part::Text("hello".into())]);
        assert_eq!(c.finish, FinishReason::Stop);
        assert_eq!(c.upstream_id.as_deref(), Some("resp-1"));
        assert_eq!(c.upstream_model.as_deref(), Some("gemini-2.5-pro"));
        assert_eq!(
            c.usage,
            Usage {
                input_tokens: 70,
                output_tokens: 25,
                cache_read_input_tokens: 30,
                cache_creation_input_tokens: 0,
                reasoning_tokens: 5
            }
        );
    }

    #[test]
    fn thoughts_become_reasoning_with_the_signature_kept() {
        let c = parse_response(
            200,
            &body(&reply(json!([{"text": "pondering", "thought": true}, {"text": "answer", "thoughtSignature": "AQID"}]), "STOP")),
            &ctx(),
        )
        .unwrap();
        assert_eq!(
            c.content,
            vec![
                Part::Reasoning {
                    text: "pondering".into(),
                    display: ReasoningDisplay::Summarized,
                    origin: Some(ctx().origin),
                    opaque: Some(vec![1, 2, 3])
                },
                Part::Text("answer".into()),
            ]
        );
    }

    #[test]
    fn function_calls_become_tool_calls_and_set_the_finish_reason() {
        let c = parse_response(
            200,
            &body(&reply(json!([{"functionCall": {"id": "x1", "name": "get_weather", "args": {"city": "Paris"}}}, {"functionCall": {"name": "ping", "args": {}}}]), "STOP")),
            &ctx(),
        )
        .unwrap();
        assert_eq!(c.finish, FinishReason::ToolCalls);
        assert_eq!(
            c.content[0],
            Part::ToolCall {
                id: "x1".into(),
                name: "get_weather".into(),
                input: json!({"city": "Paris"})
            }
        );
        assert_eq!(
            c.content[1],
            Part::ToolCall {
                id: "call_1".into(),
                name: "ping".into(),
                input: json!({})
            },
            "a missing id is made up"
        );
    }

    #[test]
    fn finish_reasons_map() {
        for (r, f) in [
            ("STOP", FinishReason::Stop),
            ("MAX_TOKENS", FinishReason::Length),
            ("SAFETY", FinishReason::Refusal),
            ("RECITATION", FinishReason::Refusal),
            ("OTHER", FinishReason::Stop),
            ("MALFORMED_FUNCTION_CALL", FinishReason::Stop),
        ] {
            assert_eq!(finish_reason(r), f, "{r}");
        }
    }

    #[test]
    fn a_blocked_prompt_is_a_refusal_with_the_reason() {
        let c = parse_response(200, &body(&json!({"promptFeedback": {"blockReason": "PROHIBITED_CONTENT"}, "usageMetadata": {"promptTokenCount": 3}})), &ctx()).unwrap();
        assert_eq!(c.finish, FinishReason::Refusal);
        assert_eq!(
            c.content,
            vec![Part::Text(
                "the prompt was blocked by Gemini (prohibited content)".into()
            )]
        );
        assert!(parse_response(200, &body(&json!({})), &ctx()).is_err());
    }

    #[test]
    fn errors_are_read_from_the_status_and_from_a_200_with_an_error_body() {
        let e = parse_response(429, &body(&json!({"error": {"code": 429, "message": "quota", "status": "RESOURCE_EXHAUSTED"}})), &ctx()).unwrap_err();
        assert_eq!((e.status, e.message.as_str()), (429, "quota"));
        let e = parse_response(
            200,
            &body(&json!({"error": {"message": "late failure"}})),
            &ctx(),
        )
        .unwrap_err();
        assert_eq!((e.status, e.message.as_str()), (502, "late failure"));
        assert!(parse_response(200, b"nope", &ctx()).is_err());
    }

    fn chunk(v: &Json) -> Vec<u8> {
        format!("data: {v}\r\n\r\n").into_bytes()
    }

    fn run(chunks: &[Vec<u8>]) -> Vec<IrEvent> {
        let mut p = GeminiStream::new(ctx());
        let mut out = Vec::new();
        for c in chunks {
            out.extend(p.push(c));
        }
        out.extend(p.finish());
        out
    }

    #[test]
    fn a_text_stream_becomes_deltas_usage_and_a_finish() {
        let ev = run(&[
            chunk(
                &json!({"candidates": [{"content": {"parts": [{"text": "Hel"}]}}], "modelVersion": "gemini-2.5-flash", "responseId": "r"}),
            ),
            chunk(&reply(json!([{"text": "lo"}]), "STOP")),
        ]);
        assert_eq!(
            ev,
            vec![
                IrEvent::Start {
                    id: Some("r".into()),
                    model: Some("gemini-2.5-flash".into())
                },
                IrEvent::TextDelta {
                    block: 0,
                    text: "Hel".into()
                },
                IrEvent::TextDelta {
                    block: 0,
                    text: "lo".into()
                },
                IrEvent::TextEnd { block: 0 },
                IrEvent::Usage(Usage {
                    input_tokens: 70,
                    output_tokens: 25,
                    cache_read_input_tokens: 30,
                    cache_creation_input_tokens: 0,
                    reasoning_tokens: 5
                }),
                IrEvent::Finish(FinishReason::Stop),
            ]
        );
    }

    #[test]
    fn thoughts_then_text_then_a_call_keep_their_order_and_the_signature() {
        let ev = run(&[
            chunk(
                &json!({"candidates": [{"content": {"parts": [{"text": "hm", "thought": true}]}}]}),
            ),
            chunk(
                &json!({"candidates": [{"content": {"parts": [{"text": "ok", "thoughtSignature": "AQID"}]}}]}),
            ),
            chunk(
                &json!({"candidates": [{"content": {"parts": [{"functionCall": {"name": "t", "args": {"a": 1}}}]}, "finishReason": "STOP"}]}),
            ),
        ]);
        assert_eq!(
            ev,
            vec![
                IrEvent::Start {
                    id: None,
                    model: None
                },
                IrEvent::ReasoningDelta {
                    block: 0,
                    text: "hm".into()
                },
                IrEvent::ReasoningSealed {
                    block: 0,
                    opaque: Some(vec![1, 2, 3])
                },
                IrEvent::TextDelta {
                    block: 1,
                    text: "ok".into()
                },
                IrEvent::TextEnd { block: 1 },
                IrEvent::ToolCallStart {
                    block: 2,
                    id: "call_2".into(),
                    name: "t".into()
                },
                IrEvent::ToolCallArgs {
                    block: 2,
                    fragment: "{\"a\":1}".into()
                },
                IrEvent::ToolCallEnd { block: 2 },
                IrEvent::Finish(FinishReason::ToolCalls),
            ]
        );
    }

    #[test]
    fn a_stream_with_no_finish_reason_is_truncated_never_complete() {
        let ev = run(&[chunk(
            &json!({"candidates": [{"content": {"parts": [{"text": "partial"}]}}]}),
        )]);
        assert!(
            matches!(
                ev.last(),
                Some(IrEvent::Error(StreamError {
                    kind: StreamErrorKind::Truncated,
                    ..
                }))
            ),
            "{ev:?}"
        );
        assert!(!ev.iter().any(|e| matches!(e, IrEvent::Finish(_))));
    }

    #[test]
    fn an_error_chunk_and_a_garbled_chunk_end_the_stream_with_an_error() {
        let ev = run(&[chunk(
            &json!({"error": {"code": 503, "message": "overloaded"}}),
        )]);
        assert!(
            matches!(&ev[..], [IrEvent::Error(StreamError { kind: StreamErrorKind::Upstream, message })] if message == "overloaded"),
            "{ev:?}"
        );
        let ev = run(&[b"data: {not json\n\n".to_vec()]);
        assert!(
            matches!(
                &ev[..],
                [IrEvent::Error(StreamError {
                    kind: StreamErrorKind::Protocol,
                    ..
                })]
            ),
            "{ev:?}"
        );
    }

    #[test]
    fn a_blocked_prompt_in_a_stream_refuses_and_finishes() {
        let ev = run(&[chunk(&json!({"promptFeedback": {"blockReason": "SAFETY"}}))]);
        assert!(
            ev.contains(&IrEvent::Finish(FinishReason::Refusal)),
            "{ev:?}"
        );
        assert!(ev.iter().any(|e| matches!(e, IrEvent::Refusal { .. })));
    }

    #[test]
    fn any_chunking_of_the_bytes_gives_the_same_events() {
        let whole: Vec<u8> = [
            chunk(&json!({"candidates": [{"content": {"parts": [{"text": "a"}]}}]})),
            chunk(&reply(json!([{"text": "b"}]), "STOP")),
        ]
        .concat();
        let expected = run(std::slice::from_ref(&whole));
        for step in [1, 2, 3, 7, 50] {
            let pieces: Vec<Vec<u8>> = whole.chunks(step).map(<[u8]>::to_vec).collect();
            assert_eq!(run(&pieces), expected, "step {step}");
        }
    }
}
