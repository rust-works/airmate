//! The SSE parser: any bytes, in any split, with tight limits, must not panic or grow without bound.
#![no_main]

use airmate_core::sse::SseParser;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let (cut, rest) = data.split_first().map_or((0, data), |(c, r)| (usize::from(*c), r));
    let cut = cut.min(rest.len());
    let mut p = SseParser::with_limits(512, 2048);
    let _ = p.push(&rest[..cut]);
    let _ = p.push(&rest[cut..]);
    let _ = p.finish();
});
