//! Rendering the IR as OpenAI Responses responses, streaming and not.
//!
//! A reasoning item's `encrypted_content` carries the gateway's signature, which in turn carries
//! the upstream's own encrypted state, so a client that echoes the item back lets the next turn
//! replay the reasoning even with `store: false`.

use serde_json::{json, Value as Json};

use super::anthropic_render::RenderCtx;
use super::{IrEvent, StreamErrorKind, StreamRender};
use crate::ir::{FinishReason, IrCompletion, Part, ReasoningDisplay, Usage};
use crate::signature::{sign, SignaturePayload};
use crate::sse::{format_comment, format_event};

/// OpenAI Responses usage.
#[must_use]
pub fn usage_json(u: &Usage) -> Json {
    let input = u.input_tokens + u.cache_read_input_tokens;
    json!({
        "input_tokens": input,
        "output_tokens": u.output_tokens,
        "total_tokens": input + u.output_tokens,
        "input_tokens_details": {"cached_tokens": u.cache_read_input_tokens},
        "output_tokens_details": {"reasoning_tokens": u.reasoning_tokens},
    })
}

fn response_id(ctx: &RenderCtx) -> String {
    format!("resp_{}", ctx.message_id.trim_start_matches("msg_"))
}

fn item_id(ctx: &RenderCtx, prefix: &str, n: usize) -> String {
    format!("{prefix}_{}_{n}", ctx.message_id.trim_start_matches("msg_"))
}

fn signature(ctx: &RenderCtx, shown: &str, full: &str, opaque: Option<Vec<u8>>) -> String {
    let Some(key) = &ctx.signing_key else {
        return String::new();
    };
    let hidden =
        (ctx.display == ReasoningDisplay::Omitted && !full.is_empty()).then(|| full.to_owned());
    sign(
        key,
        &SignaturePayload {
            origin: ctx.origin.clone(),
            encrypted_content: opaque,
            hidden_text: hidden,
        },
        shown,
    )
}

fn reasoning_item(ctx: &RenderCtx, id: &str, text: &str, opaque: Option<Vec<u8>>) -> Json {
    let omitted = ctx.display == ReasoningDisplay::Omitted;
    let shown = if omitted { "" } else { text };
    let summary = if shown.is_empty() {
        json!([])
    } else {
        json!([{"type": "summary_text", "text": shown}])
    };
    json!({"type": "reasoning", "id": id, "summary": summary, "encrypted_content": signature(ctx, shown, text, opaque)})
}

fn message_item(id: &str, text: &str, refusal: bool, status: &str) -> Json {
    let part = if refusal {
        json!({"type": "refusal", "refusal": text})
    } else {
        json!({"type": "output_text", "text": text, "annotations": []})
    };
    json!({"type": "message", "id": id, "status": status, "role": "assistant", "content": [part]})
}

fn call_item(id: &str, call_id: &str, name: &str, args: &str, status: &str) -> Json {
    json!({"type": "function_call", "id": id, "call_id": call_id, "name": name, "arguments": args, "status": status})
}

fn response_object(
    ctx: &RenderCtx,
    status: &str,
    output: Vec<Json>,
    usage: Option<&Usage>,
    incomplete: Option<&str>,
) -> Json {
    let mut v = json!({
        "id": response_id(ctx),
        "object": "response",
        "created_at": ctx.created,
        "status": status,
        "model": ctx.model,
        "output": output,
        "error": null,
        "incomplete_details": incomplete.map(|r| json!({"reason": r})),
    });
    if let Some(u) = usage {
        v["usage"] = usage_json(u);
    }
    v
}

fn incomplete_reason(f: FinishReason) -> Option<&'static str> {
    match f {
        FinishReason::Length => Some("max_output_tokens"),
        FinishReason::Refusal => Some("content_filter"),
        FinishReason::Stop | FinishReason::ToolCalls => None,
    }
}

