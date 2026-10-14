//! The OpenAI Responses outbound adapter: reading replies.

use std::collections::BTreeMap;

use serde_json::Value as Json;

use super::{ParseCtx, UpstreamError};
use crate::ir::{FinishReason, IrCompletion, Part, ReasoningDisplay, Usage};
use crate::sse::{SseEvent, SseParser};
use crate::stream::{IrEvent, StreamError, StreamErrorKind, StreamParse};

/// Maps Responses usage to the Anthropic shape.
#[must_use]
pub fn usage(u: &Json) -> Usage {
    let n = |v: Option<&Json>| v.and_then(Json::as_u64).unwrap_or(0);
    let input = n(u.get("input_tokens"));
    let cached = n(u.pointer("/input_tokens_details/cached_tokens"));
    Usage {
        input_tokens: input.saturating_sub(cached),
        output_tokens: n(u.get("output_tokens")),
        cache_read_input_tokens: cached,
        cache_creation_input_tokens: 0,
        reasoning_tokens: n(u.pointer("/output_tokens_details/reasoning_tokens")),
    }
}

fn summary_text(item: &Json) -> String {
    item.get("summary")
        .and_then(Json::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Json::as_str))
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default()
}

fn opaque(item: &Json) -> Option<Vec<u8>> {
    item.get("encrypted_content")
        .and_then(Json::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| s.as_bytes().to_vec())
}

fn incomplete_finish(response: &Json) -> FinishReason {
    match response
        .pointer("/incomplete_details/reason")
        .and_then(Json::as_str)
    {
        Some("content_filter") => FinishReason::Refusal,
        _ => FinishReason::Length,
    }
}

fn error_message(response: &Json) -> String {
    response
        .pointer("/error/message")
        .and_then(Json::as_str)
        .unwrap_or("the upstream reported a failure")
        .to_owned()
}

/// Reads a non-streaming Responses reply.
///
/// # Errors
///
/// An [`UpstreamError`] for a non-2xx status, a failed response or an unreadable reply.
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
    if json.get("status").and_then(Json::as_str) == Some("failed")
        || json.get("error").is_some_and(|e| !e.is_null())
    {
        return Err(UpstreamError {
            status: 502,
            message: error_message(&json),
            code: json
                .pointer("/error/code")
                .and_then(Json::as_str)
                .map(str::to_owned),
        });
    }
    let output = json
        .get("output")
        .and_then(Json::as_array)
        .ok_or_else(|| UpstreamError::unreadable("the upstream reply has no output"))?;

    let mut content = Vec::new();
    let mut has_calls = false;
    let mut refused = false;
    for item in output {
        match item.get("type").and_then(Json::as_str) {
            Some("reasoning") => {
                let text = summary_text(item);
                let opaque = opaque(item);
                if !text.is_empty() || opaque.is_some() {
                    content.push(Part::Reasoning {
                        text,
                        display: ReasoningDisplay::Summarized,
                        origin: Some(ctx.origin.clone()),
                        opaque,
                    });
                }
            }
            Some("message") => {
                let mut text = String::new();
                for c in item
                    .get("content")
                    .and_then(Json::as_array)
                    .into_iter()
                    .flatten()
                {
                    match c.get("type").and_then(Json::as_str) {
                        Some("output_text") => {
                            text.push_str(c.get("text").and_then(Json::as_str).unwrap_or(""))
                        }
                        Some("refusal") => {
                            refused = true;
                            text.push_str(c.get("refusal").and_then(Json::as_str).unwrap_or(""));
                        }
                        _ => {}
                    }
                }
                if !text.is_empty() {
                    content.push(Part::Text(text));
                }
            }
            Some("function_call") => {
                let name = item.get("name").and_then(Json::as_str).unwrap_or("");
                let args = item.get("arguments").and_then(Json::as_str).unwrap_or("");
                let input = if args.trim().is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::from_str(args).map_err(|e| {
                        UpstreamError::unreadable(format!(
                            "tool call `{name}` has invalid JSON arguments: {e}"
                        ))
                    })?
                };
                let id = item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(Json::as_str)
                    .unwrap_or("call_0")
                    .to_owned();
                content.push(Part::ToolCall {
                    id,
                    name: ctx.tool_names.restore(name).to_owned(),
                    input,
                });
                has_calls = true;
            }
            _ => {}
        }
    }

    let finish = if refused {
        FinishReason::Refusal
    } else if json.get("status").and_then(Json::as_str) == Some("incomplete") {
        incomplete_finish(&json)
    } else if has_calls {
        FinishReason::ToolCalls
    } else {
        FinishReason::Stop
    };
    Ok(IrCompletion {
        content,
        finish,
        usage: json
            .get("usage")
            .filter(|u| u.is_object())
            .map(usage)
            .unwrap_or_default(),
        upstream_id: json.get("id").and_then(Json::as_str).map(str::to_owned),
        upstream_model: json.get("model").and_then(Json::as_str).map(str::to_owned),
    })
}

