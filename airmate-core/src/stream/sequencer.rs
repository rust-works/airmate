//! The stream sequencer.
//!
//! Anthropic content blocks are strictly sequential, but an upstream may interleave the argument
//! deltas of parallel tool calls. The sequencer streams the earliest unfinished block live and
//! buffers the deltas of later blocks until every earlier block has closed. The output is always a
//! valid sequence, and live whenever the upstream is sequential.

use std::collections::HashMap;

use super::IrEvent;

/// What a block holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockKind {
    /// Output text.
    Text,
    /// Reasoning.
    Reasoning,
    /// A tool call.
    ToolCall {
        /// The call id.
        id: String,
        /// The tool name.
        name: String,
    },
}

/// A sequenced output event. `index` counts blocks in emission order, from zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seq {
    /// A block opens.
    Start {
        /// The block index.
        index: u32,
        /// What it holds.
        kind: BlockKind,
    },
    /// Text, reasoning text, or a tool-argument fragment.
    Delta {
        /// The block index.
        index: u32,
        /// The content.
        text: String,
    },
    /// A block closes; `opaque` is the upstream state sealed with a reasoning block.
    Stop {
        /// The block index.
        index: u32,
        /// Opaque upstream state.
        opaque: Option<Vec<u8>>,
    },
}

/// The interleaving buffer exceeded its cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferCapExceeded {
    /// The cap in bytes.
    pub cap: usize,
}

#[derive(Debug)]
struct Blk {
    kind: BlockKind,
    started: bool,
    index: u32,
    ended: bool,
    has_content: bool,
    pending: String,
    opaque: Option<Vec<u8>>,
}

/// Turns possibly interleaved block events into a strictly sequential block stream.
#[derive(Debug)]
pub struct Sequencer {
    blocks: Vec<Blk>,
    by_id: HashMap<u32, usize>,
    head: usize,
    next_index: u32,
    buffered: usize,
    cap: usize,
}

