//! Server-sent events framing: an incremental parser and a writer.
//!
//! The parser is a state machine: [`SseParser::push`] takes whatever bytes arrived (split at any
//! offset, including inside a multi-byte character or between `\r` and `\n`) and returns the
//! events completed so far. Buffers are bounded, so a hostile or broken upstream cannot make it
//! grow without limit.

use std::fmt;

/// One dispatched event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    /// The `event:` field, if any.
    pub event: Option<String>,
    /// The `data:` lines joined with `\n`.
    pub data: String,
    /// The `id:` field, if any.
    pub id: Option<String>,
}

/// A framing error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseError {
    /// A single line exceeded the configured limit.
    LineTooLong {
        /// The limit in bytes.
        limit: usize,
    },
    /// An event's data exceeded the configured limit.
    EventTooLarge {
        /// The limit in bytes.
        limit: usize,
    },
}

impl fmt::Display for SseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LineTooLong { limit } => write!(f, "SSE line longer than {limit} bytes"),
            Self::EventTooLarge { limit } => write!(f, "SSE event larger than {limit} bytes"),
        }
    }
}

impl std::error::Error for SseError {}

/// Default cap on one line, 4 MiB.
pub const DEFAULT_MAX_LINE: usize = 4 * 1024 * 1024;
/// Default cap on one event's data, 16 MiB.
pub const DEFAULT_MAX_EVENT: usize = 16 * 1024 * 1024;

/// An incremental SSE parser.
#[derive(Debug)]
pub struct SseParser {
    line: Vec<u8>,
    pending_cr: bool,
    at_start: bool,
    event: Option<String>,
    data: Option<String>,
    id: Option<String>,
    max_line: usize,
    max_event: usize,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SseParser {
    /// A parser with the default limits.
    #[must_use]
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_MAX_LINE, DEFAULT_MAX_EVENT)
    }

    /// A parser with explicit limits.
    #[must_use]
    pub fn with_limits(max_line: usize, max_event: usize) -> Self {
        Self {
            line: Vec::new(),
            pending_cr: false,
            at_start: true,
            event: None,
            data: None,
            id: None,
            max_line,
            max_event,
        }
    }

    /// Feeds bytes and returns the events they completed.
    ///
    /// # Errors
    ///
    /// Returns an error when a line or an event exceeds its limit. The parser should be discarded
    /// afterwards.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, SseError> {
        let mut out = Vec::new();
        for &b in bytes {
            if self.pending_cr {
                self.pending_cr = false;
                if b == b'\n' {
                    continue;
                }
            }
            match b {
                b'\n' => self.end_line(&mut out)?,
                b'\r' => {
                    self.pending_cr = true;
                    self.end_line(&mut out)?;
                }
                _ => {
                    if self.line.len() >= self.max_line {
                        return Err(SseError::LineTooLong {
                            limit: self.max_line,
                        });
                    }
                    self.line.push(b);
                }
            }
        }
        Ok(out)
    }

    /// Signals end of input. A trailing line without a newline, and an event without its closing
    /// blank line, are flushed rather than lost; higher layers decide whether the stream was
    /// complete.
    ///
    /// # Errors
    ///
    /// Returns an error when the flushed line or event exceeds its limit.
    pub fn finish(&mut self) -> Result<Vec<SseEvent>, SseError> {
        let mut out = Vec::new();
        if !self.line.is_empty() {
            self.end_line(&mut out)?;
        }
        if let Some(ev) = self.dispatch() {
            out.push(ev);
        }
        Ok(out)
    }

    fn end_line(&mut self, out: &mut Vec<SseEvent>) -> Result<(), SseError> {
        let raw = std::mem::take(&mut self.line);
        let mut text = String::from_utf8_lossy(&raw).into_owned();
        if self.at_start {
            self.at_start = false;
            if let Some(rest) = text.strip_prefix('\u{feff}') {
                text = rest.to_owned();
            }
        }
        if text.is_empty() {
            if let Some(ev) = self.dispatch() {
                out.push(ev);
            }
            return Ok(());
        }
        if text.starts_with(':') {
            return Ok(());
        }
        let (name, value) = match text.split_once(':') {
            Some((n, v)) => (n, v.strip_prefix(' ').unwrap_or(v)),
            None => (text.as_str(), ""),
        };
        match name {
            "event" => self.event = Some(value.to_owned()),
            "data" => {
                let used = self.data.as_ref().map_or(0, |d| d.len() + 1);
                if used + value.len() > self.max_event {
                    return Err(SseError::EventTooLarge {
                        limit: self.max_event,
                    });
                }
                match &mut self.data {
                    Some(d) => {
                        d.push('\n');
                        d.push_str(value);
                    }
                    None => self.data = Some(value.to_owned()),
                }
            }
            "id" => self.id = Some(value.to_owned()),
            _ => {}
        }
        Ok(())
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        let data = self.data.take();
        let event = self.event.take();
        let id = self.id.take();
        if data.is_none() && event.is_none() && id.is_none() {
            return None;
        }
        Some(SseEvent {
            event,
            data: data.unwrap_or_default(),
            id,
        })
    }
}