/// Renders a complete (non-streaming) response.
#[must_use]
pub fn render_response(c: &IrCompletion, ctx: &RenderCtx) -> Vec<u8> {
    let mut output = Vec::new();
    let mut n = 0;
    for part in &c.content {
        match part {
            Part::Reasoning { text, opaque, .. } if ctx.thinking_wanted => {
                output.push(reasoning_item(
                    ctx,
                    &item_id(ctx, "rs", n),
                    text,
                    opaque.clone(),
                ));
                n += 1;
            }
            Part::Text(t) => {
                let refusal = c.finish == FinishReason::Refusal;
                output.push(message_item(
                    &item_id(ctx, "msg", n),
                    t,
                    refusal,
                    "completed",
                ));
                n += 1;
            }
            Part::ToolCall { id, name, input } => {
                output.push(call_item(
                    &item_id(ctx, "fc", n),
                    id,
                    name,
                    &input.to_string(),
                    "completed",
                ));
                n += 1;
            }
            _ => {}
        }
    }
    let incomplete = incomplete_reason(c.finish).filter(|_| c.finish == FinishReason::Length);
    let status = if incomplete.is_some() {
        "incomplete"
    } else {
        "completed"
    };
    let v = response_object(ctx, status, output, Some(&c.usage), incomplete);
    serde_json::to_vec(&v).expect("a JSON value serialises")
}

#[derive(Debug)]
enum Kind {
    Reasoning,
    Text,
    Call { call_id: String, name: String },
}

#[derive(Debug)]
struct Item {
    block: u32,
    kind: Kind,
    id: String,
    output_index: usize,
    text: String,
    done: bool,
    final_json: Option<Json>,
}

/// Renders a stream as Responses events.
#[derive(Debug)]
pub struct ResponsesRender {
    ctx: RenderCtx,
    seq: u64,
    started: bool,
    terminal: bool,
    items: Vec<Item>,
    usage: Usage,
    refused: bool,
}

impl ResponsesRender {
    /// A renderer for one response.
    #[must_use]
    pub fn new(ctx: RenderCtx) -> Self {
        Self {
            ctx,
            seq: 0,
            started: false,
            terminal: false,
            items: Vec::new(),
            usage: Usage::default(),
            refused: false,
        }
    }

    fn emit(&mut self, ty: &str, mut data: Json) -> Vec<u8> {
        data["type"] = json!(ty);
        data["sequence_number"] = json!(self.seq);
        self.seq += 1;
        format_event(Some(ty), &data.to_string())
    }

    fn ensure_started(&mut self, out: &mut Vec<u8>) {
        if self.started {
            return;
        }
        self.started = true;
        let resp = response_object(&self.ctx, "in_progress", Vec::new(), None, None);
        out.extend(self.emit("response.created", json!({"response": resp.clone()})));
        out.extend(self.emit("response.in_progress", json!({"response": resp})));
    }

