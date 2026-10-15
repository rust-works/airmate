//! The AWS event stream framing (`application/vnd.amazon.eventstream`) that Bedrock's streaming
//! APIs use.
//!
//! A message is `total_len:u32 | headers_len:u32 | prelude_crc:u32 | headers | payload |
//! message_crc:u32`, all big-endian, with CRC-32 (IEEE) over the prelude and over the whole message
//! before the trailing CRC. A frame that fails either checksum, or announces a length past the cap,
//! is an error: nothing after it can be trusted to line up.

use std::collections::HashMap;

/// The longest message accepted. AWS caps messages at 24 MiB; no Converse event comes close.
pub const MAX_MESSAGE: usize = 1 << 20;

const PRELUDE: usize = 12;
const TRAILER: usize = 4;

/// CRC-32 (IEEE 802.3), the checksum the framing uses.
#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    crc32_update(0, data)
}

fn crc32_update(crc: u32, data: &[u8]) -> u32 {
    let mut c = !crc;
    for &b in data {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = if c & 1 == 1 {
                (c >> 1) ^ 0xEDB8_8320
            } else {
                c >> 1
            };
        }
    }
    !c
}

/// One decoded message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The string-valued headers (`:event-type`, `:message-type`, `:exception-type`,
    /// `:content-type`). Headers of other types are skipped.
    pub headers: HashMap<String, String>,
    /// The payload, JSON for Converse events.
    pub payload: Vec<u8>,
}

impl Frame {
    /// A header's value.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

/// Why decoding stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// A checksum did not match.
    Checksum,
    /// The lengths do not fit together or exceed the cap.
    Malformed(&'static str),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Checksum => f.write_str("an event stream message failed its checksum"),
            Self::Malformed(why) => write!(f, "malformed event stream message: {why}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Decodes a byte stream into frames, whatever the chunking.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
    failed: bool,
}

impl Decoder {
    /// A decoder with nothing buffered.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds bytes; returns the frames they completed.
    ///
    /// # Errors
    ///
    /// The first bad message. The decoder stays failed afterwards.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, DecodeError> {
        if self.failed {
            return Ok(Vec::new());
        }
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            match self.next_frame() {
                Ok(Some(f)) => out.push(f),
                Ok(None) => return Ok(out),
                Err(e) => {
                    self.failed = true;
                    self.buf.clear();
                    return Err(e);
                }
            }
        }
    }

    /// Whether bytes of an incomplete message are waiting.
    #[must_use]
    pub fn has_partial(&self) -> bool {
        !self.buf.is_empty() && !self.failed
    }

    fn next_frame(&mut self) -> Result<Option<Frame>, DecodeError> {
        if self.buf.len() < PRELUDE {
            return Ok(None);
        }
        let be = |i: usize| {
            u32::from_be_bytes([
                self.buf[i],
                self.buf[i + 1],
                self.buf[i + 2],
                self.buf[i + 3],
            ])
        };
        let (total, headers_len, prelude_crc) = (be(0) as usize, be(4) as usize, be(8));
        if crc32(&self.buf[..8]) != prelude_crc {
            return Err(DecodeError::Checksum);
        }
        if !(PRELUDE + TRAILER..=MAX_MESSAGE).contains(&total) {
            return Err(DecodeError::Malformed("the message length is out of range"));
        }
        if headers_len > total - PRELUDE - TRAILER {
            return Err(DecodeError::Malformed(
                "the headers are longer than the message",
            ));
        }
        if self.buf.len() < total {
            return Ok(None);
        }
        let message_crc = be(total - TRAILER);
        if crc32(&self.buf[..total - TRAILER]) != message_crc {
            return Err(DecodeError::Checksum);
        }
        let headers = parse_headers(&self.buf[PRELUDE..PRELUDE + headers_len])?;
        let payload = self.buf[PRELUDE + headers_len..total - TRAILER].to_vec();
        self.buf.drain(..total);
        Ok(Some(Frame { headers, payload }))
    }
}

fn parse_headers(mut b: &[u8]) -> Result<HashMap<String, String>, DecodeError> {
    let short = DecodeError::Malformed("a header is cut short");
    let mut out = HashMap::new();
    while !b.is_empty() {
        let name_len = usize::from(*b.first().ok_or_else(|| short.clone())?);
        let name = b.get(1..1 + name_len).ok_or_else(|| short.clone())?;
        let name = String::from_utf8_lossy(name).into_owned();
        let ty = *b.get(1 + name_len).ok_or_else(|| short.clone())?;
        b = &b[2 + name_len..];
        // Value types: 0/1 true/false, 2 byte, 3 i16, 4 i32, 5 i64, 6 bytes, 7 string, 8 timestamp,
        // 9 uuid.
        let (value, rest): (Option<String>, &[u8]) = match ty {
            0 | 1 => (None, b),
            2 => (None, b.get(1..).ok_or_else(|| short.clone())?),
            3 => (None, b.get(2..).ok_or_else(|| short.clone())?),
            4 => (None, b.get(4..).ok_or_else(|| short.clone())?),
            5 | 8 => (None, b.get(8..).ok_or_else(|| short.clone())?),
            9 => (None, b.get(16..).ok_or_else(|| short.clone())?),
            6 | 7 => {
                let len = usize::from(u16::from_be_bytes([
                    *b.first().ok_or_else(|| short.clone())?,
                    *b.get(1).ok_or_else(|| short.clone())?,
                ]));
                let v = b.get(2..2 + len).ok_or_else(|| short.clone())?;
                (
                    (ty == 7).then(|| String::from_utf8_lossy(v).into_owned()),
                    &b[2 + len..],
                )
            }
            _ => return Err(DecodeError::Malformed("an unknown header type")),
        };
        if let Some(v) = value {
            out.insert(name, v);
        }
        b = rest;
    }
    Ok(out)
}

