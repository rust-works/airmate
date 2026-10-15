//! The OpenAI Chat Completions outbound adapter: reading replies.

use std::collections::BTreeMap;

use serde_json::Value as Json;

use super::{ParseCtx, UpstreamError};
use crate::ir::{FinishReason, IrCompletion, Part, ReasoningDisplay, Usage};
use crate::sse::SseParser;
use crate::stream::think::{merge, Piece, ThinkSplitter};
use crate::stream::{IrEvent, JsonCompletion, StreamError, StreamErrorKind, StreamParse};

/// Maps a Chat `finish_reason`.
#[must_use]
pub fn finish_reason(reason: &str) -> FinishReason {
    match reason {
        "length" => FinishReason::Length,
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
        "content_filter" => FinishReason::Refusal,
        _ => FinishReason::Stop,
    }
}

/// Maps Chat usage to the Anthropic shape: `input_tokens` excludes cached tokens, which are
/// reported separately; no OpenAI-protocol upstream reports cache writes.
#[must_use]
pub fn usage(u: &Json) -> Usage {
    let n = |v: Option<&Json>| v.and_then(Json::as_u64).unwrap_or(0);
    let prompt = n(u.get("prompt_tokens"));
    let cached = n(u.pointer("/prompt_tokens_details/cached_tokens"))
        .max(n(u.get("prompt_cache_hit_tokens")));
    Usage {
        input_tokens: prompt.saturating_sub(cached),
        output_tokens: n(u.get("completion_tokens")),
        cache_read_input_tokens: cached,
        cache_creation_input_tokens: 0,
        reasoning_tokens: n(u.pointer("/completion_tokens_details/reasoning_tokens")),
    }
}

/// Reads a non-streaming Chat Completions reply.
///
/// # Errors
///
/// An [`UpstreamError`] for a non-2xx status or a reply that is not a valid completion.
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
    let choice = json
        .pointer("/choices/0")
        .ok_or_else(|| UpstreamError::unreadable("the upstream reply has no choices"))?;
    let message = choice.get("message").unwrap_or(&Json::Null);

    let mut content = Vec::new();
    let mut reasoning = message
        .get("reasoning_content")
        .or_else(|| message.get("reasoning"))
        .and_then(Json::as_str)
        .unwrap_or("")
        .to_owned();
    let mut text = String::new();
    if let Some(c) = message.get("content").and_then(Json::as_str) {
        for piece in merge(crate::stream::think::split_all(c, ctx.think_tags)) {
            match piece {
                Piece::Reasoning(r) => {
                    if !reasoning.is_empty() {
                        reasoning.push('\n');
                    }
                    reasoning.push_str(&r);
                }
                Piece::Text(t) => text.push_str(&t),
            }
        }
    }
    if !reasoning.is_empty() {
        content.push(Part::Reasoning {
            text: reasoning,
            display: ReasoningDisplay::Summarized,
            origin: Some(ctx.origin.clone()),
            opaque: None,
        });
    }
    let refusal = message
        .get("refusal")
        .and_then(Json::as_str)
        .filter(|r| !r.is_empty());
    if let Some(r) = refusal {
        text.push_str(r);
    }
    if !text.is_empty() {
        content.push(Part::Text(text));
    }
    let mut has_calls = false;
    if let Some(calls) = message.get("tool_calls").and_then(Json::as_array) {
        for (i, c) in calls.iter().enumerate() {
            let f = c.get("function").unwrap_or(&Json::Null);
            let name = f.get("name").and_then(Json::as_str).unwrap_or("");
            let args = f.get("arguments").and_then(Json::as_str).unwrap_or("");
            let input = if args.trim().is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(args).map_err(|e| {
                    UpstreamError::unreadable(format!(
                        "tool call `{name}` has invalid JSON arguments: {e}"
                    ))
                })?
            };
            let id = c
                .get("id")
                .and_then(Json::as_str)
                .map_or_else(|| format!("call_{i}"), str::to_owned);
            content.push(Part::ToolCall {
                id,
                name: ctx.tool_names.restore(name).to_owned(),
                input,
            });
            has_calls = true;
        }
    }

    let reason = choice
        .get("finish_reason")
        .and_then(Json::as_str)
        .unwrap_or("stop");
    let mut finish = finish_reason(reason);
    if refusal.is_some() {
        finish = FinishReason::Refusal;
    } else if has_calls && finish == FinishReason::Stop {
        finish = FinishReason::ToolCalls;
    }
    Ok(IrCompletion {
        content,
        finish,
        usage: json
            .get("usage")
            .filter(|u| !u.is_null())
            .map(usage)
            .unwrap_or_default(),
        upstream_id: json.get("id").and_then(Json::as_str).map(str::to_owned),
        upstream_model: json.get("model").and_then(Json::as_str).map(str::to_owned),
    })
}

