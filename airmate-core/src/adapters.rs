//! Dispatch: pick the adapter for a protocol.
//!
//! The server is written against these functions, so adding a protocol is one arm here plus its
//! adapter, not a change to every handler.

use serde_json::Value as Json;

use crate::facts::Facts;
use crate::inbound::{self, InboundCtx};
use crate::ir::{Effort, IrCompletion, IrRequest, Protocol};
use crate::outbound::anthropic::{estimate_value, has_image};
use crate::outbound::{
    anthropic as passthrough, anthropic_build, anthropic_parse, chat, chat_parse, responses,
    responses_parse, ParseCtx, Target, UpstreamError, UpstreamRequest,
};
use crate::outbound::{gemini, gemini_parse};
use crate::policy::{Parsed, PolicyError};
use crate::stream::anthropic_render::{render_message, AnthropicRender, RenderCtx};
use crate::stream::chat_render::{render_completion, ChatRender};
use crate::stream::responses_render::{render_response, ResponsesRender};
use crate::stream::{StreamParse, StreamRender};

/// Parses a client's request body in `inbound`'s protocol.
///
/// # Errors
///
/// A [`PolicyError`] naming the field.
pub fn parse_request(
    inbound: Protocol,
    body: &[u8],
    ctx: &InboundCtx<'_>,
) -> Result<Parsed<IrRequest>, PolicyError> {
    match inbound {
        Protocol::Anthropic => inbound::anthropic::parse_request(body, ctx),
        Protocol::Chat => inbound::chat::parse_request(body, ctx),
        Protocol::Responses => inbound::responses::parse_request(body, ctx),
        other => Err(PolicyError {
            field: "protocol".to_owned(),
            message: format!("`{}` is not served as an inbound protocol", other.as_str()),
        }),
    }
}

fn lenient(body: &[u8]) -> Result<Json, PolicyError> {
    Ok(Json::Object(passthrough::parse(body)?))
}

fn str_at<'a>(v: &'a Json, pointer: &str) -> Option<&'a str> {
    v.pointer(pointer).and_then(Json::as_str)
}

/// Reads only what routing needs from a request body, without judging the rest.
///
/// # Errors
///
/// The body is not a JSON object with a `model`.
pub fn peek(inbound: Protocol, body: &[u8]) -> Result<Facts, PolicyError> {
    match inbound {
        Protocol::Anthropic => passthrough::peek(body),
        Protocol::Chat | Protocol::Responses => {
            let v = lenient(body)?;
            let model = v
                .get("model")
                .and_then(Json::as_str)
                .ok_or_else(|| passthrough::bad("model", "`model` is required"))?
                .to_owned();
            let (messages, limit_key) = if inbound == Protocol::Chat {
                ("messages", "max_completion_tokens")
            } else {
                ("input", "max_output_tokens")
            };
            let tools = v
                .get("tools")
                .and_then(Json::as_array)
                .is_some_and(|t| !t.is_empty());
            let effort = str_at(&v, "/reasoning_effort")
                .or_else(|| str_at(&v, "/reasoning/effort"))
                .and_then(|e| match e {
                    "minimal" => Some(Effort::Low),
                    other => Effort::parse(other),
                });
            let thinking = matches!(str_at(&v, "/thinking/type"), Some("enabled" | "adaptive"))
                || v.get("reasoning").is_some_and(|r| r.is_object());
            let structured = !matches!(
                str_at(&v, "/response_format/type").or_else(|| str_at(&v, "/text/format/type")),
                None | Some("text")
            );
            let est = v.get(messages).map_or(0, estimate_value)
                + v.get("instructions").map_or(0, estimate_value)
                + v.get("tools").map_or(0, estimate_value);
            let max = v
                .get(limit_key)
                .or_else(|| v.get("max_tokens"))
                .and_then(Json::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0);
            Ok(Facts {
                inbound,
                model,
                stream: v.get("stream").and_then(Json::as_bool).unwrap_or(false),
                tools,
                images: v.get(messages).is_some_and(has_image)
                    || v.get(messages).is_some_and(has_input_image),
                thinking,
                structured_output: structured,
                effort,
                est_input_tokens: est,
                max_tokens: max,
            })
        }
        other => Err(PolicyError {
            field: "protocol".to_owned(),
            message: format!("`{}` is not served as an inbound protocol", other.as_str()),
        }),
    }
}

fn has_input_image(v: &Json) -> bool {
    match v {
        Json::Array(a) => a.iter().any(has_input_image),
        Json::Object(o) => {
            matches!(
                o.get("type").and_then(Json::as_str),
                Some("image_url" | "input_image")
            ) || o.values().any(has_input_image)
        }
        _ => false,
    }
}