/// Encodes a message with string headers: for tests and for fixtures.
#[must_use]
pub fn encode(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut h = Vec::new();
    for (k, v) in headers {
        h.push(u8::try_from(k.len()).expect("header names are short"));
        h.extend_from_slice(k.as_bytes());
        h.push(7);
        h.extend_from_slice(
            &u16::try_from(v.len())
                .expect("header values are short")
                .to_be_bytes(),
        );
        h.extend_from_slice(v.as_bytes());
    }
    let total =
        u32::try_from(PRELUDE + h.len() + payload.len() + TRAILER).expect("messages are small");
    let mut m = Vec::with_capacity(total as usize);
    m.extend_from_slice(&total.to_be_bytes());
    m.extend_from_slice(
        &u32::try_from(h.len())
            .expect("headers are small")
            .to_be_bytes(),
    );
    let prelude_crc = crc32(&m);
    m.extend_from_slice(&prelude_crc.to_be_bytes());
    m.extend_from_slice(&h);
    m.extend_from_slice(payload);
    let crc = crc32(&m);
    m.extend_from_slice(&crc.to_be_bytes());
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str, json: &str) -> Vec<u8> {
        encode(
            &[
                (":message-type", "event"),
                (":event-type", kind),
                (":content-type", "application/json"),
            ],
            json.as_bytes(),
        )
    }

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn a_message_round_trips_with_its_headers_and_payload() {
        let mut d = Decoder::new();
        let frames = d
            .push(&event("contentBlockDelta", r#"{"delta":{"text":"hi"}}"#))
            .unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].header(":event-type"), Some("contentBlockDelta"));
        assert_eq!(frames[0].header(":message-type"), Some("event"));
        assert_eq!(frames[0].payload, br#"{"delta":{"text":"hi"}}"#);
        assert!(!d.has_partial());
    }

    #[test]
    fn any_chunking_decodes_the_same_frames() {
        let bytes: Vec<u8> = [
            event("messageStart", r#"{"role":"assistant"}"#),
            event("messageStop", r#"{"stopReason":"end_turn"}"#),
        ]
        .concat();
        let whole = Decoder::new().push(&bytes).unwrap();
        assert_eq!(whole.len(), 2);
        for step in 1..bytes.len() {
            let mut d = Decoder::new();
            let mut got = Vec::new();
            for c in bytes.chunks(step) {
                got.extend(d.push(c).unwrap());
            }
            assert_eq!(got, whole, "step {step}");
        }
    }

    #[test]
    fn a_partial_message_waits() {
        let bytes = event("x", "{}");
        let mut d = Decoder::new();
        assert!(d.push(&bytes[..bytes.len() - 1]).unwrap().is_empty());
        assert!(d.has_partial());
        assert_eq!(d.push(&bytes[bytes.len() - 1..]).unwrap().len(), 1);
    }

    #[test]
    fn corruption_anywhere_is_caught_and_the_decoder_stays_failed() {
        let good = event("contentBlockDelta", r#"{"delta":{"text":"hi"}}"#);
        for i in 0..good.len() {
            let mut bad = good.clone();
            bad[i] ^= 0x01;
            let mut d = Decoder::new();
            let r = d.push(&bad);
            assert!(
                r.is_err() || r.as_ref().is_ok_and(Vec::is_empty),
                "flipping byte {i} must not decode: {r:?}"
            );
        }
        let mut d = Decoder::new();
        let mut bad = good.clone();
        let last = bad.len() - 1;
        bad[last] ^= 0xFF;
        assert_eq!(d.push(&bad), Err(DecodeError::Checksum));
        assert_eq!(
            d.push(&good),
            Ok(vec![]),
            "after a failure nothing more is decoded"
        );
    }

    #[test]
    fn absurd_lengths_are_refused_before_buffering_them() {
        let mut m = Vec::new();
        m.extend_from_slice(&u32::MAX.to_be_bytes());
        m.extend_from_slice(&0u32.to_be_bytes());
        let crc = crc32(&m);
        m.extend_from_slice(&crc.to_be_bytes());
        assert_eq!(
            Decoder::new().push(&m),
            Err(DecodeError::Malformed("the message length is out of range"))
        );
    }

    #[test]
    fn headers_of_other_types_are_skipped_not_misread() {
        // A bool header (type 0/1) and an i32 (type 4) before the string one.
        let mut h = Vec::new();
        h.extend_from_slice(&[1, b'a', 0]);
        h.extend_from_slice(&[1, b'b', 4, 0, 0, 0, 9]);
        h.extend_from_slice(&[4, b'n', b'a', b'm', b'e', 7, 0, 2, b'o', b'k']);
        let headers = parse_headers(&h).unwrap();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers["name"], "ok");
        assert!(parse_headers(&[5, b'a']).is_err());
    }
}