impl Sequencer {
    /// A sequencer that buffers at most `cap` bytes of later blocks.
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            blocks: Vec::new(),
            by_id: HashMap::new(),
            head: 0,
            next_index: 0,
            buffered: 0,
            cap,
        }
    }

    fn block(&mut self, id: u32, kind: impl FnOnce() -> BlockKind) -> usize {
        if let Some(&i) = self.by_id.get(&id) {
            return i;
        }
        let i = self.blocks.len();
        self.blocks.push(Blk {
            kind: kind(),
            started: false,
            index: 0,
            ended: false,
            has_content: false,
            pending: String::new(),
            opaque: None,
        });
        self.by_id.insert(id, i);
        i
    }

    /// Feeds one event. Events that are not block events are ignored.
    ///
    /// # Errors
    ///
    /// [`BufferCapExceeded`] when later blocks have buffered more than the cap.
    pub fn push(&mut self, ev: &IrEvent) -> Result<Vec<Seq>, BufferCapExceeded> {
        match ev {
            IrEvent::TextDelta { block, text } => {
                let i = self.block(*block, || BlockKind::Text);
                self.delta(i, text);
            }
            IrEvent::ReasoningDelta { block, text } => {
                let i = self.block(*block, || BlockKind::Reasoning);
                self.delta(i, text);
            }
            IrEvent::ToolCallStart { block, id, name } => {
                let i = self.block(*block, || BlockKind::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                });
                self.blocks[i].has_content = true;
            }
            IrEvent::ToolCallArgs { block, fragment } => {
                if let Some(&i) = self.by_id.get(block) {
                    self.delta(i, fragment);
                }
            }
            IrEvent::TextEnd { block } | IrEvent::ToolCallEnd { block } => {
                if let Some(&i) = self.by_id.get(block) {
                    self.blocks[i].ended = true;
                }
            }
            IrEvent::ReasoningSealed { block, opaque } => {
                let i = self.block(*block, || BlockKind::Reasoning);
                self.blocks[i].ended = true;
                self.blocks[i].has_content = true;
                self.blocks[i].opaque.clone_from(opaque);
            }
            _ => return Ok(Vec::new()),
        }
        let out = self.drain();
        if self.buffered > self.cap {
            return Err(BufferCapExceeded { cap: self.cap });
        }
        Ok(out)
    }

    fn delta(&mut self, i: usize, text: &str) {
        let b = &mut self.blocks[i];
        if !text.is_empty() {
            b.has_content = true;
        }
        b.pending.push_str(text);
    }

    /// Ends every open block and flushes what was buffered.
    pub fn finish(&mut self) -> Vec<Seq> {
        for b in &mut self.blocks {
            b.ended = true;
        }
        self.drain()
    }

    fn drain(&mut self) -> Vec<Seq> {
        let mut out = Vec::new();
        while self.head < self.blocks.len() {
            let head = self.head;
            let b = &mut self.blocks[head];
            if b.ended && !b.has_content {
                self.head += 1;
                continue;
            }
            if !b.started && (b.has_content || !b.pending.is_empty()) {
                b.started = true;
                b.index = self.next_index;
                self.next_index += 1;
                out.push(Seq::Start {
                    index: b.index,
                    kind: b.kind.clone(),
                });
            }
            if !b.pending.is_empty() {
                out.push(Seq::Delta {
                    index: b.index,
                    text: std::mem::take(&mut b.pending),
                });
            }
            if b.ended {
                out.push(Seq::Stop {
                    index: b.index,
                    opaque: b.opaque.take(),
                });
                self.head += 1;
            } else {
                break;
            }
        }
        // What remains buffered belongs to the blocks behind the head.
        self.buffered = self.blocks[self.head.min(self.blocks.len())..]
            .iter()
            .skip(1)
            .map(|b| b.pending.len())
            .sum();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(block: u32, t: &str) -> IrEvent {
        IrEvent::TextDelta {
            block,
            text: t.into(),
        }
    }
    fn call(block: u32, id: &str, name: &str) -> IrEvent {
        IrEvent::ToolCallStart {
            block,
            id: id.into(),
            name: name.into(),
        }
    }
    fn args(block: u32, f: &str) -> IrEvent {
        IrEvent::ToolCallArgs {
            block,
            fragment: f.into(),
        }
    }

    fn run(events: &[IrEvent]) -> Vec<Seq> {
        let mut s = Sequencer::new(1 << 20);
        let mut out = Vec::new();
        for e in events {
            out.extend(s.push(e).unwrap());
        }
        out.extend(s.finish());
        out
    }

    fn start(index: u32, kind: BlockKind) -> Seq {
        Seq::Start { index, kind }
    }
    fn delta(index: u32, t: &str) -> Seq {
        Seq::Delta {
            index,
            text: t.into(),
        }
    }
    fn stop(index: u32) -> Seq {
        Seq::Stop {
            index,
            opaque: None,
        }
    }
    fn tool(id: &str, name: &str) -> BlockKind {
        BlockKind::ToolCall {
            id: id.into(),
            name: name.into(),
        }
    }

    #[test]
    fn a_sequential_stream_is_live_and_unchanged() {
        let mut s = Sequencer::new(1024);
        // Each event produces its output immediately.
        assert_eq!(
            s.push(&text(0, "a")).unwrap(),
            vec![start(0, BlockKind::Text), delta(0, "a")]
        );
        assert_eq!(s.push(&text(0, "b")).unwrap(), vec![delta(0, "b")]);
        assert_eq!(
            s.push(&IrEvent::TextEnd { block: 0 }).unwrap(),
            vec![stop(0)]
        );
        assert_eq!(
            s.push(&call(1, "c", "n")).unwrap(),
            vec![start(1, tool("c", "n"))]
        );
        assert_eq!(s.push(&args(1, "{}")).unwrap(), vec![delta(1, "{}")]);
        assert_eq!(
            s.push(&IrEvent::ToolCallEnd { block: 1 }).unwrap(),
            vec![stop(1)]
        );
        assert!(s.finish().is_empty());
    }

    #[test]
    fn interleaved_tool_calls_come_out_one_after_the_other() {
        let out = run(&[
            call(0, "a", "f"),
            call(1, "b", "g"),
            args(0, "{\"p\":"),
            args(1, "{\"q\":"),
            args(1, "2}"),
            IrEvent::ToolCallEnd { block: 1 },
            args(0, "1}"),
            IrEvent::ToolCallEnd { block: 0 },
        ]);
        assert_eq!(
            out,
            vec![
                start(0, tool("a", "f")),
                delta(0, "{\"p\":"),
                delta(0, "1}"),
                stop(0),
                start(1, tool("b", "g")),
                delta(1, "{\"q\":2}"),
                stop(1),
            ]
        );
    }

    #[test]
    fn a_later_block_that_finished_early_waits_for_the_earlier_one() {
        let mut s = Sequencer::new(1024);
        let mut out = s.push(&call(0, "a", "f")).unwrap();
        out.extend(s.push(&call(1, "b", "g")).unwrap());
        out.extend(s.push(&args(1, "{}")).unwrap());
        out.extend(s.push(&IrEvent::ToolCallEnd { block: 1 }).unwrap());
        assert_eq!(
            out,
            vec![start(0, tool("a", "f"))],
            "block 1 is buffered while 0 is open"
        );
        let rest = s.push(&IrEvent::ToolCallEnd { block: 0 }).unwrap();
        assert_eq!(
            rest,
            vec![stop(0), start(1, tool("b", "g")), delta(1, "{}"), stop(1)]
        );
    }

    #[test]
    fn indices_are_consecutive_in_emission_order() {
        let out = run(&[
            text(5, "x"),
            IrEvent::TextEnd { block: 5 },
            call(2, "c", "n"),
            IrEvent::ToolCallEnd { block: 2 },
        ]);
        let starts: Vec<u32> = out
            .iter()
            .filter_map(|s| {
                if let Seq::Start { index, .. } = s {
                    Some(*index)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(starts, vec![0, 1]);
    }

    #[test]
    fn empty_text_blocks_are_not_emitted() {
        let out = run(&[
            IrEvent::TextEnd { block: 0 },
            text(1, ""),
            IrEvent::TextEnd { block: 1 },
            text(2, "real"),
            IrEvent::TextEnd { block: 2 },
        ]);
        assert_eq!(
            out,
            vec![start(0, BlockKind::Text), delta(0, "real"), stop(0)]
        );
    }

    #[test]
    fn a_sealed_reasoning_block_keeps_its_opaque_state_even_if_empty() {
        let out = run(&[IrEvent::ReasoningSealed {
            block: 0,
            opaque: Some(vec![9, 9]),
        }]);
        assert_eq!(
            out,
            vec![
                start(0, BlockKind::Reasoning),
                Seq::Stop {
                    index: 0,
                    opaque: Some(vec![9, 9])
                }
            ]
        );
        let out = run(&[
            IrEvent::ReasoningDelta {
                block: 0,
                text: "why".into(),
            },
            IrEvent::ReasoningSealed {
                block: 0,
                opaque: None,
            },
        ]);
        assert_eq!(
            out,
            vec![start(0, BlockKind::Reasoning), delta(0, "why"), stop(0)]
        );
    }

    #[test]
    fn finish_closes_open_blocks_and_flushes_the_buffer() {
        let out = run(&[call(0, "a", "f"), call(1, "b", "g"), args(1, "{}")]);
        assert_eq!(
            out,
            vec![
                start(0, tool("a", "f")),
                stop(0),
                start(1, tool("b", "g")),
                delta(1, "{}"),
                stop(1)
            ]
        );
    }

    #[test]
    fn the_buffer_cap_is_enforced_for_later_blocks_only() {
        let mut s = Sequencer::new(8);
        s.push(&call(0, "a", "f")).unwrap();
        // The head may stream any amount.
        s.push(&args(0, &"x".repeat(100))).unwrap();
        s.push(&call(1, "b", "g")).unwrap();
        assert!(s.push(&args(1, "12345678")).is_ok());
        assert_eq!(
            s.push(&args(1, "9")).unwrap_err(),
            BufferCapExceeded { cap: 8 }
        );
    }

    #[test]
    fn the_buffer_drains_when_a_block_reaches_the_head() {
        let mut s = Sequencer::new(8);
        s.push(&call(0, "a", "f")).unwrap();
        s.push(&call(1, "b", "g")).unwrap();
        s.push(&args(1, "12345678")).unwrap();
        s.push(&IrEvent::ToolCallEnd { block: 0 }).unwrap();
        // Block 1 is now live, so more bytes do not count against the cap.
        assert!(s.push(&args(1, "123456789")).is_ok());
    }

    #[test]
    fn non_block_events_and_unknown_blocks_are_ignored() {
        let mut s = Sequencer::new(8);
        assert!(s
            .push(&IrEvent::Refusal { text: "x".into() })
            .unwrap()
            .is_empty());
        assert!(s.push(&args(7, "{}")).unwrap().is_empty());
        assert!(s.push(&IrEvent::TextEnd { block: 7 }).unwrap().is_empty());
        assert!(s.finish().is_empty());
    }

    #[test]
    fn output_is_valid_for_every_interleaving_of_two_calls() {
        // All orderings of the four argument fragments that keep each call's own order.
        let a = ["{\"a\":", "1}"];
        let b = ["{\"b\":", "2}"];
        let orders: [[u8; 4]; 6] = [
            [0, 0, 1, 1],
            [0, 1, 0, 1],
            [0, 1, 1, 0],
            [1, 0, 0, 1],
            [1, 0, 1, 0],
            [1, 1, 0, 0],
        ];
        for order in orders {
            let mut events = vec![call(0, "a", "f"), call(1, "b", "g")];
            let (mut ia, mut ib) = (0, 0);
            for who in order {
                if who == 0 {
                    events.push(args(0, a[ia]));
                    ia += 1;
                } else {
                    events.push(args(1, b[ib]));
                    ib += 1;
                }
            }
            events.push(IrEvent::ToolCallEnd { block: 0 });
            events.push(IrEvent::ToolCallEnd { block: 1 });
            let out = run(&events);
            let joined = |idx: u32| -> String {
                out.iter()
                    .filter_map(|s| match s {
                        Seq::Delta { index, text } if *index == idx => Some(text.as_str()),
                        _ => None,
                    })
                    .collect()
            };
            assert_eq!(joined(0), "{\"a\":1}", "{order:?}");
            assert_eq!(joined(1), "{\"b\":2}", "{order:?}");
            let shape: Vec<&str> = out
                .iter()
                .map(|s| match s {
                    Seq::Start { .. } => "start",
                    Seq::Delta { .. } => "delta",
                    Seq::Stop { .. } => "stop",
                })
                .collect();
            let mut open = false;
            for s in &shape {
                match *s {
                    "start" => {
                        assert!(!open, "{order:?}");
                        open = true;
                    }
                    "stop" => {
                        assert!(open, "{order:?}");
                        open = false;
                    }
                    _ => assert!(open, "{order:?}"),
                }
            }
            assert!(!open);
        }
    }
}