// ---- streaming ------------------------------------------------------------------------------

#[derive(Debug)]
enum Item {
    Reasoning {
        block: u32,
        last_summary: Option<u64>,
        any: bool,
    },
    Message {
        block: u32,
        any: bool,
    },
    Call {
        block: u32,
        args_sent: bool,
    },
}

/// A parser for Responses server-sent events.
///
/// Each output item is a segment keyed by `output_index`; its `done` event closes it and, for
/// reasoning, supplies the encrypted content the signature carries.
#[derive(Debug)]
pub struct ResponsesStream {
    sse: SseParser,
    ctx: ParseCtx,
    started: bool,
    next_block: u32,
    items: BTreeMap<u64, Item>,
    saw_calls: bool,
    refused: bool,
    terminal: bool,
}

impl ResponsesStream {
    /// A parser for the reply to a request described by `ctx`.
    #[must_use]
    pub fn new(ctx: ParseCtx) -> Self {
        Self {
            sse: SseParser::new(),
            ctx,
            started: false,
            next_block: 0,
            items: BTreeMap::new(),
            saw_calls: false,
            refused: false,
            terminal: false,
        }
    }

    fn new_block(&mut self) -> u32 {
        let b = self.next_block;
        self.next_block += 1;
        b
    }

    fn fail(&mut self, kind: StreamErrorKind, message: String, out: &mut Vec<IrEvent>) {
        out.push(IrEvent::Error(StreamError { kind, message }));
        self.terminal = true;
    }

    fn start(&mut self, response: &Json, out: &mut Vec<IrEvent>) {
        if !self.started {
            self.started = true;
            out.push(IrEvent::Start {
                id: response.get("id").and_then(Json::as_str).map(str::to_owned),
                model: response
                    .get("model")
                    .and_then(Json::as_str)
                    .map(str::to_owned),
            });
        }
    }

    /// Opens a block for `index` if its `added` event was missed (some servers skip it).
    fn ensure_item(&mut self, index: u64, kind: &str, out: &mut Vec<IrEvent>) {
        if self.items.contains_key(&index) {
            return;
        }
        let block = self.new_block();
        let item = match kind {
            "reasoning" => Item::Reasoning {
                block,
                last_summary: None,
                any: false,
            },
            _ => Item::Message { block, any: false },
        };
        self.items.insert(index, item);
        let _ = out;
    }

