//! Rendering the IR as Gemini `GenerateContentResponse` bodies, streaming and not.
//!
//! Usage is Gemini-shaped: `promptTokenCount` includes cached tokens, `candidatesTokenCount`
//! excludes thinking tokens, which are reported as `thoughtsTokenCount`. Thought summaries are
//! returned (as parts marked `thought`) only when the client asked for them.

use std::collections::HashMap;

use serde_json::{json, Value as Json};

use super::anthropic_render::RenderCtx;
use super::{IrEvent, StreamErrorKind, StreamRender};
use crate::ir::{FinishReason, IrCompletion, Part, Usage};
use crate::sse::{format_comment, format_event};

/// The Gemini `finishReason` for a finish.
#[must_use]
pub fn finish_reason(f: FinishReason, refused_with_text: bool) -> &'static str {
    match f {
        FinishReason::Stop | FinishReason::ToolCalls => "STOP",
        FinishReason::Length => "MAX_TOKENS",
        FinishReason::Refusal if refused_with_text => "STOP",
        FinishReason::Refusal => "SAFETY",
    }
}

/// Gemini-shaped usage.
#[must_use]
pub fn usage_json(u: &Usage) -> Json {
    let prompt = u.input_tokens + u.cache_read_input_tokens;
    let mut v = json!({
        "promptTokenCount": prompt,
        "candidatesTokenCount": u.output_tokens.saturating_sub(u.reasoning_tokens),
        "totalTokenCount": prompt + u.output_tokens,
    });
    if u.cache_read_input_tokens > 0 {
        v["cachedContentTokenCount"] = json!(u.cache_read_input_tokens);
    }
    if u.reasoning_tokens > 0 {
        v["thoughtsTokenCount"] = json!(u.reasoning_tokens);
    }
    v
}

fn response_id(ctx: &RenderCtx) -> String {
    ctx.message_id.trim_start_matches("msg_").to_owned()
}

fn envelope(ctx: &RenderCtx, parts: &[Json], finish: Option<&str>, usage: Option<Json>) -> Json {
    let mut candidate = json!({"index": 0});
    if !parts.is_empty() || finish.is_some() {
        candidate["content"] = json!({"role": "model", "parts": parts});
    }
    if let Some(f) = finish {
        candidate["finishReason"] = json!(f);
    }
    let mut v = json!({
        "candidates": [candidate],
        "modelVersion": ctx.model,
        "responseId": response_id(ctx),
    });
    if let Some(u) = usage {
        v["usageMetadata"] = u;
    }
    v
}

/// Renders a complete (non-streaming) response.
#[must_use]
pub fn render_response(c: &IrCompletion, ctx: &RenderCtx) -> Vec<u8> {
    let mut parts: Vec<Json> = Vec::new();
    let mut has_text = false;
    for part in &c.content {
        match part {
            Part::Text(t) if !t.is_empty() => {
                has_text = true;
                parts.push(json!({"text": t}));
            }
            Part::Reasoning { text, .. } if ctx.thinking_wanted && !text.is_empty() => {
                parts.push(json!({"text": text, "thought": true}));
            }
            Part::ToolCall { id, name, input } => {
                parts.push(json!({"functionCall": {"id": id, "name": name, "args": input}}));
            }
            _ => {}
        }
    }
    let refused = c.finish == FinishReason::Refusal && has_text;
    let v = envelope(
        ctx,
        &parts,
        Some(finish_reason(c.finish, refused)),
        Some(usage_json(&c.usage)),
    );
    serde_json::to_vec(&v).expect("a JSON value serialises")
}

#[derive(Debug, Default)]
struct PendingCall {
    id: String,
    name: String,
    args: String,
}

/// Renders a stream as `GenerateContentResponse` events (`alt=sse`).
#[derive(Debug)]
pub struct GeminiRender {
    ctx: RenderCtx,
    terminal: bool,
    usage: Usage,
    calls: HashMap<u32, PendingCall>,
    refusal_text: bool,
}