    fn open(&mut self, block: u32, kind: Kind, out: &mut Vec<u8>) -> usize {
        if let Some(i) = self.items.iter().position(|it| it.block == block) {
            return i;
        }
        let output_index = self.items.len();
        let (id, added) = match &kind {
            Kind::Reasoning => {
                let id = item_id(&self.ctx, "rs", output_index);
                (
                    id.clone(),
                    json!({"type": "reasoning", "id": id, "summary": []}),
                )
            }
            Kind::Text => {
                let id = item_id(&self.ctx, "msg", output_index);
                (
                    id.clone(),
                    json!({"type": "message", "id": id, "status": "in_progress", "role": "assistant", "content": []}),
                )
            }
            Kind::Call { call_id, name } => {
                let id = item_id(&self.ctx, "fc", output_index);
                (id.clone(), call_item(&id, call_id, name, "", "in_progress"))
            }
        };
        out.extend(self.emit(
            "response.output_item.added",
            json!({"output_index": output_index, "item": added}),
        ));
        match &kind {
            Kind::Reasoning => out.extend(self.emit(
                "response.reasoning_summary_part.added",
                json!({"output_index": output_index, "item_id": id, "summary_index": 0, "part": {"type": "summary_text", "text": ""}}),
            )),
            Kind::Text => out.extend(self.emit(
                "response.content_part.added",
                json!({"output_index": output_index, "item_id": id, "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": []}}),
            )),
            Kind::Call { .. } => {}
        }
        self.items.push(Item {
            block,
            kind,
            id,
            output_index,
            text: String::new(),
            done: false,
            final_json: None,
        });
        self.items.len() - 1
    }

    fn close(&mut self, index: usize, opaque: Option<Vec<u8>>, out: &mut Vec<u8>) {
        if self.items[index].done {
            return;
        }
        self.items[index].done = true;
        let (id, oi) = (self.items[index].id.clone(), self.items[index].output_index);
        let text = std::mem::take(&mut self.items[index].text);
        let refused = self.refused;
        let item = match &self.items[index].kind {
            Kind::Reasoning => {
                let shown = if self.ctx.display == ReasoningDisplay::Omitted {
                    String::new()
                } else {
                    text.clone()
                };
                if self.ctx.display != ReasoningDisplay::Omitted {
                    out.extend(self.emit("response.reasoning_summary_text.done", json!({"output_index": oi, "item_id": id, "summary_index": 0, "text": shown})));
                    out.extend(self.emit(
                        "response.reasoning_summary_part.done",
                        json!({"output_index": oi, "item_id": id, "summary_index": 0, "part": {"type": "summary_text", "text": shown}}),
                    ));
                }
                reasoning_item(&self.ctx, &id, &text, opaque)
            }
            Kind::Text => {
                out.extend(self.emit(
                    "response.output_text.done",
                    json!({"output_index": oi, "item_id": id, "content_index": 0, "text": text}),
                ));
                let part = if refused {
                    json!({"type": "refusal", "refusal": text})
                } else {
                    json!({"type": "output_text", "text": text, "annotations": []})
                };
                out.extend(self.emit(
                    "response.content_part.done",
                    json!({"output_index": oi, "item_id": id, "content_index": 0, "part": part}),
                ));
                message_item(&id, &text, refused, "completed")
            }
            Kind::Call { call_id, name } => {
                let (call_id, name) = (call_id.clone(), name.clone());
                out.extend(self.emit(
                    "response.function_call_arguments.done",
                    json!({"output_index": oi, "item_id": id, "arguments": text}),
                ));
                call_item(&id, &call_id, &name, &text, "completed")
            }
        };
        out.extend(self.emit(
            "response.output_item.done",
            json!({"output_index": oi, "item": item.clone()}),
        ));
        self.items[index].final_json = Some(item);
    }

    fn fail(&mut self, kind: StreamErrorKind, message: &str) -> Vec<u8> {
        self.terminal = true;
        let code = match kind {
            StreamErrorKind::Truncated => "stream_truncated",
            StreamErrorKind::Upstream => "upstream_error",
            StreamErrorKind::Protocol => "upstream_protocol_error",
            StreamErrorKind::TooLarge => "request_too_large",
        };
        let mut out = self.emit(
            "error",
            json!({"code": code, "message": message, "param": null}),
        );
        let mut resp = response_object(&self.ctx, "failed", Vec::new(), None, None);
        resp["error"] = json!({"code": code, "message": message});
        out.extend(self.emit("response.failed", json!({"response": resp})));
        out
    }
}

