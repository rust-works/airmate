//! Rendering the IR as Anthropic Messages responses, streaming and not.
//!
//! The full event sequence is emitted:
//!
//! ```text
//! message_start, ping,
//! (content_block_start, content_block_delta*, content_block_stop)*,
//! message_delta, message_stop
//! ```
//!
//! with `text_delta`, `thinking_delta`, `signature_delta` and `input_json_delta` deltas. A stream
//! that fails ends with an `error` event, never `message_stop`.

use std::collections::HashMap;

use serde_json::{json, Value as Json};

use super::sequencer::{BlockKind, Seq, Sequencer};
use super::{IrEvent, StreamErrorKind};
use crate::ir::{FinishReason, IrCompletion, Origin, Part, ReasoningDisplay, Usage};
use crate::signature::{sign, SignaturePayload, SigningKey};
use crate::sse::format_event;

/// Default cap on the interleaving buffer, 4 MiB.
pub const DEFAULT_BUFFER_CAP: usize = 4 * 1024 * 1024;

const REFUSAL_BLOCK_BASE: u32 = 0x8000_0000;

/// Everything the renderer needs besides the events.
#[derive(Debug, Clone)]
pub struct RenderCtx {
    /// The model name the client sent; responses echo it.
    pub model: String,
    /// The message id (`msg_…`), minted by the caller since the core has no randomness.
    pub message_id: String,
    /// The key that signs reasoning; without one, signatures are empty.
    pub signing_key: Option<SigningKey>,
    /// Who the reasoning comes from; bound into every signature.
    pub origin: Origin,
    /// Whether the client asked for thinking; if not, reasoning is not returned.
    pub thinking_wanted: bool,
    /// How reasoning is displayed; `Omitted` returns empty blocks.
    pub display: ReasoningDisplay,
    /// The input token estimate shown in `message_start`.
    pub est_input_tokens: u64,
    /// The cap on the interleaving buffer.
    pub buffer_cap: usize,
}

fn usage_json(u: &Usage) -> Json {
    let mut v = json!({
        "input_tokens": u.input_tokens,
        "output_tokens": u.output_tokens,
        "cache_creation_input_tokens": u.cache_creation_input_tokens,
        "cache_read_input_tokens": u.cache_read_input_tokens,
    });
    if u.reasoning_tokens > 0 {
        v["output_tokens_details"] = json!({"thinking_tokens": u.reasoning_tokens});
    }
    v
}

/// The Anthropic `stop_reason` for a finish.
#[must_use]
pub fn stop_reason(f: FinishReason) -> &'static str {
    match f {
        FinishReason::Stop => "end_turn",
        FinishReason::Length => "max_tokens",
        FinishReason::ToolCalls => "tool_use",
        FinishReason::Refusal => "refusal",
    }
}

fn signature_for(ctx: &RenderCtx, shown: &str, full: &str, opaque: Option<Vec<u8>>) -> String {
    let Some(key) = &ctx.signing_key else {
        return String::new();
    };
    let hidden =
        (ctx.display == ReasoningDisplay::Omitted && !full.is_empty()).then(|| full.to_owned());
    let payload = SignaturePayload {
        origin: ctx.origin.clone(),
        encrypted_content: opaque,
        hidden_text: hidden,
    };
    sign(key, &payload, shown)
}

/// Renders the stream of one response as Anthropic server-sent events.
#[derive(Debug)]
pub struct AnthropicRender {
    ctx: RenderCtx,
    seq: Sequencer,
    started: bool,
    terminal: bool,
    suppressed: std::collections::HashSet<u32>,
    texts: HashMap<u32, String>,
    kinds: HashMap<u32, BlockKind>,
    usage: Usage,
    refusal: bool,
    refusal_block: Option<u32>,
    refusals: u32,
}

