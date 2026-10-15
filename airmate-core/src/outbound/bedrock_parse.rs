//! The AWS Bedrock Converse outbound adapter: reading replies.
//!
//! A `Converse` reply is one JSON object. A `ConverseStream` reply is the AWS event stream
//! framing ([`crate::eventstream`]) around JSON events: `messageStart`, `contentBlockStart`,
//! `contentBlockDelta`, `contentBlockStop`, `messageStop` and a final `metadata` carrying usage,
//! or an exception message.

use std::collections::HashMap;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::Value as Json;

use super::{ParseCtx, UpstreamError};
use crate::eventstream::{Decoder, Frame};
use crate::ir::{FinishReason, IrCompletion, Part, ReasoningDisplay, Usage};
use crate::stream::{IrEvent, StreamError, StreamErrorKind, StreamParse};

/// Maps a Converse `stopReason`.
#[must_use]
pub fn finish_reason(reason: &str) -> FinishReason {
    match reason {
        "tool_use" => FinishReason::ToolCalls,
        "max_tokens" | "model_context_window_exceeded" => FinishReason::Length,
        "guardrail_intervened" | "content_filtered" => FinishReason::Refusal,
        _ => FinishReason::Stop,
    }
}

/// Maps Converse usage to the Anthropic shape. Converse reports uncached input separately from
/// cache reads and writes, as Anthropic does.
#[must_use]
pub fn usage(u: &Json) -> Usage {
    let n = |k: &str| u.get(k).and_then(Json::as_u64).unwrap_or(0);
    Usage {
        input_tokens: n("inputTokens"),
        output_tokens: n("outputTokens"),
        cache_read_input_tokens: n("cacheReadInputTokens"),
        cache_creation_input_tokens: n("cacheWriteInputTokens"),
        reasoning_tokens: 0,
    }
}