/// Builds the upstream request for `target`, translating from the IR.
///
/// # Errors
///
/// A [`PolicyError`] for something the target cannot take.
pub fn build_request(ir: &IrRequest, target: &Target) -> Result<UpstreamRequest, PolicyError> {
    match target.protocol {
        Protocol::Chat => chat::build(ir, target),
        Protocol::Responses => responses::build(ir, target),
        Protocol::Anthropic => anthropic_build::build(ir, target),
        Protocol::Gemini => gemini::build(ir, target),
        other => Err(PolicyError {
            field: "upstream.protocol".to_owned(),
            message: format!(
                "upstream protocol `{}` is not supported for this request",
                other.as_str()
            ),
        }),
    }
}

/// Reads a non-streaming reply from an upstream speaking `protocol`.
///
/// # Errors
///
/// An [`UpstreamError`] for an error status or an unreadable reply.
pub fn parse_response(
    protocol: Protocol,
    status: u16,
    body: &[u8],
    ctx: &ParseCtx,
) -> Result<IrCompletion, UpstreamError> {
    match protocol {
        Protocol::Chat => chat_parse::parse_response(status, body, ctx),
        Protocol::Responses => responses_parse::parse_response(status, body, ctx),
        Protocol::Anthropic => anthropic_parse::parse_response(status, body, ctx),
        Protocol::Gemini => gemini_parse::parse_response(status, body, ctx),
        other => Err(UpstreamError::unreadable(format!(
            "upstream protocol `{}` is not supported",
            other.as_str()
        ))),
    }
}

/// A stream parser for an upstream speaking `protocol`.
#[must_use]
pub fn stream_parser(protocol: Protocol, ctx: ParseCtx) -> Box<dyn StreamParse> {
    match protocol {
        Protocol::Responses => Box::new(responses_parse::ResponsesStream::new(ctx)),
        Protocol::Anthropic => Box::new(anthropic_parse::AnthropicStream::new(ctx)),
        Protocol::Gemini => Box::new(gemini_parse::GeminiStream::new(ctx)),
        _ => Box::new(chat_parse::ChatStream::new(ctx)),
    }
}

/// Renders a complete response for a client speaking `inbound`.
#[must_use]
pub fn render_response_for(inbound: Protocol, c: &IrCompletion, ctx: &RenderCtx) -> Vec<u8> {
    match inbound {
        Protocol::Chat => render_completion(c, ctx),
        Protocol::Responses => render_response(c, ctx),
        _ => render_message(c, ctx),
    }
}