impl AnthropicRender {
    /// A renderer for one response.
    #[must_use]
    pub fn new(ctx: RenderCtx) -> Self {
        let cap = ctx.buffer_cap;
        Self {
            ctx,
            seq: Sequencer::new(cap),
            started: false,
            terminal: false,
            suppressed: std::collections::HashSet::new(),
            texts: HashMap::new(),
            kinds: HashMap::new(),
            usage: Usage::default(),
            refusal: false,
            refusal_block: None,
            refusals: 0,
        }
    }

    /// A `ping` event, for keep-alive while the upstream is silent.
    #[must_use]
    pub fn ping() -> Vec<u8> {
        format_event(Some("ping"), r#"{"type":"ping"}"#)
    }

    /// Whether the stream has ended (finished or failed).
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.terminal
    }

    fn emit(name: &str, data: &Json) -> Vec<u8> {
        format_event(Some(name), &data.to_string())
    }

    fn message_start(&mut self) -> Vec<u8> {
        self.started = true;
        let mut out = Self::emit(
            "message_start",
            &json!({"type": "message_start", "message": {
                "id": self.ctx.message_id,
                "type": "message",
                "role": "assistant",
                "model": self.ctx.model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": self.ctx.est_input_tokens,
                    "output_tokens": 1,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0,
                },
            }}),
        );
        out.extend(Self::ping());
        out
    }

    /// Feeds one event and returns the bytes to send.
    pub fn push(&mut self, ev: &IrEvent) -> Vec<u8> {
        if self.terminal {
            return Vec::new();
        }
        let mut out = Vec::new();
        if !self.started && !matches!(ev, IrEvent::Error(_)) {
            out.extend(self.message_start());
        }
        match ev {
            IrEvent::Start { .. } => {}
            IrEvent::Usage(u) => self.usage = *u,
            IrEvent::Refusal { text } => {
                self.refusal = true;
                // Consecutive refusal chunks are one block.
                let block = match self.refusal_block {
                    Some(b) => b,
                    None => {
                        let b = REFUSAL_BLOCK_BASE + self.refusals;
                        self.refusals += 1;
                        self.refusal_block = Some(b);
                        b
                    }
                };
                out.extend(self.sequence(&IrEvent::TextDelta {
                    block,
                    text: text.clone(),
                }));
            }
            IrEvent::Finish(reason) => {
                out.extend(self.close_refusal());
                out.extend(self.finish_message(*reason));
            }
            IrEvent::Error(e) => {
                out.extend(self.error_event(e.kind, &e.message));
            }
            block_event => {
                out.extend(self.close_refusal());
                out.extend(self.sequence(block_event));
            }
        }
        out
    }

    fn close_refusal(&mut self) -> Vec<u8> {
        match self.refusal_block.take() {
            Some(block) => self.sequence(&IrEvent::TextEnd { block }),
            None => Vec::new(),
        }
    }

    /// Ends the render. Parsers always end a stream with a finish or an error, so this only emits
    /// something for a stream that was cut off before either: an `error` event.
    pub fn finish(&mut self) -> Vec<u8> {
        if self.terminal {
            return Vec::new();
        }
        self.error_event(
            StreamErrorKind::Truncated,
            "the response ended before it was complete",
        )
    }

    fn sequence(&mut self, ev: &IrEvent) -> Vec<u8> {
        // Reasoning is only returned when the client asked for it.
        let reasoning_block = match ev {
            IrEvent::ReasoningDelta { block, .. } | IrEvent::ReasoningSealed { block, .. } => {
                Some(*block)
            }
            _ => None,
        };
        if let Some(b) = reasoning_block {
            if !self.ctx.thinking_wanted {
                self.suppressed.insert(b);
            }
        }
        if reasoning_block.is_some_and(|b| self.suppressed.contains(&b)) {
            return Vec::new();
        }
        match self.seq.push(ev) {
            Ok(seqs) => self.render_seqs(seqs),
            Err(e) => self.error_event(
                StreamErrorKind::TooLarge,
                &format!("the interleaved tool-call buffer exceeded {} bytes", e.cap),
            ),
        }
    }

    fn render_seqs(&mut self, seqs: Vec<Seq>) -> Vec<u8> {
        let mut out = Vec::new();
        for s in seqs {
            match s {
                Seq::Start { index, kind } => {
                    let block = match &kind {
                        BlockKind::Text => json!({"type": "text", "text": ""}),
                        BlockKind::Reasoning => {
                            json!({"type": "thinking", "thinking": "", "signature": ""})
                        }
                        BlockKind::ToolCall { id, name } => {
                            json!({"type": "tool_use", "id": id, "name": name, "input": {}})
                        }
                    };
                    self.kinds.insert(index, kind);
                    out.extend(Self::emit(
                        "content_block_start",
                        &json!({"type": "content_block_start", "index": index, "content_block": block}),
                    ));
                }
                Seq::Delta { index, text } => {
                    let delta = match self.kinds.get(&index) {
                        Some(BlockKind::Reasoning) => {
                            self.texts.entry(index).or_default().push_str(&text);
                            if self.ctx.display == ReasoningDisplay::Omitted {
                                continue;
                            }
                            json!({"type": "thinking_delta", "thinking": text})
                        }
                        Some(BlockKind::ToolCall { .. }) => {
                            json!({"type": "input_json_delta", "partial_json": text})
                        }
                        _ => json!({"type": "text_delta", "text": text}),
                    };
                    out.extend(Self::emit(
                        "content_block_delta",
                        &json!({"type": "content_block_delta", "index": index, "delta": delta}),
                    ));
                }
                Seq::Stop { index, opaque } => {
                    if matches!(self.kinds.get(&index), Some(BlockKind::Reasoning)) {
                        let full = self.texts.remove(&index).unwrap_or_default();
                        let shown = if self.ctx.display == ReasoningDisplay::Omitted {
                            ""
                        } else {
                            full.as_str()
                        };
                        let signature = signature_for(&self.ctx, shown, &full, opaque);
                        out.extend(Self::emit(
                            "content_block_delta",
                            &json!({"type": "content_block_delta", "index": index,
                                "delta": {"type": "signature_delta", "signature": signature}}),
                        ));
                    }
                    out.extend(Self::emit(
                        "content_block_stop",
                        &json!({"type": "content_block_stop", "index": index}),
                    ));
                }
            }
        }
        out
    }

    fn finish_message(&mut self, reason: FinishReason) -> Vec<u8> {
        let mut out = Vec::new();
        let seqs = self.seq.finish();
        out.extend(self.render_seqs(seqs));
        let reason = if self.refusal {
            FinishReason::Refusal
        } else {
            reason
        };
        let mut delta = json!({"stop_reason": stop_reason(reason), "stop_sequence": null});
        if reason == FinishReason::Refusal {
            delta["stop_details"] = json!({"type": "refusal"});
        }
        out.extend(Self::emit(
            "message_delta",
            &json!({"type": "message_delta", "delta": delta, "usage": usage_json(&self.usage)}),
        ));
        out.extend(Self::emit("message_stop", &json!({"type": "message_stop"})));
        self.terminal = true;
        out
    }

    fn error_event(&mut self, kind: StreamErrorKind, message: &str) -> Vec<u8> {
        self.terminal = true;
        let ty = match kind {
            StreamErrorKind::Truncated | StreamErrorKind::Upstream | StreamErrorKind::Protocol => {
                "api_error"
            }
            StreamErrorKind::TooLarge => "request_too_large",
        };
        Self::emit(
            "error",
            &json!({"type": "error", "error": {"type": ty, "message": message}}),
        )
    }
}

