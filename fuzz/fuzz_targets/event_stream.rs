//! The AWS event stream decoder: arbitrary bytes never panic and the buffer stays bounded.
#![no_main]

use airmate_core::eventstream::Decoder;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let (step, rest) = data.split_first().map_or((1, data), |(s, r)| (usize::from(*s).max(1), r));
    let mut d = Decoder::new();
    for chunk in rest.chunks(step) {
        let _ = d.push(chunk);
    }
});