/// A stream renderer for a client speaking `inbound`.
#[must_use]
pub fn stream_renderer(inbound: Protocol, ctx: RenderCtx) -> Box<dyn StreamRender> {
    match inbound {
        Protocol::Chat => Box::new(ChatRender::new(ctx)),
        Protocol::Responses => Box::new(ResponsesRender::new(ctx)),
        _ => Box::new(AnthropicRender::new(ctx)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flavor::Flavor;
    use crate::ir::{FinishReason, Part, Usage};
    use serde_json::json;

    fn b(v: &Json) -> Vec<u8> {
        v.to_string().into_bytes()
    }

    #[test]
    fn peek_reads_chat_routing_facts() {
        let f = peek(
            Protocol::Chat,
            &b(&json!({"model": "gpt-4.1", "stream": true, "max_completion_tokens": 99, "reasoning_effort": "high",
                "tools": [{"type": "function", "function": {"name": "t"}}], "response_format": {"type": "json_object"},
                "messages": [{"role": "user", "content": [{"type": "text", "text": "12345678"}, {"type": "image_url", "image_url": {"url": "u"}}]}]})),
        )
        .unwrap();
        assert_eq!(f.model, "gpt-4.1");
        assert!(f.stream && f.tools && f.images && f.structured_output);
        assert_eq!((f.max_tokens, f.effort), (99, Some(Effort::High)));
        assert!(f.est_input_tokens > 0);
        assert_eq!(f.inbound, Protocol::Chat);
        assert!(!f.thinking);
    }

    #[test]
    fn peek_reads_responses_routing_facts() {
        let f = peek(
            Protocol::Responses,
            &b(&json!({"model": "gpt-5", "max_output_tokens": 7, "reasoning": {"effort": "minimal"}, "text": {"format": {"type": "json_schema"}},
                "input": [{"role": "user", "content": [{"type": "input_image", "image_url": "u"}]}], "instructions": "be brief"})),
        )
        .unwrap();
        assert!(f.images && f.thinking && f.structured_output);
        assert_eq!((f.max_tokens, f.effort), (7, Some(Effort::Low)));
        assert!(f.est_input_tokens >= 2);
    }

    #[test]
    fn peek_reads_the_anthropic_facts_and_rejects_bad_bodies() {
        assert_eq!(
            peek(Protocol::Anthropic, br#"{"model":"claude-x"}"#)
                .unwrap()
                .model,
            "claude-x"
        );
        for p in [Protocol::Chat, Protocol::Responses, Protocol::Anthropic] {
            assert!(
                peek(p, b"{}").is_err() && peek(p, b"[]").is_err() && peek(p, b"x").is_err(),
                "{p:?}"
            );
        }
        assert!(peek(Protocol::Gemini, b"{}").is_err());
        let plain = peek(Protocol::Chat, br#"{"model":"m","messages":[]}"#).unwrap();
        assert!(
            !plain.stream
                && !plain.tools
                && !plain.images
                && !plain.thinking
                && !plain.structured_output
        );
    }

    #[test]
    fn parsing_dispatches_by_inbound_protocol() {
        let ctx = InboundCtx::default();
        assert!(parse_request(
            Protocol::Anthropic,
            br#"{"model":"m","max_tokens":1,"messages":[{"role":"user","content":"x"}]}"#,
            &ctx
        )
        .is_ok());
        assert!(parse_request(
            Protocol::Chat,
            br#"{"model":"m","messages":[{"role":"user","content":"x"}]}"#,
            &ctx
        )
        .is_ok());
        assert!(parse_request(Protocol::Responses, br#"{"model":"m","input":"x"}"#, &ctx).is_ok());
        assert!(parse_request(Protocol::Bedrock, b"{}", &ctx).is_err());
    }

    #[test]
    fn building_dispatches_by_target_protocol() {
        let mut ir = IrRequest::new("m", Protocol::Chat);
        ir.turns
            .push(crate::ir::Turn::User(vec![Part::Text("x".into())]));
        for (p, path) in [
            (Protocol::Chat, "/v1/chat/completions"),
            (Protocol::Responses, "/v1/responses"),
            (Protocol::Anthropic, "/v1/messages"),
            (Protocol::Gemini, "/v1beta/models/m:generateContent"),
        ] {
            assert_eq!(
                build_request(&ir, &Target::new("u", p, Flavor::Generic, "m"))
                    .unwrap()
                    .path,
                path
            );
        }
        assert!(build_request(
            &ir,
            &Target::new("u", Protocol::Bedrock, Flavor::Generic, "m")
        )
        .is_err());
    }

    #[test]
    fn rendering_dispatches_by_inbound_protocol() {
        let c = IrCompletion {
            content: vec![Part::Text("hi".into())],
            finish: FinishReason::Stop,
            usage: Usage::default(),
            upstream_id: None,
            upstream_model: None,
        };
        let ctx = RenderCtx {
            model: "m".into(),
            message_id: "msg_x".into(),
            signing_key: None,
            origin: crate::ir::Origin {
                upstream: "u".into(),
                protocol: Protocol::Chat,
            },
            thinking_wanted: false,
            display: crate::ir::ReasoningDisplay::Summarized,
            est_input_tokens: 0,
            buffer_cap: 1024,
            created: 1,
            include_usage: false,
        };
        let obj = |p| serde_json::from_slice::<Json>(&render_response_for(p, &c, &ctx)).unwrap();
        assert_eq!(obj(Protocol::Anthropic)["type"], "message");
        assert_eq!(obj(Protocol::Chat)["object"], "chat.completion");
        assert_eq!(obj(Protocol::Responses)["object"], "response");
        for p in [Protocol::Anthropic, Protocol::Chat, Protocol::Responses] {
            let mut r = stream_renderer(p, ctx.clone());
            assert!(!r
                .push(&crate::stream::IrEvent::Finish(FinishReason::Stop))
                .is_empty());
            assert!(r.is_done());
        }
    }

    #[test]
    fn parsers_dispatch_by_upstream_protocol() {
        let ctx = ParseCtx {
            origin: crate::ir::Origin {
                upstream: "u".into(),
                protocol: Protocol::Chat,
            },
            tool_names: crate::toolname::ToolNames::new(),
            think_tags: true,
        };
        let chat = br#"{"choices":[{"message":{"content":"a"},"finish_reason":"stop"}]}"#;
        let resp = br#"{"output":[{"type":"message","content":[{"type":"output_text","text":"a"}]}],"status":"completed"}"#;
        let ant = br#"{"content":[{"type":"text","text":"a"}],"stop_reason":"end_turn"}"#;
        for (p, body) in [
            (Protocol::Chat, &chat[..]),
            (Protocol::Responses, &resp[..]),
            (Protocol::Anthropic, &ant[..]),
        ] {
            assert_eq!(
                parse_response(p, 200, body, &ctx).unwrap().content,
                vec![Part::Text("a".into())],
                "{p:?}"
            );
        }
        let gem = br#"{"candidates":[{"content":{"parts":[{"text":"a"}]},"finishReason":"STOP"}]}"#;
        assert_eq!(
            parse_response(Protocol::Gemini, 200, gem, &ctx)
                .unwrap()
                .content,
            vec![Part::Text("a".into())]
        );
        assert!(parse_response(Protocol::Bedrock, 200, b"{}", &ctx).is_err());
        for p in [
            Protocol::Chat,
            Protocol::Responses,
            Protocol::Anthropic,
            Protocol::Gemini,
        ] {
            let mut sp = stream_parser(p, ctx.clone());
            assert!(!sp.finish().is_empty());
        }
    }
}
