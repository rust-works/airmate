//! Robustness: whatever bytes arrive, nothing panics, and every stream ends in exactly one
//! terminal event. These are the same entry points the `cargo fuzz` targets in `fuzz/` drive for
//! longer; here proptest drives them on every CI run.

use airmate_core::adapters;
use airmate_core::eventstream;
use airmate_core::inbound::gemini::Call;
use airmate_core::inbound::InboundCtx;
use airmate_core::ir::{Origin, Protocol};
use airmate_core::outbound::{
    anthropic_parse::AnthropicStream, bedrock_parse::BedrockStream, chat_parse::ChatStream,
    gemini_parse::GeminiStream, responses_parse::ResponsesStream, ParseCtx,
};
use airmate_core::policy::PolicyConfig;
use airmate_core::sse::SseParser;
use airmate_core::stream::{IrEvent, StreamParse};
use airmate_core::toolname::ToolNames;
use proptest::prelude::*;
use serde_json::{json, Value};

fn ctx(protocol: Protocol) -> ParseCtx {
    ParseCtx {
        origin: Origin {
            upstream: "fuzz".into(),
            protocol,
        },
        tool_names: ToolNames::new(),
        think_tags: true,
    }
}

fn parsers() -> Vec<(&'static str, Box<dyn StreamParse>)> {
    vec![
        ("chat", Box::new(ChatStream::new(ctx(Protocol::Chat)))),
        (
            "responses",
            Box::new(ResponsesStream::new(ctx(Protocol::Responses))),
        ),
        (
            "anthropic",
            Box::new(AnthropicStream::new(ctx(Protocol::Anthropic))),
        ),
        ("gemini", Box::new(GeminiStream::new(ctx(Protocol::Gemini)))),
        (
            "bedrock",
            Box::new(BedrockStream::new(ctx(Protocol::Bedrock))),
        ),
    ]
}