/// Formats one event for the wire. Multi-line data becomes several `data:` lines.
#[must_use]
pub fn format_event(event: Option<&str>, data: &str) -> Vec<u8> {
    let mut out = String::new();
    if let Some(e) = event {
        out.push_str("event: ");
        out.push_str(e);
        out.push('\n');
    }
    for line in data.split('\n') {
        out.push_str("data: ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out.into_bytes()
}

/// A comment line, used as a keep-alive on streams that have no `ping` event.
#[must_use]
pub fn format_comment(text: &str) -> Vec<u8> {
    format!(": {text}\n\n").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(chunks: &[&[u8]]) -> Vec<SseEvent> {
        let mut p = SseParser::new();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(p.push(c).unwrap());
        }
        out.extend(p.finish().unwrap());
        out
    }

    fn ev(event: Option<&str>, data: &str) -> SseEvent {
        SseEvent {
            event: event.map(str::to_owned),
            data: data.to_owned(),
            id: None,
        }
    }

    #[test]
    fn parses_named_and_unnamed_events() {
        let got = parse_all(&[b"event: a\ndata: 1\n\ndata: 2\n\n"]);
        assert_eq!(got, vec![ev(Some("a"), "1"), ev(None, "2")]);
    }

    #[test]
    fn joins_multiple_data_lines_with_newlines() {
        let got = parse_all(&[b"data: x\ndata: y\ndata:\ndata: z\n\n"]);
        assert_eq!(got, vec![ev(None, "x\ny\n\nz")]);
    }

    #[test]
    fn accepts_all_line_endings() {
        for sep in ["\n", "\r\n", "\r"] {
            let text = format!("data: a{sep}{sep}data: b{sep}{sep}");
            assert_eq!(
                parse_all(&[text.as_bytes()]),
                vec![ev(None, "a"), ev(None, "b")],
                "{sep:?}"
            );
        }
    }

    #[test]
    fn ignores_comments_and_unknown_fields() {
        let got = parse_all(&[b": keep-alive\nretry: 5\nfoo: bar\ndata: ok\n\n"]);
        assert_eq!(got, vec![ev(None, "ok")]);
    }

    #[test]
    fn strips_one_leading_space_only() {
        let got = parse_all(&[b"data:  two\n\ndata:none\n\n"]);
        assert_eq!(got, vec![ev(None, " two"), ev(None, "none")]);
    }

    #[test]
    fn strips_a_leading_byte_order_mark() {
        let got = parse_all(&[b"\xef\xbb\xbfdata: a\n\n"]);
        assert_eq!(got, vec![ev(None, "a")]);
    }

    #[test]
    fn flushes_an_unterminated_final_event() {
        let got = parse_all(&[b"data: tail"]);
        assert_eq!(got, vec![ev(None, "tail")]);
        let got = parse_all(&[b"data: tail\n"]);
        assert_eq!(got, vec![ev(None, "tail")]);
    }

    #[test]
    fn keeps_the_id_field() {
        let got = parse_all(&[b"id: 7\ndata: a\n\n"]);
        assert_eq!(got[0].id.as_deref(), Some("7"));
    }

    #[test]
    fn blank_lines_alone_dispatch_nothing() {
        assert!(parse_all(&[b"\n\n\n"]).is_empty());
    }

    #[test]
    fn splitting_at_every_byte_offset_gives_the_same_events() {
        let text = "event: x\r\ndata: h\u{e9}llo \u{1f600}\r\ndata: 2\r\n\r\n: c\n\ndata: 3\n\n";
        let whole = parse_all(&[text.as_bytes()]);
        assert_eq!(whole.len(), 2);
        for i in 0..=text.len() {
            let (a, b) = text.as_bytes().split_at(i);
            assert_eq!(parse_all(&[a, b]), whole, "split at {i}");
        }
    }

    #[test]
    fn byte_at_a_time_matches_the_whole_input() {
        let text = "data: a\r\n\r\ndata: b\n\n";
        let chunks: Vec<&[u8]> = text.as_bytes().chunks(1).collect();
        assert_eq!(parse_all(&chunks), parse_all(&[text.as_bytes()]));
    }

    #[test]
    fn rejects_an_oversized_line() {
        let mut p = SseParser::with_limits(8, 1024);
        assert_eq!(
            p.push(b"data: 123456789").unwrap_err(),
            SseError::LineTooLong { limit: 8 }
        );
    }

    #[test]
    fn rejects_an_oversized_event() {
        let mut p = SseParser::with_limits(64, 10);
        let err = p.push(b"data: aaaaaa\ndata: bbbbbb\n").unwrap_err();
        assert_eq!(err, SseError::EventTooLarge { limit: 10 });
    }

    #[test]
    fn writes_events_that_parse_back() {
        let wire = format_event(Some("message"), "a\nb");
        assert_eq!(wire, b"event: message\ndata: a\ndata: b\n\n");
        assert_eq!(parse_all(&[&wire]), vec![ev(Some("message"), "a\nb")]);
        assert_eq!(format_event(None, "x"), b"data: x\n\n");
        assert_eq!(format_comment("ping"), b": ping\n\n");
    }

    #[test]
    fn errors_display() {
        assert!(SseError::LineTooLong { limit: 3 }.to_string().contains('3'));
        assert!(SseError::EventTooLarge { limit: 4 }
            .to_string()
            .contains('4'));
    }
}
