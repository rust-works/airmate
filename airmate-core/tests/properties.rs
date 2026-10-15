//! Property tests on the sequencer and the whole upstream-to-Anthropic pipeline: for arbitrary
//! interleavings and chunk boundaries of valid upstream events, the output is always a valid
//! Anthropic sequence, and equals the sequential case when the input is sequential.

mod common;

use airmate_core::ir::{FinishReason, Protocol};
use airmate_core::stream::sequencer::{Seq, Sequencer};
use airmate_core::stream::IrEvent;
use common::{assert_valid_anthropic, convert, events};
use proptest::prelude::*;
use serde_json::{json, Value as Json};

/// A tool call: its arguments, cut into fragments.
#[derive(Debug, Clone)]
struct Call {
    fragments: Vec<String>,
}

fn call_strategy() -> impl Strategy<Value = Call> {
    // Arguments are a JSON object with a string value; cut at arbitrary character boundaries.
    ("[a-z]{0,12}", prop::collection::vec(1usize..6, 1..6)).prop_map(|(value, cuts)| {
        let full = format!("{{\"v\":\"{value}\"}}");
        let chars: Vec<char> = full.chars().collect();
        let mut fragments = Vec::new();
        let mut at = 0;
        for c in cuts {
            if at >= chars.len() {
                break;
            }
            let end = (at + c).min(chars.len());
            fragments.push(chars[at..end].iter().collect());
            at = end;
        }
        if at < chars.len() {
            fragments.push(chars[at..].iter().collect());
        }
        Call { fragments }
    })
}

/// Interleaves the fragments of several calls, preserving each call's own order, driven by `picks`.
fn interleave(calls: &[Call], picks: &[usize]) -> Vec<IrEvent> {
    let mut events: Vec<IrEvent> = calls
        .iter()
        .enumerate()
        .map(|(i, _)| IrEvent::ToolCallStart {
            block: i as u32,
            id: format!("call_{i}"),
            name: format!("tool_{i}"),
        })
        .collect();
    let mut next = vec![0usize; calls.len()];
    let mut pick = picks.iter().cycle();
    let mut remaining: usize = calls.iter().map(|c| c.fragments.len()).sum();
    while remaining > 0 {
        let start = pick.next().copied().unwrap_or(0) % calls.len();
        // Choose the next call at or after `start` that still has fragments.
        let i = (0..calls.len())
            .map(|d| (start + d) % calls.len())
            .find(|&i| next[i] < calls[i].fragments.len())
            .unwrap();
        events.push(IrEvent::ToolCallArgs {
            block: i as u32,
            fragment: calls[i].fragments[next[i]].clone(),
        });
        next[i] += 1;
        remaining -= 1;
        if next[i] == calls[i].fragments.len() {
            events.push(IrEvent::ToolCallEnd { block: i as u32 });
        }
    }
    events
}

fn sequential(calls: &[Call]) -> Vec<IrEvent> {
    let mut events = Vec::new();
    for (i, c) in calls.iter().enumerate() {
        events.push(IrEvent::ToolCallStart {
            block: i as u32,
            id: format!("call_{i}"),
            name: format!("tool_{i}"),
        });
        for f in &c.fragments {
            events.push(IrEvent::ToolCallArgs {
                block: i as u32,
                fragment: f.clone(),
            });
        }
        events.push(IrEvent::ToolCallEnd { block: i as u32 });
    }
    events
}

fn run_sequencer(events: &[IrEvent]) -> Vec<Seq> {
    let mut s = Sequencer::new(1 << 24);
    let mut out = Vec::new();
    for e in events {
        out.extend(s.push(e).unwrap());
    }
    out.extend(s.finish());
    out
}

fn args_by_index(seqs: &[Seq]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in seqs {
        if let Seq::Delta { index, text } = s {
            while out.len() <= *index as usize {
                out.push(String::new());
            }
            out[*index as usize].push_str(text);
        }
    }
    out
}