impl StreamRender for ResponsesRender {
    fn push(&mut self, ev: &IrEvent) -> Vec<u8> {
        if self.terminal {
            return Vec::new();
        }
        let mut out = Vec::new();
        match ev {
            IrEvent::Start { .. } => self.ensure_started(&mut out),
            IrEvent::TextDelta { block, text } => {
                self.ensure_started(&mut out);
                let i = self.open(*block, Kind::Text, &mut out);
                self.items[i].text.push_str(text);
                let (id, oi) = (self.items[i].id.clone(), self.items[i].output_index);
                out.extend(self.emit(
                    "response.output_text.delta",
                    json!({"output_index": oi, "item_id": id, "content_index": 0, "delta": text}),
                ));
            }
            IrEvent::TextEnd { block } => {
                if let Some(i) = self.items.iter().position(|it| it.block == *block) {
                    self.close(i, None, &mut out);
                }
            }
            IrEvent::ReasoningDelta { block, text } => {
                if self.ctx.thinking_wanted {
                    self.ensure_started(&mut out);
                    let i = self.open(*block, Kind::Reasoning, &mut out);
                    self.items[i].text.push_str(text);
                    if self.ctx.display != ReasoningDisplay::Omitted {
                        let (id, oi) = (self.items[i].id.clone(), self.items[i].output_index);
                        out.extend(self.emit("response.reasoning_summary_text.delta", json!({"output_index": oi, "item_id": id, "summary_index": 0, "delta": text})));
                    }
                }
            }
            IrEvent::ReasoningSealed { block, opaque } => {
                if self.ctx.thinking_wanted {
                    self.ensure_started(&mut out);
                    let i = self.open(*block, Kind::Reasoning, &mut out);
                    self.close(i, opaque.clone(), &mut out);
                }
            }
            IrEvent::ToolCallStart { block, id, name } => {
                self.ensure_started(&mut out);
                self.open(
                    *block,
                    Kind::Call {
                        call_id: id.clone(),
                        name: name.clone(),
                    },
                    &mut out,
                );
            }
            IrEvent::ToolCallArgs { block, fragment } => {
                if let Some(i) = self.items.iter().position(|it| it.block == *block) {
                    self.items[i].text.push_str(fragment);
                    let (id, oi) = (self.items[i].id.clone(), self.items[i].output_index);
                    out.extend(self.emit(
                        "response.function_call_arguments.delta",
                        json!({"output_index": oi, "item_id": id, "delta": fragment}),
                    ));
                }
            }
            IrEvent::ToolCallEnd { block } => {
                if let Some(i) = self.items.iter().position(|it| it.block == *block) {
                    self.close(i, None, &mut out);
                }
            }
            IrEvent::Refusal { text } => {
                self.ensure_started(&mut out);
                self.refused = true;
                let i = self.open(u32::MAX, Kind::Text, &mut out);
                self.items[i].text.push_str(text);
                let (id, oi) = (self.items[i].id.clone(), self.items[i].output_index);
                out.extend(self.emit(
                    "response.refusal.delta",
                    json!({"output_index": oi, "item_id": id, "content_index": 0, "delta": text}),
                ));
            }
            IrEvent::Usage(u) => self.usage = *u,
            IrEvent::Finish(reason) => {
                self.ensure_started(&mut out);
                for i in 0..self.items.len() {
                    self.close(i, None, &mut out);
                }
                let output: Vec<Json> = self
                    .items
                    .iter()
                    .filter_map(|it| it.final_json.clone())
                    .collect();
                let incomplete = incomplete_reason(*reason)
                    .filter(|_| *reason == FinishReason::Length || self.refused);
                let (ty, status) = if incomplete.is_some() {
                    ("response.incomplete", "incomplete")
                } else {
                    ("response.completed", "completed")
                };
                let resp =
                    response_object(&self.ctx, status, output, Some(&self.usage), incomplete);
                out.extend(self.emit(ty, json!({"response": resp})));
                self.terminal = true;
            }
            IrEvent::Error(e) => out.extend(self.fail(e.kind, &e.message)),
        }
        out
    }

