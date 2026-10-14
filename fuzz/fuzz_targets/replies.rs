//! Non-streaming upstream replies, in every protocol: arbitrary bodies and statuses never panic.
#![no_main]

use airmate_core::adapters;
use airmate_core::ir::{Origin, Protocol};
use airmate_core::outbound::ParseCtx;
use airmate_core::toolname::ToolNames;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let [hi, lo, body @ ..] = data else { return };
    let status = 100 + (u16::from(*hi) << 8 | u16::from(*lo)) % 500;
    let ctx = ParseCtx {
        origin: Origin { upstream: "fuzz".into(), protocol: Protocol::Chat },
        tool_names: ToolNames::new(),
        think_tags: true,
    };
    for p in [Protocol::Chat, Protocol::Responses, Protocol::Anthropic, Protocol::Gemini, Protocol::Bedrock] {
        let _ = adapters::parse_response(p, status, body, &ctx);
    }
});
