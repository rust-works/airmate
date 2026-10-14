//! Shared helpers for the integration tests: the upstream-to-Anthropic pipeline and a validator
//! for the Anthropic event sequence.
#![allow(dead_code)]

use airmate_core::flavor::Flavor;
use airmate_core::ir::{Origin, Protocol, ReasoningDisplay};
use airmate_core::outbound::chat_parse::ChatStream;
use airmate_core::outbound::responses_parse::ResponsesStream;
use airmate_core::outbound::ParseCtx;
use airmate_core::signature::SigningKey;
use airmate_core::sse::{SseEvent, SseParser};
use airmate_core::stream::anthropic_render::{AnthropicRender, RenderCtx, DEFAULT_BUFFER_CAP};
use airmate_core::stream::{IrEvent, StreamParse};
use airmate_core::toolname::ToolNames;
use serde_json::Value as Json;

pub fn origin(protocol: Protocol) -> Origin {
    Origin {
        upstream: "fixture".into(),
        protocol,
    }
}

pub fn parser(protocol: Protocol) -> Box<dyn StreamParse> {
    let ctx = ParseCtx {
        origin: origin(protocol),
        tool_names: ToolNames::new(),
        think_tags: true,
    };
    match protocol {
        Protocol::Responses => Box::new(ResponsesStream::new(ctx)),
        _ => Box::new(ChatStream::new(ctx)),
    }
}

pub fn render_ctx(protocol: Protocol) -> RenderCtx {
    let _ = Flavor::Generic;
    RenderCtx {
        model: "claude-sonnet".into(),
        message_id: "msg_golden".into(),
        signing_key: Some(SigningKey::from_token("golden-token")),
        origin: origin(protocol),
        thinking_wanted: true,
        display: ReasoningDisplay::Summarized,
        est_input_tokens: 100,
        buffer_cap: DEFAULT_BUFFER_CAP,
    }
}

/// Upstream bytes (delivered in the given chunks) to the downstream Anthropic bytes.
pub fn convert(chunks: &[&[u8]], protocol: Protocol) -> Vec<u8> {
    let mut parse = parser(protocol);
    let mut render = AnthropicRender::new(render_ctx(protocol));
    let mut out = Vec::new();
    for chunk in chunks {
        for ev in parse.push(chunk) {
            out.extend(render.push(&ev));
        }
    }
    for ev in parse.finish() {
        out.extend(render.push(&ev));
    }
    out.extend(render.finish());
    out
}

/// Parses downstream bytes back into events.
pub fn events(bytes: &[u8]) -> Vec<SseEvent> {
    let mut p = SseParser::new();
    let mut evs = p.push(bytes).expect("valid SSE");
    evs.extend(p.finish().expect("valid SSE"));
    evs
}

/// Checks that `bytes` is a valid Anthropic event sequence: exactly one `message_start`, blocks
/// properly nested with ascending consecutive indices, every started block stopped before
/// `message_delta`, and exactly one terminal event (`message_stop` or `error`) at the end.
pub fn assert_valid_anthropic(bytes: &[u8]) {
    let evs = events(bytes);
    assert!(!evs.is_empty(), "no events");
    let name = |e: &SseEvent| e.event.clone().unwrap_or_default();
    let mut starts = 0;
    let mut open: Option<u64> = None;
    let mut next: u64 = 0;
    let mut finished = false;
    let mut terminal = false;
    for (i, e) in evs.iter().enumerate() {
        assert!(
            !terminal,
            "event {} ({}) after the terminal event",
            i,
            name(e)
        );
        let json: Json = serde_json::from_str(&e.data).expect("event data is JSON");
        assert_eq!(
            json["type"],
            name(e).as_str(),
            "event name matches payload type"
        );
        match name(e).as_str() {
            "message_start" => {
                assert_eq!(i, 0, "message_start must come first");
                starts += 1;
            }
            "ping" => {}
            "content_block_start" => {
                assert!(open.is_none(), "block started inside another block");
                let idx = json["index"].as_u64().unwrap();
                assert_eq!(idx, next, "indices ascend by one");
                open = Some(idx);
                next += 1;
            }
            "content_block_delta" => {
                assert_eq!(
                    open,
                    json["index"].as_u64(),
                    "delta for a block that is not open"
                );
            }
            "content_block_stop" => {
                assert_eq!(
                    open,
                    json["index"].as_u64(),
                    "stop for a block that is not open"
                );
                open = None;
            }
            "message_delta" => {
                assert!(open.is_none(), "message_delta with an open block");
                finished = true;
            }
            "message_stop" => {
                assert!(finished, "message_stop without message_delta");
                terminal = true;
            }
            "error" => terminal = true,
            other => panic!("unexpected event {other}"),
        }
    }
    assert!(terminal, "stream has no terminal event");
    assert!(starts <= 1);
    if evs.last().map(name).as_deref() == Some("message_stop") {
        assert_eq!(starts, 1);
    }
}

pub fn ir_events(chunks: &[&[u8]], protocol: Protocol) -> Vec<IrEvent> {
    let mut parse = parser(protocol);
    let mut out = Vec::new();
    for c in chunks {
        out.extend(parse.push(c));
    }
    out.extend(parse.finish());
    out
}