    fn finish(&mut self) -> Vec<u8> {
        if self.terminal {
            return Vec::new();
        }
        self.fail(
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
    use crate::ir::{Origin, Protocol};
    use crate::signature::{verify, SigningKey};
    use crate::sse::SseParser;
    use crate::stream::StreamError;

    fn ctx() -> RenderCtx {
        RenderCtx {
            model: "gpt-5".into(),
            message_id: "msg_abc".into(),
            signing_key: Some(SigningKey::from_token("tok")),
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

    fn render(events: &[IrEvent], ctx: RenderCtx) -> Vec<(String, Json)> {
        let mut r = ResponsesRender::new(ctx);
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

    #[test]
    fn usage_counts_cached_tokens_inside_input_tokens() {
        let u = usage_json(&Usage {
            input_tokens: 70,
            output_tokens: 20,
            cache_read_input_tokens: 30,
            reasoning_tokens: 5,
            ..Usage::default()
        });
        assert_eq!(
            (u["input_tokens"].as_u64(), u["total_tokens"].as_u64()),
            (Some(100), Some(120))
        );
        assert_eq!(u["input_tokens_details"]["cached_tokens"], 30);
        assert_eq!(u["output_tokens_details"]["reasoning_tokens"], 5);
    }

    #[test]
    fn a_text_stream_has_the_full_item_lifecycle() {
        let evs = render(
            &[
                IrEvent::Start {
                    id: None,
                    model: None,
                },
                IrEvent::TextDelta {
                    block: 0,
                    text: "Hel".into(),
                },
                IrEvent::TextDelta {
                    block: 0,
                    text: "lo".into(),
                },
                IrEvent::TextEnd { block: 0 },
                IrEvent::Usage(Usage {
                    input_tokens: 5,
                    output_tokens: 2,
                    ..Usage::default()
                }),
                IrEvent::Finish(FinishReason::Stop),
            ],
            ctx(),
        );
        assert_eq!(
            names(&evs),
            [
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed"
            ]
        );
        let seqs: Vec<u64> = evs
            .iter()
            .map(|(_, j)| j["sequence_number"].as_u64().unwrap())
            .collect();
        assert_eq!(
            seqs,
            (0..evs.len() as u64).collect::<Vec<_>>(),
            "sequence numbers are consecutive"
        );
        assert_eq!(evs[0].1["response"]["id"], "resp_abc");
        assert_eq!(evs[0].1["response"]["status"], "in_progress");
        assert_eq!(evs[4].1["delta"], "Hel");
        assert_eq!(evs[6].1["text"], "Hello");
        let done = &evs[9].1["response"];
        assert_eq!(done["status"], "completed");
        assert_eq!(done["output"][0]["content"][0]["text"], "Hello");
        assert_eq!(done["usage"]["input_tokens"], 5);
        assert_eq!(done["model"], "gpt-5");
    }

    #[test]
    fn reasoning_items_carry_a_signature_in_encrypted_content() {
        let evs = render(
            &[
                IrEvent::ReasoningDelta {
                    block: 0,
                    text: "Need ".into(),
                },
                IrEvent::ReasoningDelta {
                    block: 0,
                    text: "to look.".into(),
                },
                IrEvent::ReasoningSealed {
                    block: 0,
                    opaque: Some(b"UPSTREAM".to_vec()),
                },
                IrEvent::Finish(FinishReason::Stop),
            ],
            ctx(),
        );
        let item = evs
            .iter()
            .find(|(n, _)| n == "response.output_item.done")
            .unwrap()
            .1["item"]
            .clone();
        assert_eq!(item["type"], "reasoning");
        assert_eq!(
            item["summary"][0],
            json!({"type": "summary_text", "text": "Need to look."})
        );
        let sig = item["encrypted_content"].as_str().unwrap();
        let payload = verify(&SigningKey::from_token("tok"), sig, "Need to look.").unwrap();
        assert_eq!(payload.encrypted_content.as_deref(), Some(&b"UPSTREAM"[..]));
        assert!(names(&evs).contains(&"response.reasoning_summary_text.delta"));
        assert!(names(&evs).contains(&"response.reasoning_summary_part.done"));
    }

    #[test]
    fn reasoning_is_dropped_unless_asked_for_and_hidden_when_omitted() {
        let events = [
            IrEvent::ReasoningDelta {
                block: 0,
                text: "secret".into(),
            },
            IrEvent::ReasoningSealed {
                block: 0,
                opaque: None,
            },
            IrEvent::Finish(FinishReason::Stop),
        ];
        let mut off = ctx();
        off.thinking_wanted = false;
        let evs = render(&events, off);
        assert!(!names(&evs)
            .iter()
            .any(|n| n.contains("reasoning") || *n == "response.output_item.added"));
        let mut omitted = ctx();
        omitted.display = ReasoningDisplay::Omitted;
        let evs = render(&events, omitted);
        assert!(!names(&evs).contains(&"response.reasoning_summary_text.delta"));
        let item = &evs
            .iter()
            .find(|(n, _)| n == "response.output_item.done")
            .unwrap()
            .1["item"];
        assert_eq!(item["summary"], json!([]));
        let payload = verify(
            &SigningKey::from_token("tok"),
            item["encrypted_content"].as_str().unwrap(),
            "",
        )
        .unwrap();
        assert_eq!(payload.hidden_text.as_deref(), Some("secret"));
    }

    #[test]
    fn function_calls_stream_arguments_and_complete_with_them() {
        let evs = render(
            &[
                IrEvent::ToolCallStart {
                    block: 0,
                    id: "call_1".into(),
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
        let added = &evs
            .iter()
            .find(|(n, _)| n == "response.output_item.added")
            .unwrap()
            .1["item"];
        assert_eq!(
            (
                added["type"].as_str(),
                added["call_id"].as_str(),
                added["name"].as_str()
            ),
            (Some("function_call"), Some("call_1"), Some("read"))
        );
        let args: String = evs
            .iter()
            .filter(|(n, _)| n == "response.function_call_arguments.delta")
            .filter_map(|(_, j)| j["delta"].as_str())
            .collect();
        assert_eq!(args, "{\"p\":1}");
        let done = &evs
            .iter()
            .find(|(n, _)| n == "response.function_call_arguments.done")
            .unwrap()
            .1;
        assert_eq!(done["arguments"], "{\"p\":1}");
        let completed = &evs.last().unwrap().1["response"];
        assert_eq!(completed["output"][0]["arguments"], "{\"p\":1}");
        assert_eq!(completed["output"][0]["status"], "completed");
    }

    #[test]
    fn interleaved_items_keep_their_own_output_index() {
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
                    fragment: "{".into(),
                },
                IrEvent::ToolCallArgs {
                    block: 1,
                    fragment: "{}".into(),
                },
                IrEvent::ToolCallArgs {
                    block: 0,
                    fragment: "}".into(),
                },
                IrEvent::ToolCallEnd { block: 1 },
                IrEvent::ToolCallEnd { block: 0 },
                IrEvent::Finish(FinishReason::ToolCalls),
            ],
            ctx(),
        );
        let completed = &evs.last().unwrap().1["response"]["output"];
        assert_eq!(
            (
                completed[0]["call_id"].as_str(),
                completed[0]["arguments"].as_str()
            ),
            (Some("a"), Some("{}"))
        );
        assert_eq!(
            (
                completed[1]["call_id"].as_str(),
                completed[1]["arguments"].as_str()
            ),
            (Some("b"), Some("{}"))
        );
        let for_item = |idx: u64| -> String {
            evs.iter()
                .filter(|(n, j)| {
                    n == "response.function_call_arguments.delta" && j["output_index"] == idx
                })
                .filter_map(|(_, j)| j["delta"].as_str())
                .collect()
        };
        assert_eq!(for_item(0), "{}");
    }

    #[test]
    fn length_and_refusal_finish_as_incomplete_with_a_reason() {
        let evs = render(
            &[
                IrEvent::TextDelta {
                    block: 0,
                    text: "cut".into(),
                },
                IrEvent::TextEnd { block: 0 },
                IrEvent::Finish(FinishReason::Length),
            ],
            ctx(),
        );
        let last = evs.last().unwrap();
        assert_eq!(last.0, "response.incomplete");
        assert_eq!(
            last.1["response"]["incomplete_details"],
            json!({"reason": "max_output_tokens"})
        );
        let evs = render(
            &[
                IrEvent::Refusal { text: "No.".into() },
                IrEvent::Finish(FinishReason::Refusal),
            ],
            ctx(),
        );
        assert!(names(&evs).contains(&"response.refusal.delta"));
        let last = evs.last().unwrap();
        assert_eq!(
            last.1["response"]["output"][0]["content"][0],
            json!({"type": "refusal", "refusal": "No."})
        );
        assert_eq!(
            last.1["response"]["incomplete_details"],
            json!({"reason": "content_filter"})
        );
    }

    #[test]
    fn errors_and_truncation_end_with_error_and_failed_events() {
        let evs = render(
            &[
                IrEvent::TextDelta {
                    block: 0,
                    text: "x".into(),
                },
                IrEvent::Error(StreamError::truncated()),
            ],
            ctx(),
        );
        let n = names(&evs);
        assert_eq!(n[n.len() - 2..], ["error", "response.failed"]);
        assert_eq!(
            evs.last().unwrap().1["response"]["error"]["code"],
            "stream_truncated"
        );
        assert!(!n.contains(&"response.completed"));
        let evs = render(
            &[IrEvent::TextDelta {
                block: 0,
                text: "x".into(),
            }],
            ctx(),
        );
        assert_eq!(evs.last().unwrap().0, "response.failed");
        for (kind, code) in [
            (StreamErrorKind::Upstream, "upstream_error"),
            (StreamErrorKind::Protocol, "upstream_protocol_error"),
            (StreamErrorKind::TooLarge, "request_too_large"),
        ] {
            let evs = render(
                &[IrEvent::Error(StreamError {
                    kind,
                    message: "m".into(),
                })],
                ctx(),
            );
            assert_eq!(evs[0].1["code"], code);
        }
    }

    #[test]
    fn nothing_follows_the_end() {
        let mut r = ResponsesRender::new(ctx());
        assert!(!r.push(&IrEvent::Finish(FinishReason::Stop)).is_empty());
        assert!(
            r.is_done()
                && r.push(&IrEvent::TextDelta {
                    block: 0,
                    text: "late".into()
                })
                .is_empty()
                && r.finish().is_empty()
        );
        assert_eq!(r.keepalive(), b": keep-alive\n\n");
    }

    // ---- non-streaming ----

    fn completion(content: Vec<Part>, finish: FinishReason) -> IrCompletion {
        IrCompletion {
            content,
            finish,
            usage: Usage {
                input_tokens: 4,
                output_tokens: 6,
                ..Usage::default()
            },
            upstream_id: None,
            upstream_model: None,
        }
    }

    fn response(c: &IrCompletion, ctx: &RenderCtx) -> Json {
        serde_json::from_slice(&render_response(c, ctx)).unwrap()
    }

    #[test]
    fn renders_a_complete_response_with_every_item_type() {
        let c = completion(
            vec![
                Part::Reasoning {
                    text: "hm".into(),
                    display: ReasoningDisplay::Summarized,
                    origin: None,
                    opaque: Some(b"E".to_vec()),
                },
                Part::Text("answer".into()),
                Part::ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    input: json!({"p": 1}),
                },
            ],
            FinishReason::ToolCalls,
        );
        let r = response(&c, &ctx());
        assert_eq!(
            (
                r["object"].as_str(),
                r["status"].as_str(),
                r["id"].as_str(),
                r["model"].as_str()
            ),
            (
                Some("response"),
                Some("completed"),
                Some("resp_abc"),
                Some("gpt-5")
            )
        );
        assert_eq!(r["output"][0]["type"], "reasoning");
        let sig = r["output"][0]["encrypted_content"].as_str().unwrap();
        assert_eq!(
            verify(&SigningKey::from_token("tok"), sig, "hm")
                .unwrap()
                .encrypted_content
                .as_deref(),
            Some(&b"E"[..])
        );
        assert_eq!(r["output"][1]["content"][0]["text"], "answer");
        assert_eq!(
            (
                r["output"][2]["call_id"].as_str(),
                r["output"][2]["arguments"].as_str()
            ),
            (Some("c1"), Some("{\"p\":1}"))
        );
        assert_eq!(r["usage"]["input_tokens"], 4);
        let mut off = ctx();
        off.thinking_wanted = false;
        assert_eq!(response(&c, &off)["output"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn a_length_stop_is_incomplete_and_a_refusal_is_a_refusal_part() {
        let r = response(
            &completion(vec![Part::Text("cut".into())], FinishReason::Length),
            &ctx(),
        );
        assert_eq!(r["status"], "incomplete");
        assert_eq!(
            r["incomplete_details"],
            json!({"reason": "max_output_tokens"})
        );
        let r = response(
            &completion(vec![Part::Text("No".into())], FinishReason::Refusal),
            &ctx(),
        );
        assert_eq!(
            r["output"][0]["content"][0],
            json!({"type": "refusal", "refusal": "No"})
        );
    }
}