/// Renders a complete (non-streaming) response as an Anthropic message.
#[must_use]
pub fn render_message(c: &IrCompletion, ctx: &RenderCtx) -> Vec<u8> {
    let mut content = Vec::new();
    for part in &c.content {
        match part {
            Part::Reasoning { text, opaque, .. } => {
                if !ctx.thinking_wanted {
                    continue;
                }
                let omitted = ctx.display == ReasoningDisplay::Omitted;
                let shown = if omitted { "" } else { text.as_str() };
                content.push(json!({
                    "type": "thinking",
                    "thinking": shown,
                    "signature": signature_for(ctx, shown, text, opaque.clone()),
                }));
            }
            Part::Text(t) => content.push(json!({"type": "text", "text": t})),
            Part::ToolCall { id, name, input } => {
                content.push(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
            }
            Part::Image(_) | Part::Document(_) => {}
        }
    }
    let mut v = json!({
        "id": ctx.message_id,
        "type": "message",
        "role": "assistant",
        "model": ctx.model,
        "content": content,
        "stop_reason": stop_reason(c.finish),
        "stop_sequence": null,
        "usage": usage_json(&c.usage),
    });
    if c.finish == FinishReason::Refusal {
        v["stop_details"] = json!({"type": "refusal"});
    }
    serde_json::to_vec(&v).expect("a JSON value serialises")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Protocol;
    use crate::signature::verify;
    use crate::sse::SseParser;
    use crate::stream::StreamError;

    fn origin() -> Origin {
        Origin {
            upstream: "up".into(),
            protocol: Protocol::Chat,
        }
    }

    fn ctx() -> RenderCtx {
        RenderCtx {
            model: "claude-x".into(),
            message_id: "msg_test".into(),
            signing_key: Some(SigningKey::from_token("tok")),
            origin: origin(),
            thinking_wanted: true,
            display: ReasoningDisplay::Summarized,
            est_input_tokens: 42,
            buffer_cap: 1 << 20,
        }
    }

    /// Runs events through the renderer and parses the SSE back into (event name, JSON).
    fn render(events: &[IrEvent], ctx: RenderCtx) -> Vec<(String, Json)> {
        let mut r = AnthropicRender::new(ctx);
        let mut bytes = Vec::new();
        for e in events {
            bytes.extend(r.push(e));
        }
        bytes.extend(r.finish());
        let mut p = SseParser::new();
        let mut parsed = p.push(&bytes).unwrap();
        parsed.extend(p.finish().unwrap());
        parsed
            .into_iter()
            .map(|e| (e.event.unwrap(), serde_json::from_str(&e.data).unwrap()))
            .collect()
    }

    fn names(evs: &[(String, Json)]) -> Vec<&str> {
        evs.iter().map(|(n, _)| n.as_str()).collect()
    }

    fn text(block: u32, t: &str) -> IrEvent {
        IrEvent::TextDelta {
            block,
            text: t.into(),
        }
    }

    fn reasoning(block: u32, t: &str) -> IrEvent {
        IrEvent::ReasoningDelta {
            block,
            text: t.into(),
        }
    }

    #[test]
    fn a_text_response_has_the_full_event_sequence() {
        let evs = render(
            &[
                IrEvent::Start {
                    id: Some("u".into()),
                    model: Some("m".into()),
                },
                text(0, "Hel"),
                text(0, "lo"),
                IrEvent::TextEnd { block: 0 },
                IrEvent::Usage(Usage {
                    input_tokens: 10,
                    output_tokens: 3,
                    cache_read_input_tokens: 4,
                    ..Usage::default()
                }),
                IrEvent::Finish(FinishReason::Stop),
            ],
            ctx(),
        );
        assert_eq!(
            names(&evs),
            [
                "message_start",
                "ping",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        let start = &evs[0].1["message"];
        assert_eq!(
            (
                start["id"].as_str(),
                start["model"].as_str(),
                start["role"].as_str()
            ),
            (Some("msg_test"), Some("claude-x"), Some("assistant"))
        );
        assert_eq!(start["usage"]["input_tokens"], 42);
        assert_eq!(
            evs[2].1["content_block"],
            json!({"type": "text", "text": ""})
        );
        assert_eq!(
            evs[3].1["delta"],
            json!({"type": "text_delta", "text": "Hel"})
        );
        let delta = &evs[6].1;
        assert_eq!(delta["delta"]["stop_reason"], "end_turn");
        assert_eq!(delta["usage"]["input_tokens"], 10);
        assert_eq!(delta["usage"]["output_tokens"], 3);
        assert_eq!(delta["usage"]["cache_read_input_tokens"], 4);
        assert_eq!(delta["usage"]["cache_creation_input_tokens"], 0);
        assert!(delta["usage"].get("output_tokens_details").is_none());
    }

    #[test]
    fn reasoning_streams_as_thinking_with_a_verifiable_signature() {
        let evs = render(
            &[
                reasoning(0, "I "),
                reasoning(0, "think"),
                IrEvent::ReasoningSealed {
                    block: 0,
                    opaque: Some(b"ENC".to_vec()),
                },
                text(1, "ok"),
                IrEvent::TextEnd { block: 1 },
                IrEvent::Usage(Usage {
                    reasoning_tokens: 7,
                    output_tokens: 9,
                    ..Usage::default()
                }),
                IrEvent::Finish(FinishReason::Stop),
            ],
            ctx(),
        );
        assert_eq!(evs[2].1["content_block"]["type"], "thinking");
        assert_eq!(
            evs[3].1["delta"],
            json!({"type": "thinking_delta", "thinking": "I "})
        );
        let sig = evs
            .iter()
            .find_map(|(_, j)| j.pointer("/delta/signature").and_then(Json::as_str))
            .unwrap();
        let payload = verify(&SigningKey::from_token("tok"), sig, "I think").unwrap();
        assert_eq!(payload.origin, origin());
        assert_eq!(payload.encrypted_content.as_deref(), Some(&b"ENC"[..]));
        assert_eq!(payload.hidden_text, None);
        let md = evs.iter().find(|(n, _)| n == "message_delta").unwrap();
        assert_eq!(md.1["usage"]["output_tokens_details"]["thinking_tokens"], 7);
        // The signature delta comes before the block closes, and blocks are indexed 0 then 1.
        let idx: Vec<_> = evs
            .iter()
            .filter(|(n, _)| n == "content_block_start")
            .map(|(_, j)| j["index"].as_u64().unwrap())
            .collect();
        assert_eq!(idx, [0, 1]);
    }

    #[test]
    fn omitted_display_sends_empty_thinking_and_hides_the_text_in_the_signature() {
        let mut c = ctx();
        c.display = ReasoningDisplay::Omitted;
        let evs = render(
            &[
                reasoning(0, "secret"),
                IrEvent::ReasoningSealed {
                    block: 0,
                    opaque: None,
                },
                IrEvent::Finish(FinishReason::Stop),
            ],
            c,
        );
        assert!(!evs.iter().any(
            |(_, j)| j.pointer("/delta/type").and_then(Json::as_str) == Some("thinking_delta")
        ));
        let sig = evs
            .iter()
            .find_map(|(_, j)| j.pointer("/delta/signature").and_then(Json::as_str))
            .unwrap();
        let payload = verify(&SigningKey::from_token("tok"), sig, "").unwrap();
        assert_eq!(payload.hidden_text.as_deref(), Some("secret"));
    }

    #[test]
    fn reasoning_is_dropped_when_the_client_did_not_ask_for_thinking() {
        let mut c = ctx();
        c.thinking_wanted = false;
        let evs = render(
            &[
                reasoning(0, "hidden"),
                IrEvent::ReasoningSealed {
                    block: 0,
                    opaque: None,
                },
                text(1, "answer"),
                IrEvent::TextEnd { block: 1 },
                IrEvent::Finish(FinishReason::Stop),
            ],
            c,
        );
        assert!(!evs.iter().any(|(_, j)| j.to_string().contains("thinking")));
        let starts: Vec<_> = evs
            .iter()
            .filter(|(n, _)| n == "content_block_start")
            .collect();
        assert_eq!(starts.len(), 1);
        assert_eq!(
            starts[0].1["index"], 0,
            "no gap is left where the reasoning was"
        );
    }

    #[test]
    fn without_a_signing_key_the_signature_is_empty() {
        let mut c = ctx();
        c.signing_key = None;
        let evs = render(
            &[
                reasoning(0, "x"),
                IrEvent::ReasoningSealed {
                    block: 0,
                    opaque: None,
                },
                IrEvent::Finish(FinishReason::Stop),
            ],
            c,
        );
        let sig = evs
            .iter()
            .find_map(|(_, j)| j.pointer("/delta/signature").and_then(Json::as_str))
            .unwrap();
        assert_eq!(sig, "");
    }

    #[test]
    fn tool_calls_stream_as_tool_use_with_input_json_deltas() {
        let evs = render(
            &[
                IrEvent::ToolCallStart {
                    block: 0,
                    id: "toolu_1".into(),
                    name: "read".into(),
                },
                IrEvent::ToolCallArgs {
                    block: 0,
                    fragment: "{\"p\":".into(),
                },
                IrEvent::ToolCallArgs {
                    block: 0,
                    fragment: "1}".into(),
                },
                IrEvent::ToolCallEnd { block: 0 },
                IrEvent::Finish(FinishReason::ToolCalls),
            ],
            ctx(),
        );
        assert_eq!(
            evs[2].1["content_block"],
            json!({"type": "tool_use", "id": "toolu_1", "name": "read", "input": {}})
        );
        assert_eq!(
            evs[3].1["delta"],
            json!({"type": "input_json_delta", "partial_json": "{\"p\":"})
        );
        assert_eq!(
            evs.iter().find(|(n, _)| n == "message_delta").unwrap().1["delta"]["stop_reason"],
            "tool_use"
        );
    }

    #[test]
    fn interleaved_parallel_tool_calls_become_a_valid_sequence() {
        let evs = render(
            &[
                IrEvent::ToolCallStart {
                    block: 0,
                    id: "a".into(),
                    name: "f".into(),
                },
                IrEvent::ToolCallStart {
                    block: 1,
                    id: "b".into(),
                    name: "g".into(),
                },
                IrEvent::ToolCallArgs {
                    block: 0,
                    fragment: "{\"x\":".into(),
                },
                IrEvent::ToolCallArgs {
                    block: 1,
                    fragment: "{}".into(),
                },
                IrEvent::ToolCallEnd { block: 1 },
                IrEvent::ToolCallArgs {
                    block: 0,
                    fragment: "1}".into(),
                },
                IrEvent::ToolCallEnd { block: 0 },
                IrEvent::Finish(FinishReason::ToolCalls),
            ],
            ctx(),
        );
        // Strictly nested: start 0 .. stop 0, start 1 .. stop 1.
        let shape: Vec<(String, u64)> = evs
            .iter()
            .filter(|(n, _)| n.starts_with("content_block"))
            .map(|(n, j)| (n.clone(), j["index"].as_u64().unwrap()))
            .collect();
        let want = [
            ("content_block_start", 0),
            ("content_block_delta", 0),
            ("content_block_delta", 0),
            ("content_block_stop", 0),
            ("content_block_start", 1),
            ("content_block_delta", 1),
            ("content_block_stop", 1),
        ];
        assert_eq!(
            shape,
            want.iter()
                .map(|(n, i)| (n.to_string(), *i))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_refusal_ends_with_stop_reason_refusal_and_details() {
        let evs = render(
            &[
                IrEvent::Refusal {
                    text: "I can't help with that.".into(),
                },
                IrEvent::Finish(FinishReason::Stop),
            ],
            ctx(),
        );
        assert!(evs
            .iter()
            .any(|(_, j)| j.pointer("/delta/text").and_then(Json::as_str)
                == Some("I can't help with that.")));
        let md = &evs.iter().find(|(n, _)| n == "message_delta").unwrap().1["delta"];
        assert_eq!(md["stop_reason"], "refusal");
        assert_eq!(md["stop_details"], json!({"type": "refusal"}));
    }

    #[test]
    fn consecutive_refusal_chunks_are_one_text_block() {
        let evs = render(
            &[
                IrEvent::Refusal {
                    text: "I can't ".into(),
                },
                IrEvent::Refusal {
                    text: "help.".into(),
                },
                text(1, "after"),
                IrEvent::TextEnd { block: 1 },
                IrEvent::Finish(FinishReason::Stop),
            ],
            ctx(),
        );
        let starts = evs
            .iter()
            .filter(|(n, _)| n == "content_block_start")
            .count();
        assert_eq!(
            starts, 2,
            "one block for the refusal, one for the text after it"
        );
        let first: String = evs
            .iter()
            .filter(|(_, j)| j["index"] == 0)
            .filter_map(|(_, j)| j.pointer("/delta/text").and_then(Json::as_str))
            .collect();
        assert_eq!(first, "I can't help.");
    }

    #[test]
    fn finish_reasons_map_to_stop_reasons() {
        for (f, want) in [
            (FinishReason::Stop, "end_turn"),
            (FinishReason::Length, "max_tokens"),
            (FinishReason::ToolCalls, "tool_use"),
            (FinishReason::Refusal, "refusal"),
        ] {
            assert_eq!(stop_reason(f), want);
        }
    }

    #[test]
    fn an_error_ends_the_stream_with_an_error_event_and_no_message_stop() {
        let evs = render(
            &[text(0, "partial"), IrEvent::Error(StreamError::truncated())],
            ctx(),
        );
        let last = evs.last().unwrap();
        assert_eq!(last.0, "error");
        assert_eq!(last.1["error"]["type"], "api_error");
        assert!(!names(&evs).contains(&"message_stop"));
        assert!(!names(&evs).contains(&"message_delta"));
    }

    #[test]
    fn an_error_before_anything_else_is_just_an_error_event() {
        let evs = render(
            &[IrEvent::Error(StreamError {
                kind: StreamErrorKind::Upstream,
                message: "boom".into(),
            })],
            ctx(),
        );
        assert_eq!(names(&evs), ["error"]);
        assert_eq!(evs[0].1["error"]["message"], "boom");
    }

    #[test]
    fn a_stream_cut_off_before_any_terminal_event_gets_an_error() {
        let evs = render(&[text(0, "x")], ctx());
        assert_eq!(evs.last().unwrap().0, "error");
    }

    #[test]
    fn nothing_is_emitted_after_the_end() {
        let mut r = AnthropicRender::new(ctx());
        assert!(!r.push(&IrEvent::Finish(FinishReason::Stop)).is_empty());
        assert!(r.is_done());
        assert!(r.push(&text(0, "late")).is_empty());
        assert!(r.finish().is_empty());
    }

    #[test]
    fn exceeding_the_interleave_buffer_fails_the_stream() {
        let mut c = ctx();
        c.buffer_cap = 4;
        let evs = render(
            &[
                IrEvent::ToolCallStart {
                    block: 0,
                    id: "a".into(),
                    name: "f".into(),
                },
                IrEvent::ToolCallStart {
                    block: 1,
                    id: "b".into(),
                    name: "g".into(),
                },
                IrEvent::ToolCallArgs {
                    block: 1,
                    fragment: "0123456789".into(),
                },
            ],
            c,
        );
        let last = evs.last().unwrap();
        assert_eq!(last.0, "error");
        assert_eq!(last.1["error"]["type"], "request_too_large");
    }

    #[test]
    fn ping_is_a_ping_event() {
        assert_eq!(
            AnthropicRender::ping(),
            b"event: ping\ndata: {\"type\":\"ping\"}\n\n"
        );
    }

    #[test]
    fn the_start_is_emitted_once_even_with_a_start_event() {
        let evs = render(
            &[
                IrEvent::Start {
                    id: None,
                    model: None,
                },
                text(0, "x"),
                IrEvent::TextEnd { block: 0 },
                IrEvent::Finish(FinishReason::Stop),
            ],
            ctx(),
        );
        assert_eq!(
            names(&evs)
                .iter()
                .filter(|n| **n == "message_start")
                .count(),
            1
        );
    }

    // ---- non-streaming ----

    fn completion(content: Vec<Part>, finish: FinishReason) -> IrCompletion {
        IrCompletion {
            content,
            finish,
            usage: Usage {
                input_tokens: 5,
                output_tokens: 6,
                ..Usage::default()
            },
            upstream_id: None,
            upstream_model: None,
        }
    }

    fn message(c: &IrCompletion, ctx: &RenderCtx) -> Json {
        serde_json::from_slice(&render_message(c, ctx)).unwrap()
    }

    #[test]
    fn renders_a_complete_message() {
        let c = completion(
            vec![
                Part::Reasoning {
                    text: "hm".into(),
                    display: ReasoningDisplay::Summarized,
                    origin: Some(origin()),
                    opaque: Some(b"E".to_vec()),
                },
                Part::Text("answer".into()),
                Part::ToolCall {
                    id: "t".into(),
                    name: "n".into(),
                    input: json!({"a": 1}),
                },
            ],
            FinishReason::ToolCalls,
        );
        let m = message(&c, &ctx());
        assert_eq!(
            (
                m["id"].as_str(),
                m["type"].as_str(),
                m["role"].as_str(),
                m["model"].as_str()
            ),
            (
                Some("msg_test"),
                Some("message"),
                Some("assistant"),
                Some("claude-x")
            )
        );
        assert_eq!(m["stop_reason"], "tool_use");
        assert_eq!(m["stop_sequence"], Json::Null);
        assert_eq!(m["content"][0]["type"], "thinking");
        assert_eq!(m["content"][0]["thinking"], "hm");
        let sig = m["content"][0]["signature"].as_str().unwrap();
        assert_eq!(
            verify(&SigningKey::from_token("tok"), sig, "hm")
                .unwrap()
                .encrypted_content
                .as_deref(),
            Some(&b"E"[..])
        );
        assert_eq!(m["content"][1], json!({"type": "text", "text": "answer"}));
        assert_eq!(
            m["content"][2],
            json!({"type": "tool_use", "id": "t", "name": "n", "input": {"a": 1}})
        );
        assert_eq!(m["usage"]["input_tokens"], 5);
    }

    #[test]
    fn complete_messages_honour_thinking_wanted_omitted_and_refusals() {
        let c = completion(
            vec![
                Part::Reasoning {
                    text: "hm".into(),
                    display: ReasoningDisplay::Summarized,
                    origin: None,
                    opaque: None,
                },
                Part::Text("x".into()),
            ],
            FinishReason::Refusal,
        );
        let mut off = ctx();
        off.thinking_wanted = false;
        let m = message(&c, &off);
        assert_eq!(m["content"].as_array().unwrap().len(), 1);
        assert_eq!(m["stop_reason"], "refusal");
        assert_eq!(m["stop_details"], json!({"type": "refusal"}));
        let mut omitted = ctx();
        omitted.display = ReasoningDisplay::Omitted;
        let m = message(&c, &omitted);
        assert_eq!(m["content"][0]["thinking"], "");
        let sig = m["content"][0]["signature"].as_str().unwrap();
        assert_eq!(
            verify(&SigningKey::from_token("tok"), sig, "")
                .unwrap()
                .hidden_text
                .as_deref(),
            Some("hm")
        );
    }
}