/// Arbitrary JSON, nested a few levels.
fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| json!(n)),
        any::<f64>()
            .prop_filter("finite", |f| f.is_finite())
            .prop_map(|f| json!(f)),
        "[ -~]{0,16}".prop_map(Value::String),
    ];
    leaf.prop_recursive(4, 48, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            prop::collection::btree_map("[a-zA-Z_]{1,14}", inner, 0..6)
                .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

/// A JSON object whose keys are drawn from fields the real protocols use, so the parsers' deeper
/// branches are reached instead of stopping at the first unknown key.
fn plausible_body() -> impl Strategy<Value = Value> {
    let keys = prop::sample::select(vec![
        "model",
        "messages",
        "contents",
        "input",
        "system",
        "systemInstruction",
        "tools",
        "tool_choice",
        "toolConfig",
        "generationConfig",
        "max_tokens",
        "max_completion_tokens",
        "stream",
        "thinking",
        "reasoning",
        "reasoning_effort",
        "temperature",
        "stop",
        "stop_sequences",
        "response_format",
        "output_config",
        "metadata",
        "role",
        "content",
        "parts",
        "text",
        "type",
        "name",
        "arguments",
        "inlineData",
        "functionCall",
        "functionResponse",
        "tool_use_id",
        "input_schema",
        "parameters",
    ]);
    prop::collection::vec((keys, json_value()), 0..10)
        .prop_map(|kv| Value::Object(kv.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn the_sse_parser_never_panics_and_respects_its_limits(
        bytes in prop::collection::vec(any::<u8>(), 0..2048),
        cut in 0usize..2048,
    ) {
        let mut p = SseParser::with_limits(256, 1024);
        let cut = cut.min(bytes.len());
        let _ = p.push(&bytes[..cut]);
        let _ = p.push(&bytes[cut..]);
        let _ = p.finish();
    }

    #[test]
    fn every_stream_parser_survives_arbitrary_bytes_and_ends_with_one_terminal_event(
        bytes in prop::collection::vec(any::<u8>(), 0..2048),
        step in 1usize..64,
    ) {
        for (name, mut parser) in parsers() {
            let mut events: Vec<IrEvent> = Vec::new();
            for chunk in bytes.chunks(step) {
                events.extend(parser.push(chunk));
            }
            events.extend(parser.finish());
            let terminals = events
                .iter()
                .filter(|e| matches!(e, IrEvent::Finish(_) | IrEvent::Error(_)))
                .count();
            prop_assert_eq!(terminals, 1, "{} produced {} terminal events: {:?}", name, terminals, events);
            prop_assert!(
                matches!(events.last(), Some(IrEvent::Finish(_) | IrEvent::Error(_))),
                "{} did not end on its terminal event: {:?}", name, events.last()
            );
        }
    }

    #[test]
    fn the_event_stream_decoder_never_panics_and_never_buffers_past_its_cap(
        bytes in prop::collection::vec(any::<u8>(), 0..4096),
        step in 1usize..128,
    ) {
        let mut d = eventstream::Decoder::new();
        for chunk in bytes.chunks(step) {
            let _ = d.push(chunk);
        }
        prop_assert!(!d.has_partial() || bytes.len() < eventstream::MAX_MESSAGE);
    }

    #[test]
    fn inbound_parsers_never_panic_on_json_shaped_input(body in plausible_body()) {
        let bytes = serde_json::to_vec(&body).unwrap();
        let ctx = InboundCtx { policy: PolicyConfig::default(), signing_key: None, count_tokens: false };
        for p in [Protocol::Anthropic, Protocol::Chat, Protocol::Responses] {
            let _ = adapters::parse_request(p, &bytes, &ctx);
            let _ = adapters::peek(p, &bytes);
        }
        let call = Call { model: "gemini-2.5-pro", stream: false };
        let _ = adapters::parse_gemini_request(&bytes, &ctx, &call);
        let _ = adapters::peek_gemini(&bytes, &call);
    }

    #[test]
    fn inbound_parsers_never_panic_on_arbitrary_json_or_bytes(body in json_value(), raw in prop::collection::vec(any::<u8>(), 0..256)) {
        let ctx = InboundCtx::default();
        let call = Call { model: "m", stream: true };
        for bytes in [serde_json::to_vec(&body).unwrap(), raw] {
            for p in [Protocol::Anthropic, Protocol::Chat, Protocol::Responses] {
                let _ = adapters::parse_request(p, &bytes, &ctx);
                let _ = adapters::peek(p, &bytes);
            }
            let _ = adapters::parse_gemini_request(&bytes, &ctx, &call);
        }
    }

    #[test]
    fn replies_never_panic_the_non_streaming_parsers(status in 100u16..600, body in prop::collection::vec(any::<u8>(), 0..512)) {
        let c = ctx(Protocol::Chat);
        for p in [Protocol::Chat, Protocol::Responses, Protocol::Anthropic, Protocol::Gemini, Protocol::Bedrock] {
            let _ = adapters::parse_response(p, status, &body, &c);
        }
    }

    #[test]
    fn mutating_a_valid_request_one_byte_at_a_time_never_panics(at in 0usize..400, byte in any::<u8>()) {
        let valid = json!({"model": "claude-x", "max_tokens": 64, "system": "s", "stream": true,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]},
                         {"role": "assistant", "content": [{"type": "tool_use", "id": "t", "name": "n", "input": {"a": 1}}]},
                         {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": "ok"}]}],
            "tools": [{"name": "n", "input_schema": {"type": "object"}}], "thinking": {"type": "enabled", "budget_tokens": 2048}});
        let mut bytes = serde_json::to_vec(&valid).unwrap();
        let i = at % bytes.len();
        bytes[i] = byte;
        let ctx = InboundCtx::default();
        let _ = adapters::parse_request(Protocol::Anthropic, &bytes, &ctx);
        let _ = adapters::peek(Protocol::Anthropic, &bytes);
    }
}