// ---- streaming ------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SegKind {
    Text,
    Reasoning,
}

#[derive(Debug)]
struct Tool {
    block: u32,
    id: Option<String>,
    name: Option<String>,
    pending_args: String,
    started: bool,
    ended: bool,
    json: JsonCompletion,
}

/// A parser for Chat Completions server-sent events.
#[derive(Debug)]
pub struct ChatStream {
    sse: SseParser,
    ctx: ParseCtx,
    splitter: ThinkSplitter,
    started: bool,
    next_block: u32,
    seg: Option<(SegKind, u32)>,
    tools: BTreeMap<u64, Tool>,
    saw_tool_calls: bool,
    finish: Option<FinishReason>,
    usage: Option<Usage>,
    terminal: bool,
}

impl ChatStream {
    /// A parser for the reply to a request described by `ctx`.
    #[must_use]
    pub fn new(ctx: ParseCtx) -> Self {
        let splitter = if ctx.think_tags {
            ThinkSplitter::new()
        } else {
            ThinkSplitter::disabled()
        };
        Self {
            sse: SseParser::new(),
            ctx,
            splitter,
            started: false,
            next_block: 0,
            seg: None,
            tools: BTreeMap::new(),
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
            Some((SegKind::Text, block)) => out.push(IrEvent::TextEnd { block }),
            Some((SegKind::Reasoning, block)) => out.push(IrEvent::ReasoningSealed {
                block,
                opaque: None,
            }),
            None => {}
        }
    }

