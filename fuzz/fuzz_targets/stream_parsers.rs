//! Every upstream stream parser: arbitrary bytes in arbitrary chunks never panic, and the stream
//! ends in exactly one terminal event.
#![no_main]

use airmate_core::ir::{Origin, Protocol};
use airmate_core::outbound::{
    anthropic_parse::AnthropicStream, bedrock_parse::BedrockStream, chat_parse::ChatStream,
    gemini_parse::GeminiStream, responses_parse::ResponsesStream, ParseCtx,
};
use airmate_core::stream::{IrEvent, StreamParse};
use airmate_core::toolname::ToolNames;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let [which, step, rest @ ..] = data else { return };
    let protocol = [Protocol::Chat, Protocol::Responses, Protocol::Anthropic, Protocol::Gemini, Protocol::Bedrock][usize::from(*which) % 5];
    let ctx = ParseCtx {
        origin: Origin { upstream: "fuzz".into(), protocol },
        tool_names: ToolNames::new(),
        think_tags: *which & 0x80 != 0,
    };
    let mut parser: Box<dyn StreamParse> = match protocol {
        Protocol::Chat => Box::new(ChatStream::new(ctx)),
        Protocol::Responses => Box::new(ResponsesStream::new(ctx)),
        Protocol::Anthropic => Box::new(AnthropicStream::new(ctx)),
        Protocol::Gemini => Box::new(GeminiStream::new(ctx)),
        Protocol::Bedrock => Box::new(BedrockStream::new(ctx)),
    };
    let mut events = Vec::new();
    for chunk in rest.chunks(usize::from(*step).max(1)) {
        events.extend(parser.push(chunk));
    }
    events.extend(parser.finish());
    let terminals = events.iter().filter(|e| matches!(e, IrEvent::Finish(_) | IrEvent::Error(_))).count();
    assert_eq!(terminals, 1, "{protocol:?}: {events:?}");
});
