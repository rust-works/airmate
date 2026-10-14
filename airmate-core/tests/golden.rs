//! Golden streams: recorded upstream streams in `tests/fixtures/`, each paired with the exact
//! expected downstream Anthropic event sequence. `UPDATE_GOLDEN=1 cargo test` rewrites the
//! expectations; review the diff.

mod common;

use std::path::PathBuf;

use airmate_core::ir::Protocol;
use common::{assert_valid_anthropic, convert};

const CASES: &[(&str, Protocol)] = &[
    ("chat_text", Protocol::Chat),
    ("chat_thinking_then_text", Protocol::Chat),
    ("chat_inline_think", Protocol::Chat),
    ("chat_parallel_tools", Protocol::Chat),
    ("chat_refusal", Protocol::Chat),
    ("chat_truncated", Protocol::Chat),
    ("chat_midstream_error", Protocol::Chat),
    ("chat_trailing_usage", Protocol::Chat),
    ("responses_reasoning_tool", Protocol::Responses),
    ("gemini_text", Protocol::Gemini),
    ("gemini_thinking_tool", Protocol::Gemini),
    ("gemini_truncated", Protocol::Gemini),
    ("gemini_midstream_error", Protocol::Gemini),
    ("gemini_blocked", Protocol::Gemini),
];

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn upstream(name: &str) -> Vec<u8> {
    std::fs::read(dir().join(format!("{name}.sse"))).unwrap_or_else(|e| panic!("{name}.sse: {e}"))
}

#[test]
fn every_stream_matches_its_golden_output() {
    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    for (name, protocol) in CASES {
        let got = convert(&[&upstream(name)], *protocol);
        let path = dir().join(format!("{name}.expected.sse"));
        if update {
            std::fs::write(&path, &got).unwrap();
            continue;
        }
        let want = std::fs::read(&path).unwrap_or_else(|e| {
            panic!(
                "{}: {e}; run with UPDATE_GOLDEN=1 to create it",
                path.display()
            )
        });
        assert_eq!(
            String::from_utf8_lossy(&got),
            String::from_utf8_lossy(&want),
            "{name} differs from its golden file"
        );
    }
}

#[test]
fn every_golden_output_is_a_valid_anthropic_sequence() {
    for (name, protocol) in CASES {
        assert_valid_anthropic(&convert(&[&upstream(name)], *protocol));
    }
}

#[test]
fn splitting_every_stream_at_every_byte_offset_changes_nothing() {
    for (name, protocol) in CASES {
        let bytes = upstream(name);
        let whole = convert(&[&bytes], *protocol);
        for i in 0..=bytes.len() {
            let (a, b) = bytes.split_at(i);
            assert_eq!(
                convert(&[a, b], *protocol),
                whole,
                "{name} split at byte {i}"
            );
        }
    }
}

#[test]
fn byte_at_a_time_delivery_changes_nothing() {
    for (name, protocol) in CASES {
        let bytes = upstream(name);
        let whole = convert(&[&bytes], *protocol);
        let chunks: Vec<&[u8]> = bytes.chunks(1).collect();
        assert_eq!(convert(&chunks, *protocol), whole, "{name}");
    }
}

#[test]
fn truncated_and_failed_streams_never_end_in_message_stop() {
    for (name, protocol) in [
        ("chat_truncated", Protocol::Chat),
        ("chat_midstream_error", Protocol::Chat),
        ("gemini_truncated", Protocol::Gemini),
        ("gemini_midstream_error", Protocol::Gemini),
    ] {
        let out = String::from_utf8(convert(&[&upstream(name)], protocol)).unwrap();
        assert!(out.contains("event: error"), "{name}");
        assert!(!out.contains("message_stop"), "{name}");
    }
}