proptest! {
    #[test]
    fn interleaved_calls_are_valid_complete_and_sequential(
        calls in prop::collection::vec(call_strategy(), 1..5),
        picks in prop::collection::vec(0usize..8, 1..20),
    ) {
        let seqs = run_sequencer(&interleave(&calls, &picks));
        // Every call's arguments arrive whole, in the block that holds its start.
        let got = args_by_index(&seqs);
        let want: Vec<String> = calls.iter().map(|c| c.fragments.concat()).collect();
        prop_assert_eq!(got, want);
        // Blocks are strictly nested and indices ascend by one.
        let mut open: Option<u32> = None;
        let mut next = 0u32;
        for s in &seqs {
            match s {
                Seq::Start { index, .. } => { prop_assert!(open.is_none()); prop_assert_eq!(*index, next); open = Some(*index); next += 1; }
                Seq::Delta { index, .. } => prop_assert_eq!(open, Some(*index)),
                Seq::Stop { index, .. } => { prop_assert_eq!(open, Some(*index)); open = None; }
            }
        }
        prop_assert!(open.is_none());
        prop_assert_eq!(next as usize, calls.len());
    }

    #[test]
    fn sequential_input_equals_the_sequential_case_and_is_live(calls in prop::collection::vec(call_strategy(), 1..5)) {
        let events = sequential(&calls);
        let mut s = Sequencer::new(1 << 24);
        for e in &events {
            let out = s.push(e).unwrap();
            // Live: a delta is emitted by the very push that delivered it.
            if let IrEvent::ToolCallArgs { fragment, .. } = e {
                let emitted: String = out.iter().filter_map(|s| if let Seq::Delta { text, .. } = s { Some(text.as_str()) } else { None }).collect();
                prop_assert_eq!(&emitted, fragment);
            }
        }
        // An all-at-once interleaving with the same order gives identical output.
        let picks = vec![0usize];
        let single_call_order: Vec<Seq> = run_sequencer(&interleave(&calls, &picks));
        prop_assert_eq!(run_sequencer(&events), single_call_order);
    }

    #[test]
    fn the_whole_pipeline_is_valid_at_any_chunking(
        calls in prop::collection::vec(call_strategy(), 1..4),
        text in "[a-zA-Z ]{1,30}",
        size in 1usize..40,
    ) {
        // Build a Chat stream from the calls and some text, deliver it in chunks of `size`.
        let mut wire = String::new();
        let chunk = |delta: Json, finish: Option<&str>| -> String {
            format!("data: {}\n\n", json!({"id":"p","model":"m","choices":[{"index":0,"delta":delta,"finish_reason":finish}]}))
        };
        wire.push_str(&chunk(json!({"content": text}), None));
        for (i, c) in calls.iter().enumerate() {
            wire.push_str(&chunk(json!({"tool_calls":[{"index":i,"id":format!("c{i}"),"function":{"name":format!("t{i}"),"arguments":""}}]}), None));
            for f in &c.fragments {
                wire.push_str(&chunk(json!({"tool_calls":[{"index":i,"function":{"arguments":f}}]}), None));
            }
        }
        wire.push_str(&chunk(json!({}), Some("tool_calls")));
        wire.push_str("data: [DONE]\n\n");
        let whole = convert(&[wire.as_bytes()], Protocol::Chat);
        let chunks: Vec<&[u8]> = wire.as_bytes().chunks(size).collect();
        let split = convert(&chunks, Protocol::Chat);
        assert_valid_anthropic(&split);
        prop_assert_eq!(&split, &whole);
        let evs = events(&split);
        prop_assert_eq!(evs.iter().filter(|e| e.event.as_deref() == Some("message_start")).count(), 1);
        prop_assert_eq!(evs.last().and_then(|e| e.event.as_deref()), Some("message_stop"));
    }

    #[test]
    fn a_stream_cut_anywhere_never_ends_in_message_stop(cut in 0usize..400) {
        let wire = concat!(
            "data: {\"id\":\"p\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello world\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"p\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let cut = cut.min(wire.len());
        if cut < wire.len() {
            // Cutting before the finish chunk is complete must end in an error event. Once the
            // finish chunk's JSON is whole the answer is complete, with or without [DONE].
            let prefix = &wire.as_bytes()[..cut];
            let out = convert(&[prefix], Protocol::Chat);
            assert_valid_anthropic(&out);
            let finish_chunk_end = wire.find("\n\ndata: [DONE]").unwrap();
            let text = String::from_utf8(out).unwrap();
            if cut < finish_chunk_end {
                prop_assert!(text.contains("event: error") && !text.contains("message_stop"));
            }
        }
    }
}

#[test]
fn finish_reasons_pass_through_the_pipeline() {
    for (reason, want) in [
        ("stop", "end_turn"),
        ("length", "max_tokens"),
        ("tool_calls", "tool_use"),
        ("content_filter", "refusal"),
    ] {
        let wire = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"id":"p","model":"m","choices":[{"index":0,"delta":{"content":"x"},"finish_reason":reason}]})
        );
        let out = String::from_utf8(convert(&[wire.as_bytes()], Protocol::Chat)).unwrap();
        assert!(
            out.contains(&format!("\"stop_reason\":\"{want}\"")),
            "{reason}: {out}"
        );
    }
    let _ = FinishReason::Stop;
}
