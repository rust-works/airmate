//! Protocol-neutral streaming.
//!
//! Upstream stream parsers turn bytes into [`IrEvent`]s; inbound renderers turn [`IrEvent`]s into
//! the client's protocol. Both are state machines: `push(bytes)` and `finish()`.

pub mod anthropic_render;
pub mod chat_render;
pub mod gemini_render;
pub mod responses_render;
pub mod sequencer;
pub mod think;

use crate::ir::{FinishReason, Usage};

/// Why a stream failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamErrorKind {
    /// The stream ended without a terminal marker: a cut-off answer is never a finished one.
    Truncated,
    /// The upstream sent an error event.
    Upstream,
    /// The upstream sent something that is not valid for its protocol.
    Protocol,
    /// A buffer cap was exceeded.
    TooLarge,
}

/// A stream failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamError {
    /// The class of failure.
    pub kind: StreamErrorKind,
    /// A message for the client.
    pub message: String,
}

impl StreamError {
    /// A stream that stopped before it was complete.
    #[must_use]
    pub fn truncated() -> Self {
        Self {
            kind: StreamErrorKind::Truncated,
            message: "the upstream stream ended before the response was complete".to_owned(),
        }
    }
}

/// An event of the protocol-neutral stream.
///
/// `block` numbers identify content blocks (text, reasoning, tool calls) and are unique within a
/// stream. A parser opens a block with its first event and ends it explicitly; the sequencer
/// turns the possibly interleaved result into strictly sequential output.
#[derive(Debug, Clone, PartialEq)]
pub enum IrEvent {
    /// The response began.
    Start {
        /// The upstream's response id.
        id: Option<String>,
        /// The upstream's model name.
        model: Option<String>,
    },
    /// Output text.
    TextDelta {
        /// The block.
        block: u32,
        /// The text.
        text: String,
    },
    /// A text block ended.
    TextEnd {
        /// The block.
        block: u32,
    },
    /// Reasoning text.
    ReasoningDelta {
        /// The block.
        block: u32,
        /// The text.
        text: String,
    },
    /// A reasoning block ended; `opaque` carries upstream state such as encrypted content.
    ReasoningSealed {
        /// The block.
        block: u32,
        /// Opaque upstream state.
        opaque: Option<Vec<u8>>,
    },
    /// A tool call began.
    ToolCallStart {
        /// The block.
        block: u32,
        /// The call id.
        id: String,
        /// The tool name as the client knows it.
        name: String,
    },
    /// A fragment of a tool call's JSON arguments.
    ToolCallArgs {
        /// The block.
        block: u32,
        /// The fragment.
        fragment: String,
    },
    /// A tool call's arguments are complete.
    ToolCallEnd {
        /// The block.
        block: u32,
    },
    /// The model refused; the text is the refusal.
    Refusal {
        /// The refusal text.
        text: String,
    },
    /// Token usage (may arrive after the finish).
    Usage(Usage),
    /// The response finished.
    Finish(FinishReason),
    /// The stream failed.
    Error(StreamError),
}

/// Parses an upstream byte stream into [`IrEvent`]s.
pub trait StreamParse: Send {
    /// Feeds bytes and returns the events they completed.
    fn push(&mut self, bytes: &[u8]) -> Vec<IrEvent>;
    /// Signals end of input. A stream that has not reached its terminal marker yields an
    /// [`IrEvent::Error`] with [`StreamErrorKind::Truncated`].
    fn finish(&mut self) -> Vec<IrEvent>;
}

/// Renders [`IrEvent`]s into the client's protocol.
pub trait StreamRender: Send {
    /// Feeds one event and returns the bytes to send.
    fn push(&mut self, ev: &IrEvent) -> Vec<u8>;
    /// Ends the render; emits an error event if the stream was cut off before a terminal event.
    fn finish(&mut self) -> Vec<u8>;
    /// Bytes to send while the upstream is silent.
    fn keepalive(&self) -> Vec<u8>;
    /// Whether the stream has ended.
    fn is_done(&self) -> bool;
}

/// Tracks whether a JSON value, fed in fragments, has become complete.
///
/// It counts `{`/`[` depth outside strings. Tool-call arguments are JSON objects, so the call is
/// complete when the depth returns to zero after an opening brace.
#[derive(Debug, Clone, Default)]
pub struct JsonCompletion {
    depth: u32,
    in_string: bool,
    escaped: bool,
    started: bool,
    complete: bool,
}

impl JsonCompletion {
    /// A fresh tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds a fragment and reports whether the value is now complete.
    pub fn feed(&mut self, fragment: &str) -> bool {
        for c in fragment.chars() {
            if self.complete {
                break;
            }
            if self.in_string {
                if self.escaped {
                    self.escaped = false;
                } else if c == '\\' {
                    self.escaped = true;
                } else if c == '"' {
                    self.in_string = false;
                }
                continue;
            }
            match c {
                '"' => self.in_string = true,
                '{' | '[' => {
                    self.started = true;
                    self.depth += 1;
                }
                '}' | ']' => {
                    self.depth = self.depth.saturating_sub(1);
                    if self.started && self.depth == 0 {
                        self.complete = true;
                    }
                }
                _ => {}
            }
        }
        self.complete
    }

    /// Whether the value is complete.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.complete
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(parts: &[&str]) -> bool {
        let mut j = JsonCompletion::new();
        let mut last = false;
        for p in parts {
            last = j.feed(p);
        }
        last
    }

    #[test]
    fn an_object_is_complete_when_its_braces_close() {
        assert!(!feed_all(&["{\"a\":"]));
        assert!(!feed_all(&["{\"a\":", "{\"b\":1}"]));
        assert!(feed_all(&["{\"a\":", "{\"b\":1}", "}"]));
        assert!(feed_all(&["{}"]));
    }

    #[test]
    fn braces_inside_strings_do_not_count() {
        assert!(!feed_all(&["{\"a\":\"}\""]));
        assert!(feed_all(&["{\"a\":\"}\"}"]));
        assert!(feed_all(&["{\"a\":\"\\\"}\"", "}"]));
        assert!(!feed_all(&["{\"a\":\"\\", "\"}"]));
    }

    #[test]
    fn nothing_is_complete_before_a_brace_opens() {
        assert!(!feed_all(&[""]));
        assert!(!feed_all(&["  "]));
        assert!(!JsonCompletion::new().is_complete());
    }

    #[test]
    fn arrays_complete_too_and_trailing_text_is_ignored() {
        assert!(feed_all(&["[1,[2]]"]));
        assert!(feed_all(&["{}{"]));
    }

    #[test]
    fn truncation_has_a_stock_error() {
        let e = StreamError::truncated();
        assert_eq!(e.kind, StreamErrorKind::Truncated);
        assert!(e.message.contains("before the response was complete"));
    }
}