    fn piece(&mut self, piece: Piece, out: &mut Vec<IrEvent>) {
        let (kind, text) = match piece {
            Piece::Text(t) => (SegKind::Text, t),
            Piece::Reasoning(r) => (SegKind::Reasoning, r),
        };
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
            SegKind::Text => IrEvent::TextDelta { block, text },
            SegKind::Reasoning => IrEvent::ReasoningDelta { block, text },
        });
    }

    fn tool_delta(&mut self, position: usize, tc: &Json, out: &mut Vec<IrEvent>) {
        let index = tc
            .get("index")
            .and_then(Json::as_u64)
            .unwrap_or(position as u64);
        if !self.tools.contains_key(&index) {
            let block = self.new_block();
            self.tools.insert(
                index,
                Tool {
                    block,
                    id: None,
                    name: None,
                    pending_args: String::new(),
                    started: false,
                    ended: false,
                    json: JsonCompletion::new(),
                },
            );
        }
        let f = tc.get("function").unwrap_or(&Json::Null);
        let id = tc
            .get("id")
            .and_then(Json::as_str)
            .filter(|s| !s.is_empty());
        let name = f
            .get("name")
            .and_then(Json::as_str)
            .filter(|s| !s.is_empty());
        let args = f.get("arguments").and_then(Json::as_str).unwrap_or("");

        let needs_close = {
            let t = self.tools.get_mut(&index).expect("inserted above");
            if let Some(id) = id {
                t.id = Some(id.to_owned());
            }
            if let Some(name) = name {
                t.name = Some(name.to_owned());
            }
            t.pending_args.push_str(args);
            !t.started && t.name.is_some()
        };
        if needs_close {
            self.close_segment(out);
            let t = self.tools.get_mut(&index).expect("inserted above");
            let name = self
                .ctx
                .tool_names
                .restore(t.name.as_deref().unwrap_or(""))
                .to_owned();
            let id = t.id.clone().unwrap_or_else(|| format!("call_{}", t.block));
            t.started = true;
            self.saw_tool_calls = true;
            out.push(IrEvent::ToolCallStart {
                block: t.block,
                id,
                name,
            });
        }
        let t = self.tools.get_mut(&index).expect("inserted above");
        if t.started && !t.pending_args.is_empty() {
            let fragment = std::mem::take(&mut t.pending_args);
            let done = !t.ended && t.json.feed(&fragment);
            out.push(IrEvent::ToolCallArgs {
                block: t.block,
                fragment,
            });
            if done {
                t.ended = true;
                out.push(IrEvent::ToolCallEnd { block: t.block });
            }
        }
    }

    /// Closes everything still open: the splitter's held text, the segment, and unfinished calls.
    fn close_all(&mut self, out: &mut Vec<IrEvent>) {
        for p in merge(self.splitter.finish()) {
            self.piece(p, out);
        }
        self.close_segment(out);
        for t in self.tools.values_mut() {
            if t.started && !t.ended {
                t.ended = true;
                out.push(IrEvent::ToolCallEnd { block: t.block });
            }
        }
    }

    fn complete(&mut self, out: &mut Vec<IrEvent>) {
        self.close_all(out);
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
                .or_else(|| err.as_str())
                .unwrap_or("the upstream reported an error")
                .to_owned();
            self.fail(StreamErrorKind::Upstream, message, out);
            return;
        }
        if !self.started {
            self.started = true;
            out.push(IrEvent::Start {
                id: json.get("id").and_then(Json::as_str).map(str::to_owned),
                model: json.get("model").and_then(Json::as_str).map(str::to_owned),
            });
        }
        if let Some(u) = json.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(usage(u));
        }
        let Some(choice) = json.pointer("/choices/0") else {
            return;
        };
        let delta = choice.get("delta").unwrap_or(&Json::Null);

        if let Some(r) = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(Json::as_str)
        {
            self.piece(Piece::Reasoning(r.to_owned()), out);
        }
        if let Some(c) = delta.get("content").and_then(Json::as_str) {
            for p in self.splitter.push(c) {
                self.piece(p, out);
            }
        }
        if let Some(r) = delta
            .get("refusal")
            .and_then(Json::as_str)
            .filter(|r| !r.is_empty())
        {
            self.close_segment(out);
            out.push(IrEvent::Refusal { text: r.to_owned() });
            self.finish = Some(FinishReason::Refusal);
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Json::as_array) {
            // Held `<think>` text must not be reordered behind a tool call.
            for p in merge(self.splitter.finish()) {
                self.piece(p, out);
            }
            for (i, tc) in calls.iter().enumerate() {
                self.tool_delta(i, tc, out);
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Json::as_str) {
            self.close_all(out);
            let mapped = finish_reason(reason);
            if self.finish != Some(FinishReason::Refusal) {
                self.finish = Some(mapped);
            }
        }
    }

    fn handle(&mut self, events: Vec<crate::sse::SseEvent>, out: &mut Vec<IrEvent>) {
        for ev in events {
            if self.terminal {
                return;
            }
            let data = ev.data.trim();
            if data == "[DONE]" {
                self.complete(out);
            } else if data.is_empty() {
                continue;
            } else {
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
}

impl StreamParse for ChatStream {
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
            if self.finish.is_some() {
                self.complete(&mut out);
            } else {
                out.push(IrEvent::Error(StreamError::truncated()));
                self.terminal = true;
            }
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
                upstream: "up".into(),
                protocol: Protocol::Chat,
            },
            tool_names: ToolNames::new(),
            think_tags: true,
        }
    }

    fn body(v: Json) -> Vec<u8> {
        serde_json::to_vec(&v).unwrap()
    }

    fn completion(message: Json, finish: &str) -> Vec<u8> {
        body(
            json!({"id": "chatcmpl-1", "model": "m-up", "choices": [{"index": 0, "message": message, "finish_reason": finish}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 20, "prompt_tokens_details": {"cached_tokens": 30}, "completion_tokens_details": {"reasoning_tokens": 5}}}),
        )
    }

    #[test]
    fn parses_text_usage_and_ids() {
        let c = parse_response(
            200,
            &completion(json!({"role": "assistant", "content": "hello"}), "stop"),
            &ctx(),
        )
        .unwrap();
        assert_eq!(c.content, vec![Part::Text("hello".into())]);
        assert_eq!(c.finish, FinishReason::Stop);
        assert_eq!(c.upstream_id.as_deref(), Some("chatcmpl-1"));
        assert_eq!(c.upstream_model.as_deref(), Some("m-up"));
        assert_eq!(
            c.usage,
            Usage {
                input_tokens: 70,
                output_tokens: 20,
                cache_read_input_tokens: 30,
                cache_creation_input_tokens: 0,
                reasoning_tokens: 5
            }
        );
    }

    #[test]
    fn usage_is_optional_and_cached_tokens_never_underflow() {
        let b = body(json!({"choices": [{"message": {"content": "x"}, "finish_reason": "stop"}]}));
        assert_eq!(
            parse_response(200, &b, &ctx()).unwrap().usage,
            Usage::default()
        );
        let u = usage(&json!({"prompt_tokens": 5, "prompt_tokens_details": {"cached_tokens": 9}}));
        assert_eq!((u.input_tokens, u.cache_read_input_tokens), (0, 9));
        assert_eq!(
            usage(&json!({"prompt_tokens": 10, "prompt_cache_hit_tokens": 4}))
                .cache_read_input_tokens,
            4
        );
    }

    #[test]
    fn finish_reasons_map_to_the_neutral_vocabulary() {
        for (up, want) in [
            ("stop", FinishReason::Stop),
            ("length", FinishReason::Length),
            ("tool_calls", FinishReason::ToolCalls),
            ("function_call", FinishReason::ToolCalls),
            ("content_filter", FinishReason::Refusal),
            ("anything_else", FinishReason::Stop),
        ] {
            assert_eq!(finish_reason(up), want, "{up}");
        }
    }

    #[test]
    fn tool_calls_are_parsed_restored_and_force_tool_use() {
        let mut names = ToolNames::new();
        let long = "x".repeat(80);
        let short = names.shorten(&long);
        let mut c = ctx();
        c.tool_names = names;
        let msg = json!({"content": null, "tool_calls": [
            {"id": "c1", "type": "function", "function": {"name": short, "arguments": "{\"a\":1}"}},
            {"function": {"name": "empty", "arguments": ""}}
        ]});
        let r = parse_response(200, &completion(msg, "stop"), &c).unwrap();
        assert_eq!(r.finish, FinishReason::ToolCalls);
        assert_eq!(
            r.content[0],
            Part::ToolCall {
                id: "c1".into(),
                name: long,
                input: json!({"a": 1})
            }
        );
        assert_eq!(
            r.content[1],
            Part::ToolCall {
                id: "call_1".into(),
                name: "empty".into(),
                input: json!({})
            }
        );
    }

    #[test]
    fn invalid_tool_arguments_are_an_error_not_a_guess() {
        let msg =
            json!({"tool_calls": [{"id": "c", "function": {"name": "n", "arguments": "{oops"}}]});
        let err = parse_response(200, &completion(msg, "tool_calls"), &ctx()).unwrap_err();
        assert_eq!(err.status, 502);
        assert!(err.message.contains("invalid JSON arguments"));
    }

    #[test]
    fn reasoning_comes_from_either_field_or_think_tags() {
        for field in ["reasoning_content", "reasoning"] {
            let r = parse_response(
                200,
                &completion(json!({"content": "ans", field: "why"}), "stop"),
                &ctx(),
            )
            .unwrap();
            assert!(
                matches!(&r.content[0], Part::Reasoning { text, origin: Some(o), .. } if text == "why" && o.upstream == "up")
            );
            assert_eq!(r.content[1], Part::Text("ans".into()));
        }
        let r = parse_response(
            200,
            &completion(json!({"content": "<think>inner</think>\n\nans"}), "stop"),
            &ctx(),
        )
        .unwrap();
        assert!(matches!(&r.content[0], Part::Reasoning { text, .. } if text == "inner"));
        assert_eq!(r.content[1], Part::Text("ans".into()));
        let mut off = ctx();
        off.think_tags = false;
        let r = parse_response(
            200,
            &completion(json!({"content": "<think>inner</think>ans"}), "stop"),
            &off,
        )
        .unwrap();
        assert_eq!(
            r.content,
            vec![Part::Text("<think>inner</think>ans".into())]
        );
    }

    #[test]
    fn refusals_finish_as_refusal() {
        let r = parse_response(
            200,
            &completion(json!({"content": null, "refusal": "I can't"}), "stop"),
            &ctx(),
        )
        .unwrap();
        assert_eq!(r.finish, FinishReason::Refusal);
        assert_eq!(r.content, vec![Part::Text("I can't".into())]);
        let r = parse_response(
            200,
            &completion(json!({"content": ""}), "content_filter"),
            &ctx(),
        )
        .unwrap();
        assert_eq!(r.finish, FinishReason::Refusal);
    }

    #[test]
    fn errors_and_garbage_are_reported() {
        let err = parse_response(429, br#"{"error":{"message":"slow down"}}"#, &ctx()).unwrap_err();
        assert_eq!((err.status, err.message.as_str()), (429, "slow down"));
        assert!(parse_response(200, b"not json", &ctx())
            .unwrap_err()
            .message
            .contains("not valid JSON"));
        assert!(parse_response(200, br#"{"choices":[]}"#, &ctx())
            .unwrap_err()
            .message
            .contains("no choices"));
        assert_eq!(
            parse_response(200, br#"{"error":{"message":"in-band"}}"#, &ctx())
                .unwrap_err()
                .message,
            "in-band"
        );
    }

    // ---- streaming ----

    fn sse(chunks: &[Json]) -> String {
        let mut s = String::new();
        for c in chunks {
            s.push_str(&format!("data: {c}\n\n"));
        }
        s
    }

    fn run(wire: &str) -> Vec<IrEvent> {
        run_split(wire, wire.len())
    }

    fn run_split(wire: &str, size: usize) -> Vec<IrEvent> {
        let mut p = ChatStream::new(ctx());
        let mut out = Vec::new();
        for c in wire.as_bytes().chunks(size.max(1)) {
            out.extend(p.push(c));
        }
        out.extend(p.finish());
        out
    }

    fn delta(d: Json) -> Json {
        json!({"id": "s1", "model": "m", "choices": [{"index": 0, "delta": d, "finish_reason": null}]})
    }

    fn finish_chunk(reason: &str) -> Json {
        json!({"id": "s1", "choices": [{"index": 0, "delta": {}, "finish_reason": reason}]})
    }

    fn text_of(events: &[IrEvent]) -> String {
        events
            .iter()
            .filter_map(|e| {
                if let IrEvent::TextDelta { text, .. } = e {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect()
    }

    #[test]
    fn streams_text_then_finishes_with_trailing_usage() {
        let wire = sse(&[
            delta(json!({"role": "assistant", "content": ""})),
            delta(json!({"content": "Hel"})),
            delta(json!({"content": "lo"})),
            finish_chunk("stop"),
            json!({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 2}}),
        ]) + "data: [DONE]\n\n";
        let ev = run(&wire);
        assert!(
            matches!(&ev[0], IrEvent::Start { id: Some(i), model: Some(m) } if i == "s1" && m == "m")
        );
        assert_eq!(text_of(&ev), "Hello");
        let n = ev.len();
        assert_eq!(ev[n - 3], IrEvent::TextEnd { block: 0 });
        assert!(matches!(
            ev[n - 2],
            IrEvent::Usage(Usage {
                input_tokens: 10,
                output_tokens: 2,
                ..
            })
        ));
        assert_eq!(ev[n - 1], IrEvent::Finish(FinishReason::Stop));
    }

    #[test]
    fn every_chunk_size_gives_the_same_events() {
        let wire = sse(&[
            delta(json!({"reasoning_content": "think "})),
            delta(json!({"reasoning_content": "more"})),
            delta(json!({"content": "<think>t</think>ans"})),
            delta(
                json!({"tool_calls": [{"index": 0, "id": "c", "function": {"name": "n", "arguments": "{\"a\""}}]}),
            ),
            delta(json!({"tool_calls": [{"index": 0, "function": {"arguments": ":1}"}}]})),
            finish_chunk("tool_calls"),
        ]) + "data: [DONE]\n\n";
        let whole = run(&wire);
        for size in [1, 2, 3, 5, 7, 11, 64] {
            assert_eq!(run_split(&wire, size), whole, "chunk size {size}");
        }
    }

    #[test]
    fn reasoning_then_text_are_separate_blocks() {
        let wire = sse(&[
            delta(json!({"reasoning_content": "hm"})),
            delta(json!({"content": "ok"})),
            finish_chunk("stop"),
        ]) + "data: [DONE]\n\n";
        let ev = run(&wire);
        assert!(ev.contains(&IrEvent::ReasoningDelta {
            block: 0,
            text: "hm".into()
        }));
        assert!(ev.contains(&IrEvent::ReasoningSealed {
            block: 0,
            opaque: None
        }));
        assert!(ev.contains(&IrEvent::TextDelta {
            block: 1,
            text: "ok".into()
        }));
    }

    #[test]
    fn inline_think_tags_split_across_chunks() {
        let wire = sse(&[
            delta(json!({"content": "<thi"})),
            delta(json!({"content": "nk>why</th"})),
            delta(json!({"content": "ink>\n\nanswer"})),
            finish_chunk("stop"),
        ]) + "data: [DONE]\n\n";
        let ev = run(&wire);
        let reasoning: String = ev
            .iter()
            .filter_map(|e| {
                if let IrEvent::ReasoningDelta { text, .. } = e {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(reasoning, "why");
        assert_eq!(text_of(&ev), "answer");
    }

    #[test]
    fn tool_calls_stream_with_restored_names_and_end_when_their_json_closes() {
        let mut names = ToolNames::new();
        let long = "y".repeat(90);
        let short = names.shorten(&long);
        let mut c = ctx();
        c.tool_names = names;
        let wire = sse(&[
            delta(
                json!({"tool_calls": [{"index": 0, "id": "c0", "function": {"name": short, "arguments": ""}}]}),
            ),
            delta(json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"x\":"}}]})),
            delta(json!({"tool_calls": [{"index": 0, "function": {"arguments": "1}"}}]})),
            delta(
                json!({"tool_calls": [{"index": 1, "id": "c1", "function": {"name": "b", "arguments": "{}"}}]}),
            ),
            finish_chunk("tool_calls"),
        ]) + "data: [DONE]\n\n";
        let mut p = ChatStream::new(c);
        let mut ev = p.push(wire.as_bytes());
        ev.extend(p.finish());
        let pos = |want: &IrEvent| {
            ev.iter()
                .position(|e| e == want)
                .unwrap_or_else(|| panic!("missing {want:?}"))
        };
        assert!(ev.contains(&IrEvent::ToolCallStart {
            block: 0,
            id: "c0".into(),
            name: long
        }));
        // Call 0 ends as soon as its JSON closes, before call 1 starts, so a sequential upstream
        // streams live.
        assert!(
            pos(&IrEvent::ToolCallEnd { block: 0 })
                < pos(&IrEvent::ToolCallStart {
                    block: 1,
                    id: "c1".into(),
                    name: "b".into()
                })
        );
        assert!(ev.contains(&IrEvent::ToolCallEnd { block: 1 }));
        assert_eq!(
            *ev.last().unwrap(),
            IrEvent::Finish(FinishReason::ToolCalls)
        );
    }

    #[test]
    fn interleaved_parallel_tool_calls_keep_their_blocks() {
        let wire = sse(&[
            delta(
                json!({"tool_calls": [{"index": 0, "id": "a", "function": {"name": "f", "arguments": "{\"p\":"}},
                                         {"index": 1, "id": "b", "function": {"name": "g", "arguments": "{\"q\":"}}]}),
            ),
            delta(json!({"tool_calls": [{"index": 1, "function": {"arguments": "2}"}}]})),
            delta(json!({"tool_calls": [{"index": 0, "function": {"arguments": "1}"}}]})),
            finish_chunk("tool_calls"),
        ]) + "data: [DONE]\n\n";
        let ev = run(&wire);
        let args = |b: u32| -> String {
            ev.iter()
                .filter_map(|e| match e {
                    IrEvent::ToolCallArgs { block, fragment } if *block == b => {
                        Some(fragment.as_str())
                    }
                    _ => None,
                })
                .collect()
        };
        assert_eq!(args(0), "{\"p\":1}");
        assert_eq!(args(1), "{\"q\":2}");
    }

    #[test]
    fn a_tool_call_whose_name_arrives_late_still_starts_correctly() {
        let wire = sse(&[
            delta(
                json!({"tool_calls": [{"index": 0, "id": "c", "function": {"arguments": "{\"a\":"}}]}),
            ),
            delta(
                json!({"tool_calls": [{"index": 0, "function": {"name": "late", "arguments": "1}"}}]}),
            ),
            finish_chunk("tool_calls"),
        ]) + "data: [DONE]\n\n";
        let ev = run(&wire);
        assert!(ev.contains(&IrEvent::ToolCallStart {
            block: 0,
            id: "c".into(),
            name: "late".into()
        }));
        let args: String = ev
            .iter()
            .filter_map(|e| {
                if let IrEvent::ToolCallArgs { fragment, .. } = e {
                    Some(fragment.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(args, "{\"a\":1}");
    }

    #[test]
    fn a_stream_without_done_but_with_a_finish_reason_still_completes() {
        let wire = sse(&[delta(json!({"content": "hi"})), finish_chunk("length")]);
        let ev = run(&wire);
        assert_eq!(*ev.last().unwrap(), IrEvent::Finish(FinishReason::Length));
    }

    #[test]
    fn done_without_a_finish_reason_finishes_as_stop() {
        let wire = sse(&[delta(json!({"content": "hi"}))]) + "data: [DONE]\n\n";
        assert_eq!(
            *run(&wire).last().unwrap(),
            IrEvent::Finish(FinishReason::Stop)
        );
    }

    #[test]
    fn a_truncated_stream_ends_in_an_error_never_a_finish() {
        let wire = sse(&[delta(json!({"content": "half an ans"}))]);
        let ev = run(&wire);
        assert_eq!(
            *ev.last().unwrap(),
            IrEvent::Error(StreamError::truncated())
        );
        assert!(!ev.iter().any(|e| matches!(e, IrEvent::Finish(_))));
        let empty = run("");
        assert_eq!(empty, vec![IrEvent::Error(StreamError::truncated())]);
    }

    #[test]
    fn a_half_written_tool_call_is_not_passed_off_as_finished() {
        let wire = sse(&[delta(
            json!({"tool_calls": [{"index": 0, "id": "c", "function": {"name": "n", "arguments": "{\"a\":"}}]}),
        )]);
        let ev = run(&wire);
        assert_eq!(
            *ev.last().unwrap(),
            IrEvent::Error(StreamError::truncated())
        );
        assert!(!ev
            .iter()
            .any(|e| matches!(e, IrEvent::ToolCallEnd { .. } | IrEvent::Finish(_))));
    }

    #[test]
    fn mid_stream_errors_and_garbage_end_the_stream() {
        let wire = sse(&[
            delta(json!({"content": "hi"})),
            json!({"error": {"message": "overloaded"}}),
        ]);
        let ev = run(&wire);
        assert_eq!(
            ev.last(),
            Some(&IrEvent::Error(StreamError {
                kind: StreamErrorKind::Upstream,
                message: "overloaded".into()
            }))
        );
        let ev = run("data: {not json\n\n");
        assert!(matches!(
            ev.last(),
            Some(IrEvent::Error(StreamError {
                kind: StreamErrorKind::Protocol,
                ..
            }))
        ));
        // Nothing is emitted after the terminal event.
        let mut p = ChatStream::new(ctx());
        let _ = p.push(b"data: {bad\n\n");
        assert!(p.push(b"data: [DONE]\n\n").is_empty());
        assert!(p.finish().is_empty());
    }

    #[test]
    fn refusals_stream_and_finish_as_refusal() {
        let wire =
            sse(&[delta(json!({"refusal": "No."})), finish_chunk("stop")]) + "data: [DONE]\n\n";
        let ev = run(&wire);
        assert!(ev.contains(&IrEvent::Refusal { text: "No.".into() }));
        assert_eq!(*ev.last().unwrap(), IrEvent::Finish(FinishReason::Refusal));
        let ev = run(&(sse(&[
            delta(json!({"content": ""})),
            finish_chunk("content_filter"),
        ]) + "data: [DONE]\n\n"));
        assert_eq!(*ev.last().unwrap(), IrEvent::Finish(FinishReason::Refusal));
    }

    #[test]
    fn oversized_events_fail_with_too_large() {
        let mut p = ChatStream::new(ctx());
        p.sse = SseParser::with_limits(16, 16);
        let ev = p.push(b"data: 0123456789abcdef0123456789\n\n");
        assert!(matches!(
            ev.last(),
            Some(IrEvent::Error(StreamError {
                kind: StreamErrorKind::TooLarge,
                ..
            }))
        ));
        let mut p = ChatStream::new(ctx());
        p.sse = SseParser::with_limits(16, 16);
        let ev = p.finish();
        assert!(matches!(ev.last(), Some(IrEvent::Error(_))));
    }

    #[test]
    fn disabled_think_tags_pass_through_as_text() {
        let mut c = ctx();
        c.think_tags = false;
        let mut p = ChatStream::new(c);
        let mut ev = p.push(
            (sse(&[
                delta(json!({"content": "<think>x</think>y"})),
                finish_chunk("stop"),
            ]) + "data: [DONE]\n\n")
                .as_bytes(),
        );
        ev.extend(p.finish());
        assert_eq!(text_of(&ev), "<think>x</think>y");
    }
}
