//! The inbound request parsers and the routing peek: arbitrary bodies never panic.
#![no_main]

use airmate_core::adapters;
use airmate_core::inbound::gemini::Call;
use airmate_core::inbound::InboundCtx;
use airmate_core::ir::Protocol;
use airmate_core::policy::{PolicyConfig, UnknownFields};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((first, body)) = data.split_first() else { return };
    let ctx = InboundCtx {
        policy: PolicyConfig {
            unknown_fields: if first & 0x08 != 0 { UnknownFields::Warn } else { UnknownFields::Reject },
        },
        signing_key: None,
        count_tokens: first & 0x10 != 0,
    };
    match first & 0x07 {
        0 => {
            let _ = adapters::parse_request(Protocol::Anthropic, body, &ctx);
            let _ = adapters::peek(Protocol::Anthropic, body);
        }
        1 => {
            let _ = adapters::parse_request(Protocol::Chat, body, &ctx);
            let _ = adapters::peek(Protocol::Chat, body);
        }
        2 => {
            let _ = adapters::parse_request(Protocol::Responses, body, &ctx);
            let _ = adapters::peek(Protocol::Responses, body);
        }
        _ => {
            let call = Call { model: "gemini-2.5-pro", stream: first & 0x20 != 0 };
            let _ = adapters::parse_gemini_request(body, &ctx, &call);
            let _ = adapters::peek_gemini(body, &call);
        }
    }
});