impl GeminiRender {
    /// A renderer for one response.
    #[must_use]
    pub fn new(ctx: RenderCtx) -> Self {
        Self {
            ctx,
            terminal: false,
            usage: Usage::default(),
            calls: HashMap::new(),
            refusal_text: false,
        }
    }

    fn chunk(&self, parts: &[Json], finish: Option<&str>, usage: Option<Json>) -> Vec<u8> {
        format_event(None, &envelope(&self.ctx, parts, finish, usage).to_string())
    }

    fn error(&mut self, kind: StreamErrorKind, message: &str) -> Vec<u8> {
        self.terminal = true;
        let (code, status) = match kind {
            StreamErrorKind::Truncated | StreamErrorKind::Upstream => (503, "UNAVAILABLE"),
            StreamErrorKind::Protocol => (502, "UNAVAILABLE"),
            StreamErrorKind::TooLarge => (413, "INVALID_ARGUMENT"),
        };
        format_event(
            None,
            &json!({"error": {"code": code, "message": message, "status": status}}).to_string(),
        )
    }
}

impl StreamRender for GeminiRender {
    fn push(&mut self, ev: &IrEvent) -> Vec<u8> {
        if self.terminal {
            return Vec::new();
        }
        let mut out = Vec::new();
        match ev {
            IrEvent::TextDelta { text, .. } => {
                out.extend(self.chunk(&[json!({"text": text})], None, None));
            }
            IrEvent::ReasoningDelta { text, .. } => {
                if self.ctx.thinking_wanted {
                    out.extend(self.chunk(&[json!({"text": text, "thought": true})], None, None));
                }
            }
            IrEvent::ToolCallStart { block, id, name } => {
                self.calls.insert(
                    *block,
                    PendingCall {
                        id: id.clone(),
                        name: name.clone(),
                        args: String::new(),
                    },
                );
            }
            IrEvent::ToolCallArgs { block, fragment } => {
                if let Some(c) = self.calls.get_mut(block) {
                    c.args.push_str(fragment);
                }
            }
            IrEvent::ToolCallEnd { block } => {
                if let Some(c) = self.calls.remove(block) {
                    let args: Json = if c.args.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&c.args).unwrap_or_else(|_| json!({}))
                    };
                    out.extend(self.chunk(
                        &[json!({"functionCall": {"id": c.id, "name": c.name, "args": args}})],
                        None,
                        None,
                    ));
                }
            }
            IrEvent::Refusal { text } => {
                self.refusal_text = true;
                out.extend(self.chunk(&[json!({"text": text})], None, None));
            }
            IrEvent::Usage(u) => self.usage = *u,
            IrEvent::Finish(reason) => {
                out.extend(self.chunk(
                    &[],
                    Some(finish_reason(*reason, self.refusal_text)),
                    Some(usage_json(&self.usage)),
                ));
                self.terminal = true;
            }
            IrEvent::Error(e) => out.extend(self.error(e.kind, &e.message)),
            IrEvent::Start { .. } | IrEvent::TextEnd { .. } | IrEvent::ReasoningSealed { .. } => {}
        }
        out
    }

    fn finish(&mut self) -> Vec<u8> {
        if self.terminal {
            return Vec::new();
        }
        self.error(
            StreamErrorKind::Truncated,
            "the response ended before it was complete",
        )
    }

    fn keepalive(&self) -> Vec<u8> {
        format_comment("keep-alive")
    }

    fn is_done(&self) -> bool {
        self.terminal
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Origin, Protocol, ReasoningDisplay};
    use crate::sse::SseParser;
    use crate::stream::StreamError;

    fn ctx() -> RenderCtx {
        RenderCtx {
            model: "gemini-2.5-pro".into(),
            message_id: "msg_abc".into(),
            signing_key: None,
            origin: Origin {
                upstream: "up".into(),
                protocol: Protocol::Chat,
            },
            thinking_wanted: true,
            display: ReasoningDisplay::Summarized,
            est_input_tokens: 0,
            buffer_cap: 1 << 20,
            created: 1_700_000_000,
            include_usage: true,
        }
    }

    fn render(events: &[IrEvent], ctx: RenderCtx) -> Vec<Json> {
        let mut r = GeminiRender::new(ctx);
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
            .map(|e| serde_json::from_str(&e.data).unwrap())
            .collect()
    }

    #[test]
    fn usage_follows_gemini_conventions() {
        let u = usage_json(&Usage {
            input_tokens: 70,
            output_tokens: 25,
            cache_read_input_tokens: 30,
            reasoning_tokens: 5,
            ..Usage::default()
        });
        assert_eq!(
            u,
            json!({"promptTokenCount": 100, "candidatesTokenCount": 20, "totalTokenCount": 125, "cachedContentTokenCount": 30, "thoughtsTokenCount": 5})
        );
        let plain = usage_json(&Usage {
            input_tokens: 3,
            output_tokens: 2,
            ..Usage::default()
        });
        assert!(
            plain.get("cachedContentTokenCount").is_none()
                && plain.get("thoughtsTokenCount").is_none()
        );
    }

    #[test]
    fn finish_reasons_map() {
        assert_eq!(finish_reason(FinishReason::Stop, false), "STOP");
        assert_eq!(finish_reason(FinishReason::ToolCalls, false), "STOP");
        assert_eq!(finish_reason(FinishReason::Length, false), "MAX_TOKENS");
        assert_eq!(finish_reason(FinishReason::Refusal, false), "SAFETY");
        assert_eq!(finish_reason(FinishReason::Refusal, true), "STOP");
    }

    #[test]
    fn a_whole_response_has_text_thoughts_calls_usage_and_the_clients_model() {
        let c = IrCompletion {
            content: vec![
                Part::Reasoning {
                    text: "hmm".into(),
                    display: ReasoningDisplay::Summarized,
                    origin: None,
                    opaque: None,
                },
                Part::Text("answer".into()),
                Part::ToolCall {
                    id: "c1".into(),
                    name: "t".into(),
                    input: json!({"a": 1}),
                },
            ],
            finish: FinishReason::ToolCalls,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Usage::default()
            },
            upstream_id: None,
            upstream_model: None,
        };
        let v: Json = serde_json::from_slice(&render_response(&c, &ctx())).unwrap();
        assert_eq!(v["modelVersion"], "gemini-2.5-pro");
        assert_eq!(v["candidates"][0]["finishReason"], "STOP");
        assert_eq!(
            v["candidates"][0]["content"],
            json!({"role": "model", "parts": [{"text": "hmm", "thought": true}, {"text": "answer"}, {"functionCall": {"id": "c1", "name": "t", "args": {"a": 1}}}]})
        );
        assert_eq!(v["usageMetadata"]["promptTokenCount"], 10);
        let mut quiet = ctx();
        quiet.thinking_wanted = false;
        let v: Json = serde_json::from_slice(&render_response(&c, &quiet)).unwrap();
        assert_eq!(
            v["candidates"][0]["content"]["parts"]
                .as_array()
                .unwrap()
                .len(),
            2,
            "thoughts only when asked for"
        );
    }

    #[test]
    fn a_refusal_with_no_text_is_safety_and_with_text_is_an_answer() {
        let blocked = IrCompletion {
            content: vec![],
            finish: FinishReason::Refusal,
            usage: Usage::default(),
            upstream_id: None,
            upstream_model: None,
        };
        let v: Json = serde_json::from_slice(&render_response(&blocked, &ctx())).unwrap();
        assert_eq!(v["candidates"][0]["finishReason"], "SAFETY");
        let said = IrCompletion {
            content: vec![Part::Text("I can't".into())],
            ..blocked
        };
        let v: Json = serde_json::from_slice(&render_response(&said, &ctx())).unwrap();
        assert_eq!(v["candidates"][0]["finishReason"], "STOP");
    }

    #[test]
    fn a_stream_is_chunks_then_a_final_chunk_with_finish_and_usage() {
        let chunks = render(
            &[
                IrEvent::Start {
                    id: None,
                    model: None,
                },
                IrEvent::ReasoningDelta {
                    block: 0,
                    text: "think".into(),
                },
                IrEvent::ReasoningSealed {
                    block: 0,
                    opaque: None,
                },
                IrEvent::TextDelta {
                    block: 1,
                    text: "Hel".into(),
                },
                IrEvent::TextDelta {
                    block: 1,
                    text: "lo".into(),
                },
                IrEvent::TextEnd { block: 1 },
                IrEvent::ToolCallStart {
                    block: 2,
                    id: "c".into(),
                    name: "t".into(),
                },
                IrEvent::ToolCallArgs {
                    block: 2,
                    fragment: "{\"x\":".into(),
                },
                IrEvent::ToolCallArgs {
                    block: 2,
                    fragment: "1}".into(),
                },
                IrEvent::ToolCallEnd { block: 2 },
                IrEvent::Usage(Usage {
                    input_tokens: 4,
                    output_tokens: 3,
                    ..Usage::default()
                }),
                IrEvent::Finish(FinishReason::ToolCalls),
            ],
            ctx(),
        );
        let part = |i: usize| chunks[i]["candidates"][0]["content"]["parts"][0].clone();
        assert_eq!(part(0), json!({"text": "think", "thought": true}));
        assert_eq!(
            (part(1), part(2)),
            (json!({"text": "Hel"}), json!({"text": "lo"}))
        );
        assert_eq!(
            part(3),
            json!({"functionCall": {"id": "c", "name": "t", "args": {"x": 1}}})
        );
        let last = chunks.last().unwrap();
        assert_eq!(last["candidates"][0]["finishReason"], "STOP");
        assert_eq!(last["usageMetadata"]["totalTokenCount"], 7);
        assert!(chunks.iter().all(|c| c["modelVersion"] == "gemini-2.5-pro"));
    }

    #[test]
    fn a_cut_or_failed_stream_ends_with_an_error_object_not_a_finish() {
        let cut = render(
            &[IrEvent::TextDelta {
                block: 0,
                text: "par".into(),
            }],
            ctx(),
        );
        assert_eq!(cut.last().unwrap()["error"]["status"], "UNAVAILABLE");
        assert!(cut.iter().all(|c| c
            .get("candidates")
            .is_none_or(|cs| cs[0].get("finishReason").is_none())));
        let failed = render(
            &[IrEvent::Error(StreamError {
                kind: StreamErrorKind::Upstream,
                message: "boom".into(),
            })],
            ctx(),
        );
        assert_eq!(failed[0]["error"]["message"], "boom");
        assert_eq!(failed.len(), 1);
    }

    #[test]
    fn thoughts_are_dropped_from_the_stream_unless_asked_for() {
        let mut quiet = ctx();
        quiet.thinking_wanted = false;
        let chunks = render(
            &[
                IrEvent::ReasoningDelta {
                    block: 0,
                    text: "x".into(),
                },
                IrEvent::TextDelta {
                    block: 1,
                    text: "y".into(),
                },
                IrEvent::Finish(FinishReason::Stop),
            ],
            quiet,
        );
        assert_eq!(chunks.len(), 2);
        assert_eq!(
            chunks[0]["candidates"][0]["content"]["parts"][0],
            json!({"text": "y"})
        );
    }

    #[test]
    fn the_gemini_error_envelope_carries_code_message_and_status() {
        let (status, body) =
            crate::error::GatewayError::new(crate::error::ErrorKind::RateLimit, "slow down")
                .render(Protocol::Gemini);
        assert_eq!(status, 429);
        let v: Json = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v,
            json!({"error": {"code": 429, "message": "slow down", "status": "RESOURCE_EXHAUSTED"}})
        );
    }
}