/// The message of an error body: `{"message": "..."}`.
fn exception_message(payload: &[u8]) -> String {
    serde_json::from_slice::<Json>(payload)
        .ok()
        .and_then(|j| {
            j.get("message")
                .or_else(|| j.get("Message"))
                .and_then(Json::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "the upstream reported an error".to_owned())
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
    let blocks = json
        .pointer("/output/message/content")
        .and_then(Json::as_array)
        .ok_or_else(|| UpstreamError::unreadable("the upstream reply has no message"))?;
    let mut content = Vec::new();
    let mut calls = 0usize;
    for b in blocks {
        if let Some(t) = b.get("text").and_then(Json::as_str) {
            if !t.is_empty() {
                content.push(Part::Text(t.to_owned()));
            }
        } else if let Some(tu) = b.get("toolUse") {
            calls += 1;
            let name = tu.get("name").and_then(Json::as_str).unwrap_or("");
            content.push(Part::ToolCall {
                id: tu
                    .get("toolUseId")
                    .and_then(Json::as_str)
                    .map_or_else(|| format!("call_{calls}"), str::to_owned),
                name: ctx.tool_names.restore(name).to_owned(),
                input: tu
                    .get("input")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
            });
        } else if let Some(rc) = b.get("reasoningContent") {
            if let Some(rt) = rc.get("reasoningText") {
                content.push(Part::Reasoning {
                    text: rt
                        .get("text")
                        .and_then(Json::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    display: ReasoningDisplay::Summarized,
                    origin: Some(ctx.origin.clone()),
                    opaque: rt
                        .get("signature")
                        .and_then(Json::as_str)
                        .map(|s| s.as_bytes().to_vec()),
                });
            } else if let Some(red) = rc.get("redactedContent").and_then(Json::as_str) {
                content.push(Part::Reasoning {
                    text: String::new(),
                    display: ReasoningDisplay::Omitted,
                    origin: Some(ctx.origin.clone()),
                    opaque: STANDARD.decode(red).ok(),
                });
            }
        }
    }
    let reason = json
        .get("stopReason")
        .and_then(Json::as_str)
        .map_or(FinishReason::Stop, finish_reason);
    Ok(IrCompletion {
        content,
        finish: if reason == FinishReason::Stop && calls > 0 {
            FinishReason::ToolCalls
        } else {
            reason
        },
        usage: json.get("usage").map(usage).unwrap_or_default(),
        upstream_id: None,
        upstream_model: None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Reasoning,
    Tool,
}

#[derive(Debug, Default)]
struct Open {
    kind: Option<Kind>,
    signature: Option<String>,
    redacted: Option<Vec<u8>>,
}

/// A parser for `ConverseStream`.
#[derive(Debug)]
pub struct BedrockStream {
    decoder: Decoder,
    ctx: ParseCtx,
    started: bool,
    blocks: HashMap<u32, Open>,
    saw_tool_calls: bool,
    finish: Option<FinishReason>,
    usage: Option<Usage>,
    terminal: bool,
}

impl BedrockStream {
    /// A parser for the reply to a request described by `ctx`.
    #[must_use]
    pub fn new(ctx: ParseCtx) -> Self {
        Self {
            decoder: Decoder::new(),
            ctx,
            started: false,
            blocks: HashMap::new(),
            saw_tool_calls: false,
            finish: None,
            usage: None,
            terminal: false,
        }
    }

    fn fail(&mut self, kind: StreamErrorKind, message: String, out: &mut Vec<IrEvent>) {
        out.push(IrEvent::Error(StreamError { kind, message }));
        self.terminal = true;
    }

    fn complete(&mut self, out: &mut Vec<IrEvent>) {
        // A block the service never closed is closed here so the sequencer sees it end.
        let mut open: Vec<u32> = self.blocks.keys().copied().collect();
        open.sort_unstable();
        for b in open {
            self.close_block(b, out);
        }
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

    fn close_block(&mut self, index: u32, out: &mut Vec<IrEvent>) {
        let Some(open) = self.blocks.remove(&index) else {
            return;
        };
        match open.kind {
            Some(Kind::Text) => out.push(IrEvent::TextEnd { block: index }),
            Some(Kind::Reasoning) => out.push(IrEvent::ReasoningSealed {
                block: index,
                opaque: open.signature.map(String::into_bytes).or(open.redacted),
            }),
            Some(Kind::Tool) => out.push(IrEvent::ToolCallEnd { block: index }),
            None => {}
        }
    }

    fn event(&mut self, kind: &str, j: &Json, out: &mut Vec<IrEvent>) {
        let index = j
            .get("contentBlockIndex")
            .and_then(Json::as_u64)
            .and_then(|i| u32::try_from(i).ok())
            .unwrap_or(0);
        match kind {
            "messageStart" => {
                if !self.started {
                    self.started = true;
                    out.push(IrEvent::Start {
                        id: None,
                        model: None,
                    });
                }
            }
            "contentBlockStart" => {
                if let Some(tu) = j.pointer("/start/toolUse") {
                    let name = self
                        .ctx
                        .tool_names
                        .restore(tu.get("name").and_then(Json::as_str).unwrap_or(""))
                        .to_owned();
                    let id = tu
                        .get("toolUseId")
                        .and_then(Json::as_str)
                        .map_or_else(|| format!("call_{index}"), str::to_owned);
                    self.saw_tool_calls = true;
                    self.blocks.insert(
                        index,
                        Open {
                            kind: Some(Kind::Tool),
                            ..Open::default()
                        },
                    );
                    out.push(IrEvent::ToolCallStart {
                        block: index,
                        id,
                        name,
                    });
                }
            }
            "contentBlockDelta" => {
                let d = j.get("delta").unwrap_or(&Json::Null);
                if let Some(t) = d.get("text").and_then(Json::as_str) {
                    self.blocks.entry(index).or_insert(Open {
                        kind: Some(Kind::Text),
                        ..Open::default()
                    });
                    out.push(IrEvent::TextDelta {
                        block: index,
                        text: t.to_owned(),
                    });
                } else if let Some(frag) = d.pointer("/toolUse/input").and_then(Json::as_str) {
                    out.push(IrEvent::ToolCallArgs {
                        block: index,
                        fragment: frag.to_owned(),
                    });
                } else if let Some(rc) = d.get("reasoningContent") {
                    let open = self.blocks.entry(index).or_insert(Open {
                        kind: Some(Kind::Reasoning),
                        ..Open::default()
                    });
                    if let Some(t) = rc.get("text").and_then(Json::as_str) {
                        out.push(IrEvent::ReasoningDelta {
                            block: index,
                            text: t.to_owned(),
                        });
                    }
                    if let Some(s) = rc.get("signature").and_then(Json::as_str) {
                        open.signature = Some(s.to_owned());
                    }
                    if let Some(r) = rc.get("redactedContent").and_then(Json::as_str) {
                        open.redacted = STANDARD.decode(r).ok();
                    }
                }
            }
            "contentBlockStop" => self.close_block(index, out),
            "messageStop" => {
                if let Some(r) = j.get("stopReason").and_then(Json::as_str) {
                    self.finish = Some(finish_reason(r));
                }
            }
            "metadata" => {
                if let Some(u) = j.get("usage") {
                    self.usage = Some(usage(u));
                }
                self.complete(out);
            }
            _ => {}
        }
    }

    fn frame(&mut self, f: &Frame, out: &mut Vec<IrEvent>) {
        if f.header(":message-type") == Some("exception") || f.header(":exception-type").is_some() {
            let kind = f.header(":exception-type").unwrap_or("exception");
            let message = format!("{}: {}", kind, exception_message(&f.payload));
            self.fail(StreamErrorKind::Upstream, message, out);
            return;
        }
        if f.header(":message-type") == Some("error") {
            let code = f.header(":error-code").unwrap_or("error");
            let message = f
                .header(":error-message")
                .unwrap_or("the upstream reported an error");
            self.fail(StreamErrorKind::Upstream, format!("{code}: {message}"), out);
            return;
        }
        let Some(kind) = f.header(":event-type") else {
            return;
        };
        match serde_json::from_slice::<Json>(&f.payload) {
            Ok(j) => self.event(kind, &j, out),
            Err(e) => self.fail(
                StreamErrorKind::Protocol,
                format!("the upstream sent an unreadable event: {e}"),
                out,
            ),
        }
    }
}

impl StreamParse for BedrockStream {
    fn push(&mut self, bytes: &[u8]) -> Vec<IrEvent> {
        let mut out = Vec::new();
        if self.terminal {
            return out;
        }
        match self.decoder.push(bytes) {
            Ok(frames) => {
                for f in &frames {
                    if self.terminal {
                        break;
                    }
                    self.frame(f, &mut out);
                }
            }
            Err(e) => self.fail(StreamErrorKind::Protocol, e.to_string(), &mut out),
        }
        out
    }

    fn finish(&mut self) -> Vec<IrEvent> {
        let mut out = Vec::new();
        if self.terminal {
            return out;
        }
        if self.finish.is_some() && !self.decoder.has_partial() {
            // The service ended the stream after `messageStop` without a `metadata` event.
            self.complete(&mut out);
        } else {
            out.push(IrEvent::Error(StreamError::truncated()));
            self.terminal = true;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eventstream::encode;
    use crate::ir::{Origin, Protocol};
    use crate::toolname::ToolNames;
    use serde_json::json;

    fn ctx() -> ParseCtx {
        ParseCtx {
            origin: Origin {
                upstream: "b".into(),
                protocol: Protocol::Bedrock,
            },
            tool_names: ToolNames::new(),
            think_tags: true,
        }
    }

    fn reply(blocks: Json, stop: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "output": {"message": {"role": "assistant", "content": blocks}},
            "stopReason": stop,
            "usage": {"inputTokens": 12, "outputTokens": 7, "totalTokens": 19, "cacheReadInputTokens": 3, "cacheWriteInputTokens": 2},
            "metrics": {"latencyMs": 100}
        }))
        .unwrap()
    }

    #[test]
    fn text_usage_and_stop_reason_are_read() {
        let c =
            parse_response(200, &reply(json!([{"text": "hello"}]), "end_turn"), &ctx()).unwrap();
        assert_eq!(c.content, vec![Part::Text("hello".into())]);
        assert_eq!(c.finish, FinishReason::Stop);
        assert_eq!(
            c.usage,
            Usage {
                input_tokens: 12,
                output_tokens: 7,
                cache_read_input_tokens: 3,
                cache_creation_input_tokens: 2,
                reasoning_tokens: 0
            }
        );
    }

    #[test]
    fn tool_use_and_reasoning_blocks_are_read() {
        let c = parse_response(
            200,
            &reply(
                json!([
                    {"reasoningContent": {"reasoningText": {"text": "hmm", "signature": "sig"}}},
                    {"reasoningContent": {"redactedContent": "AQID"}},
                    {"text": "calling"},
                    {"toolUse": {"toolUseId": "tu_1", "name": "get_weather", "input": {"city": "Paris"}}}
                ]),
                "tool_use",
            ),
            &ctx(),
        )
        .unwrap();
        assert_eq!(c.finish, FinishReason::ToolCalls);
        assert_eq!(
            c.content,
            vec![
                Part::Reasoning {
                    text: "hmm".into(),
                    display: ReasoningDisplay::Summarized,
                    origin: Some(ctx().origin),
                    opaque: Some(b"sig".to_vec())
                },
                Part::Reasoning {
                    text: String::new(),
                    display: ReasoningDisplay::Omitted,
                    origin: Some(ctx().origin),
                    opaque: Some(vec![1, 2, 3])
                },
                Part::Text("calling".into()),
                Part::ToolCall {
                    id: "tu_1".into(),
                    name: "get_weather".into(),
                    input: json!({"city": "Paris"})
                },
            ]
        );
    }

    #[test]
    fn stop_reasons_map() {
        for (r, f) in [
            ("end_turn", FinishReason::Stop),
            ("stop_sequence", FinishReason::Stop),
            ("tool_use", FinishReason::ToolCalls),
            ("max_tokens", FinishReason::Length),
            ("model_context_window_exceeded", FinishReason::Length),
            ("guardrail_intervened", FinishReason::Refusal),
            ("content_filtered", FinishReason::Refusal),
        ] {
            assert_eq!(finish_reason(r), f, "{r}");
        }
    }

    #[test]
    fn errors_come_from_the_status_and_a_malformed_reply_is_unreadable() {
        let e = parse_response(
            429,
            br#"{"message":"Too many requests, please wait before trying again."}"#,
            &ctx(),
        )
        .unwrap_err();
        assert_eq!(
            (e.status, e.message.as_str()),
            (429, "Too many requests, please wait before trying again.")
        );
        assert!(parse_response(200, b"nope", &ctx()).is_err());
        assert!(parse_response(200, br#"{"output":{}}"#, &ctx()).is_err());
    }

    fn ev(kind: &str, j: &Json) -> Vec<u8> {
        encode(
            &[
                (":message-type", "event"),
                (":event-type", kind),
                (":content-type", "application/json"),
            ],
            j.to_string().as_bytes(),
        )
    }

    fn run(chunks: &[Vec<u8>]) -> Vec<IrEvent> {
        let mut p = BedrockStream::new(ctx());
        let mut out = Vec::new();
        for c in chunks {
            out.extend(p.push(c));
        }
        out.extend(p.finish());
        out
    }

    fn full_stream() -> Vec<Vec<u8>> {
        vec![
            ev("messageStart", &json!({"role": "assistant"})),
            ev(
                "contentBlockDelta",
                &json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"text": "thinking"}}}),
            ),
            ev(
                "contentBlockDelta",
                &json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"signature": "sigabc"}}}),
            ),
            ev("contentBlockStop", &json!({"contentBlockIndex": 0})),
            ev(
                "contentBlockDelta",
                &json!({"contentBlockIndex": 1, "delta": {"text": "Hel"}}),
            ),
            ev(
                "contentBlockDelta",
                &json!({"contentBlockIndex": 1, "delta": {"text": "lo"}}),
            ),
            ev("contentBlockStop", &json!({"contentBlockIndex": 1})),
            ev(
                "contentBlockStart",
                &json!({"contentBlockIndex": 2, "start": {"toolUse": {"toolUseId": "tu_1", "name": "t"}}}),
            ),
            ev(
                "contentBlockDelta",
                &json!({"contentBlockIndex": 2, "delta": {"toolUse": {"input": "{\"a\":"}}}),
            ),
            ev(
                "contentBlockDelta",
                &json!({"contentBlockIndex": 2, "delta": {"toolUse": {"input": "1}"}}}),
            ),
            ev("contentBlockStop", &json!({"contentBlockIndex": 2})),
            ev("messageStop", &json!({"stopReason": "tool_use"})),
            ev(
                "metadata",
                &json!({"usage": {"inputTokens": 4, "outputTokens": 9}, "metrics": {"latencyMs": 5}}),
            ),
        ]
    }

    #[test]
    fn a_stream_becomes_ir_events_in_order() {
        let events = run(&full_stream());
        assert_eq!(
            events,
            vec![
                IrEvent::Start {
                    id: None,
                    model: None
                },
                IrEvent::ReasoningDelta {
                    block: 0,
                    text: "thinking".into()
                },
                IrEvent::ReasoningSealed {
                    block: 0,
                    opaque: Some(b"sigabc".to_vec())
                },
                IrEvent::TextDelta {
                    block: 1,
                    text: "Hel".into()
                },
                IrEvent::TextDelta {
                    block: 1,
                    text: "lo".into()
                },
                IrEvent::TextEnd { block: 1 },
                IrEvent::ToolCallStart {
                    block: 2,
                    id: "tu_1".into(),
                    name: "t".into()
                },
                IrEvent::ToolCallArgs {
                    block: 2,
                    fragment: "{\"a\":".into()
                },
                IrEvent::ToolCallArgs {
                    block: 2,
                    fragment: "1}".into()
                },
                IrEvent::ToolCallEnd { block: 2 },
                IrEvent::Usage(Usage {
                    input_tokens: 4,
                    output_tokens: 9,
                    ..Usage::default()
                }),
                IrEvent::Finish(FinishReason::ToolCalls),
            ]
        );
    }

    #[test]
    fn any_chunking_of_the_bytes_gives_the_same_events() {
        let bytes: Vec<u8> = full_stream().concat();
        let expected = run(std::slice::from_ref(&bytes));
        for step in [1, 2, 3, 5, 7, 13, 64, 1000] {
            let pieces: Vec<Vec<u8>> = bytes.chunks(step).map(<[u8]>::to_vec).collect();
            assert_eq!(run(&pieces), expected, "step {step}");
        }
    }

    #[test]
    fn a_stream_cut_before_message_stop_is_truncated() {
        let mut s = full_stream();
        s.truncate(6);
        let events = run(&s);
        assert!(
            matches!(
                events.last(),
                Some(IrEvent::Error(StreamError {
                    kind: StreamErrorKind::Truncated,
                    ..
                }))
            ),
            "{events:?}"
        );
        assert!(!events.iter().any(|e| matches!(e, IrEvent::Finish(_))));
    }

    #[test]
    fn a_stream_that_ends_after_message_stop_without_metadata_still_finishes() {
        let mut s = full_stream();
        s.pop();
        let events = run(&s);
        assert_eq!(
            events.last(),
            Some(&IrEvent::Finish(FinishReason::ToolCalls))
        );
    }

    #[test]
    fn an_exception_frame_ends_the_stream_with_its_message() {
        let frame = encode(
            &[
                (":message-type", "exception"),
                (":exception-type", "throttlingException"),
                (":content-type", "application/json"),
            ],
            br#"{"message":"Too many requests"}"#,
        );
        let events = run(&[ev("messageStart", &json!({})), frame]);
        let last = events.last().unwrap();
        assert!(
            matches!(last, IrEvent::Error(StreamError { kind: StreamErrorKind::Upstream, message }) if message == "throttlingException: Too many requests"),
            "{events:?}"
        );
    }

    #[test]
    fn a_corrupt_frame_is_a_protocol_error_not_garbage() {
        let mut bad = ev("messageStart", &json!({}));
        let n = bad.len();
        bad[n - 1] ^= 0xFF;
        let events = run(&[bad]);
        assert!(
            matches!(
                &events[..],
                [IrEvent::Error(StreamError {
                    kind: StreamErrorKind::Protocol,
                    ..
                })]
            ),
            "{events:?}"
        );
    }

    #[test]
    fn a_block_the_service_never_closed_is_closed_before_the_finish() {
        let events = run(&[
            ev("messageStart", &json!({})),
            ev(
                "contentBlockDelta",
                &json!({"contentBlockIndex": 0, "delta": {"text": "x"}}),
            ),
            ev("messageStop", &json!({"stopReason": "end_turn"})),
            ev(
                "metadata",
                &json!({"usage": {"inputTokens": 1, "outputTokens": 1}}),
            ),
        ]);
        let pos = |e: &IrEvent| events.iter().position(|x| x == e).unwrap();
        assert!(pos(&IrEvent::TextEnd { block: 0 }) < pos(&IrEvent::Finish(FinishReason::Stop)));
    }
}