    fn event(&mut self, ty: &str, j: &Json, out: &mut Vec<IrEvent>) {
        let index = j.get("output_index").and_then(Json::as_u64).unwrap_or(0);
        match ty {
            "response.created" | "response.in_progress" | "response.queued" => {
                if let Some(r) = j.get("response") {
                    self.start(r, out);
                }
            }
            "response.output_item.added" => {
                self.start(&Json::Null, out);
                let item = j.get("item").unwrap_or(&Json::Null);
                let block = self.new_block();
                match item.get("type").and_then(Json::as_str) {
                    Some("reasoning") => {
                        self.items.insert(
                            index,
                            Item::Reasoning {
                                block,
                                last_summary: None,
                                any: false,
                            },
                        );
                    }
                    Some("function_call") => {
                        let name = item.get("name").and_then(Json::as_str).unwrap_or("");
                        let id = item
                            .get("call_id")
                            .or_else(|| item.get("id"))
                            .and_then(Json::as_str)
                            .map_or_else(|| format!("call_{block}"), str::to_owned);
                        self.items.insert(
                            index,
                            Item::Call {
                                block,
                                args_sent: false,
                            },
                        );
                        self.saw_calls = true;
                        out.push(IrEvent::ToolCallStart {
                            block,
                            id,
                            name: self.ctx.tool_names.restore(name).to_owned(),
                        });
                    }
                    _ => {
                        self.items
                            .insert(index, Item::Message { block, any: false });
                    }
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                self.ensure_item(index, "reasoning", out);
                let summary = j.get("summary_index").and_then(Json::as_u64);
                let delta = j.get("delta").and_then(Json::as_str).unwrap_or("");
                if let Some(Item::Reasoning {
                    block,
                    last_summary,
                    any,
                }) = self.items.get_mut(&index)
                {
                    let mut text = String::new();
                    if *any && summary.is_some() && *last_summary != summary {
                        text.push_str("\n\n");
                    }
                    text.push_str(delta);
                    *last_summary = summary;
                    *any = true;
                    if !text.is_empty() {
                        out.push(IrEvent::ReasoningDelta {
                            block: *block,
                            text,
                        });
                    }
                }
            }
            "response.output_text.delta" => {
                self.ensure_item(index, "message", out);
                let delta = j.get("delta").and_then(Json::as_str).unwrap_or("");
                if let Some(Item::Message { block, any }) = self.items.get_mut(&index) {
                    if !delta.is_empty() {
                        *any = true;
                        out.push(IrEvent::TextDelta {
                            block: *block,
                            text: delta.to_owned(),
                        });
                    }
                }
            }
            "response.refusal.delta" => {
                self.refused = true;
                let delta = j.get("delta").and_then(Json::as_str).unwrap_or("");
                if !delta.is_empty() {
                    out.push(IrEvent::Refusal {
                        text: delta.to_owned(),
                    });
                }
            }
            "response.function_call_arguments.delta" => {
                let delta = j.get("delta").and_then(Json::as_str).unwrap_or("");
                if let Some(Item::Call { block, args_sent }) = self.items.get_mut(&index) {
                    if !delta.is_empty() {
                        *args_sent = true;
                        out.push(IrEvent::ToolCallArgs {
                            block: *block,
                            fragment: delta.to_owned(),
                        });
                    }
                }
            }
            "response.output_item.done" => {
                let item = j.get("item").unwrap_or(&Json::Null);
                match self.items.remove(&index) {
                    Some(Item::Reasoning { block, any, .. }) => {
                        if !any {
                            let text = summary_text(item);
                            if !text.is_empty() {
                                out.push(IrEvent::ReasoningDelta { block, text });
                            }
                        }
                        out.push(IrEvent::ReasoningSealed {
                            block,
                            opaque: opaque(item),
                        });
                    }
                    Some(Item::Message { block, any }) => {
                        if !any {
                            let text: String = item
                                .get("content")
                                .and_then(Json::as_array)
                                .into_iter()
                                .flatten()
                                .filter_map(|c| c.get("text").and_then(Json::as_str))
                                .collect();
                            if !text.is_empty() {
                                out.push(IrEvent::TextDelta { block, text });
                            }
                        }
                        out.push(IrEvent::TextEnd { block });
                    }
                    Some(Item::Call { block, args_sent }) => {
                        if !args_sent {
                            if let Some(a) = item
                                .get("arguments")
                                .and_then(Json::as_str)
                                .filter(|a| !a.is_empty())
                            {
                                out.push(IrEvent::ToolCallArgs {
                                    block,
                                    fragment: a.to_owned(),
                                });
                            }
                        }
                        out.push(IrEvent::ToolCallEnd { block });
                    }
                    None => {}
                }
            }
            "response.completed" | "response.incomplete" => {
                let r = j.get("response").unwrap_or(&Json::Null);
                self.start(r, out);
                if let Some(u) = r.get("usage").filter(|u| u.is_object()) {
                    out.push(IrEvent::Usage(usage(u)));
                }
                let reason = if self.refused {
                    FinishReason::Refusal
                } else if ty == "response.incomplete" {
                    incomplete_finish(r)
                } else if self.saw_calls {
                    FinishReason::ToolCalls
                } else {
                    FinishReason::Stop
                };
                out.push(IrEvent::Finish(reason));
                self.terminal = true;
            }
            "response.failed" => {
                let r = j.get("response").unwrap_or(&Json::Null);
                self.fail(StreamErrorKind::Upstream, error_message(r), out);
            }
            "error" => {
                let message = j
                    .get("message")
                    .or_else(|| j.pointer("/error/message"))
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
            let data = ev.data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            match serde_json::from_str::<Json>(data) {
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

impl StreamParse for ResponsesStream {
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
                upstream: "oa".into(),
                protocol: Protocol::Responses,
            },
            tool_names: ToolNames::new(),
            think_tags: true,
        }
    }

    fn body(v: Json) -> Vec<u8> {
        serde_json::to_vec(&v).unwrap()
    }

    fn response(output: Json, status: &str) -> Vec<u8> {
        body(
            json!({"id": "resp_1", "model": "gpt-x", "status": status, "output": output,
            "usage": {"input_tokens": 100, "output_tokens": 30, "input_tokens_details": {"cached_tokens": 40}, "output_tokens_details": {"reasoning_tokens": 12}}}),
        )
    }

    #[test]
    fn parses_text_reasoning_calls_and_usage() {
        let out = json!([
            {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "first"}, {"type": "summary_text", "text": "second"}], "encrypted_content": "ENC"},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "hello "}, {"type": "output_text", "text": "world"}]},
            {"type": "function_call", "call_id": "c1", "name": "read", "arguments": "{\"p\":1}"}
        ]);
        let c = parse_response(200, &response(out, "completed"), &ctx()).unwrap();
        assert!(
            matches!(&c.content[0], Part::Reasoning { text, opaque: Some(o), origin: Some(og), .. } if text == "first\n\nsecond" && o == b"ENC" && og.upstream == "oa")
        );
        assert_eq!(c.content[1], Part::Text("hello world".into()));
        assert_eq!(
            c.content[2],
            Part::ToolCall {
                id: "c1".into(),
                name: "read".into(),
                input: json!({"p": 1})
            }
        );
        assert_eq!(c.finish, FinishReason::ToolCalls);
        assert_eq!(
            c.usage,
            Usage {
                input_tokens: 60,
                output_tokens: 30,
                cache_read_input_tokens: 40,
                cache_creation_input_tokens: 0,
                reasoning_tokens: 12
            }
        );
        assert_eq!(c.upstream_id.as_deref(), Some("resp_1"));
        assert_eq!(c.upstream_model.as_deref(), Some("gpt-x"));
    }

    #[test]
    fn empty_reasoning_items_are_dropped_but_encrypted_only_ones_are_kept() {
        let out = json!([
            {"type": "reasoning", "summary": []},
            {"type": "reasoning", "summary": [], "encrypted_content": "ENC"},
            {"type": "message", "content": [{"type": "output_text", "text": "x"}]}
        ]);
        let c = parse_response(200, &response(out, "completed"), &ctx()).unwrap();
        assert_eq!(c.content.len(), 2);
        assert!(matches!(&c.content[0], Part::Reasoning { text, .. } if text.is_empty()));
        assert_eq!(c.finish, FinishReason::Stop);
    }

    #[test]
    fn incomplete_and_refusing_responses_finish_accordingly() {
        let mut v: Json = serde_json::from_slice(&response(
            json!([{"type": "message", "content": [{"type": "output_text", "text": "cut"}]}]),
            "incomplete",
        ))
        .unwrap();
        v["incomplete_details"] = json!({"reason": "max_output_tokens"});
        assert_eq!(
            parse_response(200, &body(v.clone()), &ctx())
                .unwrap()
                .finish,
            FinishReason::Length
        );
        v["incomplete_details"] = json!({"reason": "content_filter"});
        assert_eq!(
            parse_response(200, &body(v), &ctx()).unwrap().finish,
            FinishReason::Refusal
        );
        let c = parse_response(
            200,
            &response(
                json!([{"type": "message", "content": [{"type": "refusal", "refusal": "No"}]}]),
                "completed",
            ),
            &ctx(),
        )
        .unwrap();
        assert_eq!(c.finish, FinishReason::Refusal);
        assert_eq!(c.content, vec![Part::Text("No".into())]);
    }

    #[test]
    fn failures_and_garbage_are_errors() {
        let failed = body(
            json!({"status": "failed", "error": {"message": "model exploded", "code": "server_error"}}),
        );
        let e = parse_response(200, &failed, &ctx()).unwrap_err();
        assert_eq!(
            (e.status, e.message.as_str(), e.code.as_deref()),
            (502, "model exploded", Some("server_error"))
        );
        assert_eq!(
            parse_response(429, br#"{"error":{"message":"slow"}}"#, &ctx())
                .unwrap_err()
                .status,
            429
        );
        assert!(parse_response(200, b"nope", &ctx()).is_err());
        assert!(parse_response(200, b"{}", &ctx())
            .unwrap_err()
            .message
            .contains("no output"));
        let bad_args = response(
            json!([{"type": "function_call", "call_id": "c", "name": "n", "arguments": "{x"}]),
            "completed",
        );
        assert!(parse_response(200, &bad_args, &ctx())
            .unwrap_err()
            .message
            .contains("invalid JSON"));
    }

    #[test]
    fn tool_names_are_restored_and_empty_arguments_become_an_empty_object() {
        let mut names = ToolNames::new();
        let long = "q".repeat(90);
        let short = names.shorten(&long);
        let mut c = ctx();
        c.tool_names = names;
        let out =
            json!([{"type": "function_call", "call_id": "c", "name": short, "arguments": ""}]);
        let r = parse_response(200, &response(out, "completed"), &c).unwrap();
        assert_eq!(
            r.content[0],
            Part::ToolCall {
                id: "c".into(),
                name: long,
                input: json!({})
            }
        );
    }

    // ---- streaming ----

    fn ev(ty: &str, data: Json) -> String {
        let mut d = data;
        d["type"] = json!(ty);
        format!("event: {ty}\ndata: {d}\n\n")
    }

    fn run_split(wire: &str, size: usize) -> Vec<IrEvent> {
        let mut p = ResponsesStream::new(ctx());
        let mut out = Vec::new();
        for c in wire.as_bytes().chunks(size.max(1)) {
            out.extend(p.push(c));
        }
        out.extend(p.finish());
        out
    }

    fn run(wire: &str) -> Vec<IrEvent> {
        run_split(wire, wire.len())
    }

    fn reasoning_then_tool_stream() -> String {
        [
            ev("response.created", json!({"response": {"id": "resp_9", "model": "gpt-x"}})),
            ev("response.output_item.added", json!({"output_index": 0, "item": {"type": "reasoning", "id": "rs"}})),
            ev("response.reasoning_summary_part.added", json!({"output_index": 0, "summary_index": 0})),
            ev("response.reasoning_summary_text.delta", json!({"output_index": 0, "summary_index": 0, "delta": "Think"})),
            ev("response.reasoning_summary_text.delta", json!({"output_index": 0, "summary_index": 0, "delta": "ing"})),
            ev("response.reasoning_summary_text.delta", json!({"output_index": 0, "summary_index": 1, "delta": "More"})),
            ev("response.output_item.done", json!({"output_index": 0, "item": {"type": "reasoning", "encrypted_content": "ENC"}})),
            ev("response.output_item.added", json!({"output_index": 1, "item": {"type": "message", "role": "assistant"}})),
            ev("response.content_part.added", json!({"output_index": 1, "content_index": 0, "part": {"type": "output_text"}})),
            ev("response.output_text.delta", json!({"output_index": 1, "content_index": 0, "delta": "Let me "})),
            ev("response.output_text.delta", json!({"output_index": 1, "content_index": 0, "delta": "look."})),
            ev("response.output_item.done", json!({"output_index": 1, "item": {"type": "message"}})),
            ev("response.output_item.added", json!({"output_index": 2, "item": {"type": "function_call", "call_id": "call_a", "name": "read"}})),
            ev("response.function_call_arguments.delta", json!({"output_index": 2, "delta": "{\"p\":"})),
            ev("response.function_call_arguments.delta", json!({"output_index": 2, "delta": "1}"})),
            ev("response.output_item.done", json!({"output_index": 2, "item": {"type": "function_call", "arguments": "{\"p\":1}"}})),
            ev("response.completed", json!({"response": {"id": "resp_9", "usage": {"input_tokens": 10, "output_tokens": 5, "input_tokens_details": {"cached_tokens": 2}}}})),
        ]
        .concat()
    }

    #[test]
    fn streams_reasoning_text_and_a_tool_call_with_encrypted_content() {
        let events = run(&reasoning_then_tool_stream());
        assert!(
            matches!(&events[0], IrEvent::Start { id: Some(i), model: Some(m) } if i == "resp_9" && m == "gpt-x")
        );
        let reasoning: String = events
            .iter()
            .filter_map(|e| {
                if let IrEvent::ReasoningDelta { text, .. } = e {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(reasoning, "Thinking\n\nMore");
        assert!(events.contains(&IrEvent::ReasoningSealed {
            block: 0,
            opaque: Some(b"ENC".to_vec())
        }));
        let text: String = events
            .iter()
            .filter_map(|e| {
                if let IrEvent::TextDelta { text, .. } = e {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(text, "Let me look.");
        assert!(events.contains(&IrEvent::TextEnd { block: 1 }));
        assert!(events.contains(&IrEvent::ToolCallStart {
            block: 2,
            id: "call_a".into(),
            name: "read".into()
        }));
        assert!(events.contains(&IrEvent::ToolCallEnd { block: 2 }));
        let n = events.len();
        assert!(matches!(
            events[n - 2],
            IrEvent::Usage(Usage {
                input_tokens: 8,
                output_tokens: 5,
                cache_read_input_tokens: 2,
                ..
            })
        ));
        assert_eq!(events[n - 1], IrEvent::Finish(FinishReason::ToolCalls));
    }

    #[test]
    fn every_chunk_size_gives_the_same_events() {
        let wire = reasoning_then_tool_stream();
        let whole = run(&wire);
        for size in [1, 2, 3, 7, 13, 101] {
            assert_eq!(run_split(&wire, size), whole, "chunk size {size}");
        }
    }

    #[test]
    fn done_events_fill_in_what_deltas_did_not_carry() {
        let wire = [
            ev("response.output_item.added", json!({"output_index": 0, "item": {"type": "reasoning"}})),
            ev("response.output_item.done", json!({"output_index": 0, "item": {"type": "reasoning", "summary": [{"type": "summary_text", "text": "whole"}]}})),
            ev("response.output_item.added", json!({"output_index": 1, "item": {"type": "message"}})),
            ev("response.output_item.done", json!({"output_index": 1, "item": {"type": "message", "content": [{"type": "output_text", "text": "all at once"}]}})),
            ev("response.output_item.added", json!({"output_index": 2, "item": {"type": "function_call", "call_id": "c", "name": "n"}})),
            ev("response.output_item.done", json!({"output_index": 2, "item": {"type": "function_call", "arguments": "{}"}})),
            ev("response.completed", json!({"response": {}})),
        ]
        .concat();
        let events = run(&wire);
        assert!(events.contains(&IrEvent::ReasoningDelta {
            block: 0,
            text: "whole".into()
        }));
        assert!(events.contains(&IrEvent::TextDelta {
            block: 1,
            text: "all at once".into()
        }));
        assert!(events.contains(&IrEvent::ToolCallArgs {
            block: 2,
            fragment: "{}".into()
        }));
        assert_eq!(
            *events.last().unwrap(),
            IrEvent::Finish(FinishReason::ToolCalls)
        );
    }

    #[test]
    fn a_text_delta_without_an_added_event_still_opens_a_block() {
        let wire = [
            ev(
                "response.output_text.delta",
                json!({"output_index": 0, "delta": "hi"}),
            ),
            ev(
                "response.output_item.done",
                json!({"output_index": 0, "item": {}}),
            ),
            ev("response.completed", json!({"response": {}})),
        ]
        .concat();
        let events = run(&wire);
        assert!(events.contains(&IrEvent::TextDelta {
            block: 0,
            text: "hi".into()
        }));
        assert!(events.contains(&IrEvent::TextEnd { block: 0 }));
    }

    #[test]
    fn incomplete_refusal_and_failure_terminals() {
        let inc = ev(
            "response.incomplete",
            json!({"response": {"incomplete_details": {"reason": "max_output_tokens"}}}),
        );
        assert_eq!(
            *run(&inc).last().unwrap(),
            IrEvent::Finish(FinishReason::Length)
        );
        let filt = ev(
            "response.incomplete",
            json!({"response": {"incomplete_details": {"reason": "content_filter"}}}),
        );
        assert_eq!(
            *run(&filt).last().unwrap(),
            IrEvent::Finish(FinishReason::Refusal)
        );
        let refusal = [
            ev("response.refusal.delta", json!({"delta": "No."})),
            ev("response.completed", json!({"response": {}})),
        ]
        .concat();
        let events = run(&refusal);
        assert!(events.contains(&IrEvent::Refusal { text: "No.".into() }));
        assert_eq!(
            *events.last().unwrap(),
            IrEvent::Finish(FinishReason::Refusal)
        );
        let failed = ev(
            "response.failed",
            json!({"response": {"error": {"message": "boom"}}}),
        );
        assert_eq!(
            *run(&failed).last().unwrap(),
            IrEvent::Error(StreamError {
                kind: StreamErrorKind::Upstream,
                message: "boom".into()
            })
        );
        let err = ev("error", json!({"message": "bad"}));
        assert_eq!(
            *run(&err).last().unwrap(),
            IrEvent::Error(StreamError {
                kind: StreamErrorKind::Upstream,
                message: "bad".into()
            })
        );
    }

    #[test]
    fn a_stream_with_no_terminal_event_is_truncated() {
        let wire = [
            ev(
                "response.output_item.added",
                json!({"output_index": 0, "item": {"type": "message"}}),
            ),
            ev(
                "response.output_text.delta",
                json!({"output_index": 0, "delta": "half"}),
            ),
        ]
        .concat();
        let events = run(&wire);
        assert_eq!(
            *events.last().unwrap(),
            IrEvent::Error(StreamError::truncated())
        );
        assert!(!events.iter().any(|e| matches!(e, IrEvent::Finish(_))));
        assert_eq!(run(""), vec![IrEvent::Error(StreamError::truncated())]);
    }

    #[test]
    fn garbage_and_oversized_events_end_the_stream_and_nothing_follows() {
        let events = run("data: {oops\n\n");
        assert!(matches!(
            events.last(),
            Some(IrEvent::Error(StreamError {
                kind: StreamErrorKind::Protocol,
                ..
            }))
        ));
        let mut p = ResponsesStream::new(ctx());
        let _ = p.push(b"data: {oops\n\n");
        assert!(p
            .push(ev("response.completed", json!({"response": {}})).as_bytes())
            .is_empty());
        assert!(p.finish().is_empty());
        let mut p = ResponsesStream::new(ctx());
        p.sse = SseParser::with_limits(8, 8);
        assert!(matches!(
            p.push(b"data: 0123456789abcdef\n\n").last(),
            Some(IrEvent::Error(StreamError {
                kind: StreamErrorKind::TooLarge,
                ..
            }))
        ));
        let mut p = ResponsesStream::new(ctx());
        p.sse = SseParser::with_limits(8, 8);
        assert!(matches!(p.finish().last(), Some(IrEvent::Error(_))));
    }

    #[test]
    fn the_event_name_is_used_when_the_payload_has_no_type() {
        let wire = "event: response.output_text.delta\ndata: {\"output_index\":0,\"delta\":\"x\"}\n\nevent: response.completed\ndata: {\"response\":{}}\n\n";
        let events = run(wire);
        assert!(events.contains(&IrEvent::TextDelta {
            block: 0,
            text: "x".into()
        }));
        assert_eq!(*events.last().unwrap(), IrEvent::Finish(FinishReason::Stop));
    }

    #[test]
    fn restores_shortened_tool_names_on_the_stream() {
        let mut names = ToolNames::new();
        let long = "w".repeat(90);
        let short = names.shorten(&long);
        let mut c = ctx();
        c.tool_names = names;
        let wire = [
            ev("response.output_item.added", json!({"output_index": 0, "item": {"type": "function_call", "call_id": "c", "name": short}})),
            ev("response.output_item.done", json!({"output_index": 0, "item": {"arguments": "{}"}})),
            ev("response.completed", json!({"response": {}})),
        ]
        .concat();
        let mut p = ResponsesStream::new(c);
        let events = p.push(wire.as_bytes());
        assert!(events.contains(&IrEvent::ToolCallStart {
            block: 0,
            id: "c".into(),
            name: long
        }));
    }
}
