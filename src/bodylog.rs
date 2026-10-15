//! The body log: request and response bodies, opt-in (`log.bodies`), in a file of their own with
//! a size cap per body and a retention period. Bodies carry prompts, completions, thinking text and
//! tool inputs, so they never go in the request log and nothing writes them unless asked.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::obs::{rfc3339, JsonlLog};

/// One logged body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BodyRecord {
    /// RFC 3339 UTC timestamp.
    pub ts: String,
    /// The request id, to join with the request log.
    pub request_id: String,
    /// The client.
    pub client: String,
    /// `request` (from the client) or `response` (to the client).
    pub direction: String,
    /// The HTTP status, for a response.
    pub status: u16,
    /// The body's real size in bytes.
    pub bytes: usize,
    /// Whether `body` is cut short of `bytes`.
    pub truncated: bool,
    /// The body as text, invalid UTF-8 replaced, cut at `log.body_max_bytes`.
    pub body: String,
}

/// Receives bodies.
pub trait BodySink: Send + Sync {
    /// Whether anything is recorded; callers skip building records when it is not.
    fn enabled(&self) -> bool;
    /// The cap per body, in bytes.
    fn max_bytes(&self) -> usize;
    /// Records one body.
    fn record(&self, rec: &BodyRecord);
}

/// Discards bodies.
pub struct NullBodies;

impl BodySink for NullBodies {
    fn enabled(&self) -> bool {
        false
    }
    fn max_bytes(&self) -> usize {
        0
    }
    fn record(&self, _: &BodyRecord) {}
}

/// Cuts `bytes` to at most `max` bytes without splitting a character; returns the text and
/// whether anything was cut.
#[must_use]
pub fn clip(bytes: &[u8], max: usize) -> (String, bool) {
    if bytes.len() <= max {
        return (String::from_utf8_lossy(bytes).into_owned(), false);
    }
    let mut end = max;
    // Back up to a UTF-8 boundary so the cut does not manufacture a replacement character.
    while end > 0 && (bytes[end] & 0b1100_0000) == 0b1000_0000 {
        end -= 1;
    }
    (String::from_utf8_lossy(&bytes[..end]).into_owned(), true)
}

/// A record for `bytes`, clipped to `max`.
#[must_use]
pub fn make_record(
    request_id: &str,
    client: &str,
    direction: &str,
    status: u16,
    bytes: &[u8],
    max: usize,
) -> BodyRecord {
    let (body, truncated) = clip(bytes, max);
    BodyRecord {
        ts: crate::obs::now(),
        request_id: request_id.to_owned(),
        client: client.to_owned(),
        direction: direction.to_owned(),
        status,
        bytes: bytes.len(),
        truncated,
        body,
    }
}

/// Drops lines older than `cutoff` (an RFC 3339 string, which sorts as time does) from the file at
/// `path`, replacing it atomically. Returns how many lines were dropped.
///
/// # Errors
///
/// The file cannot be read or replaced.
pub fn prune(path: &Path, cutoff: &str) -> std::io::Result<usize> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut kept = String::with_capacity(text.len());
    let mut dropped = 0;
    for line in text.lines() {
        let old = serde_json::from_str::<BodyRecord>(line)
            .map(|r| r.ts.as_str() < cutoff)
            .unwrap_or(false);
        if old {
            dropped += 1;
        } else {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    if dropped == 0 {
        return Ok(0);
    }
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(&tmp)?.write_all(kept.as_bytes())?;
    std::fs::rename(&tmp, path)?;
    Ok(dropped)
}

/// The body log file.
pub struct JsonlBodies {
    log: JsonlLog,
    path: PathBuf,
    max: usize,
    retention: Duration,
    last_prune: Mutex<SystemTime>,
}

impl JsonlBodies {
    /// Opens the log, first dropping what is past retention.
    ///
    /// # Errors
    ///
    /// The file cannot be opened.
    pub fn open(path: &Path, max_bytes: u64, retention_days: u32) -> std::io::Result<Self> {
        let retention = Duration::from_secs(u64::from(retention_days) * 86_400);
        let this = Self {
            log: JsonlLog::open(path, None)?,
            path: path.to_owned(),
            max: usize::try_from(max_bytes).unwrap_or(usize::MAX),
            retention,
            last_prune: Mutex::new(SystemTime::UNIX_EPOCH),
        };
        this.prune_if_due();
        Ok(this)
    }

    fn prune_if_due(&self) {
        let now = SystemTime::now();
        let Ok(mut last) = self.last_prune.lock() else {
            return;
        };
        if now.duration_since(*last).unwrap_or_default() < Duration::from_secs(3_600) {
            return;
        }
        *last = now;
        let cutoff = rfc3339(now - self.retention);
        // Holding the append lock for the swap keeps a concurrent write out of the old file.
        let _ = self.log.exclusive(|| prune(&self.path, &cutoff));
    }
}

impl BodySink for JsonlBodies {
    fn enabled(&self) -> bool {
        true
    }

    fn max_bytes(&self) -> usize {
        self.max
    }

    fn record(&self, rec: &BodyRecord) {
        self.prune_if_due();
        self.log.append(rec);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("airmate-bodies-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d.join("bodies.jsonl")
    }

    #[test]
    fn clipping_never_splits_a_character() {
        assert_eq!(clip(b"hello", 10), ("hello".to_owned(), false));
        assert_eq!(clip(b"hello", 5), ("hello".to_owned(), false));
        assert_eq!(clip(b"hello", 3), ("hel".to_owned(), true));
        let s = "aé€"; // 1 + 2 + 3 bytes
        assert_eq!(
            clip(s.as_bytes(), 2),
            ("a".to_owned(), true),
            "the cut backs up past the half of é"
        );
        assert_eq!(clip(s.as_bytes(), 3), ("aé".to_owned(), true));
        assert_eq!(clip(s.as_bytes(), 5), ("aé".to_owned(), true));
        assert_eq!(
            clip(&[0xff, 0xfe], 10).0,
            "\u{fffd}\u{fffd}",
            "invalid UTF-8 is replaced, not an error"
        );
        assert_eq!(clip(b"", 0), (String::new(), false));
    }

    #[test]
    fn records_carry_the_real_size_and_whether_they_were_cut() {
        let r = make_record("req_1", "ci", "request", 0, b"0123456789", 4);
        assert_eq!(
            (r.bytes, r.truncated, r.body.as_str(), r.direction.as_str()),
            (10, true, "0123", "request")
        );
        assert!(r.ts.ends_with('Z'));
    }

    #[test]
    fn pruning_drops_only_records_older_than_the_cutoff() {
        let path = tmp("prune");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let line = |ts: &str, id: &str| {
            serde_json::to_string(&BodyRecord {
                ts: ts.into(),
                request_id: id.into(),
                ..BodyRecord::default()
            })
            .unwrap()
        };
        std::fs::write(
            &path,
            format!(
                "{}\n{}\nnot json\n{}\n",
                line("2026-01-01T00:00:00.000Z", "old"),
                line("2026-10-01T00:00:00.000Z", "new"),
                line("2025-12-31T23:59:59.999Z", "older")
            ),
        )
        .unwrap();
        assert_eq!(prune(&path, "2026-06-01T00:00:00.000Z").unwrap(), 2);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("\"new\"") && text.contains("not json") && !text.contains("\"old\""),
            "{text}"
        );
        assert_eq!(
            prune(&path, "2026-06-01T00:00:00.000Z").unwrap(),
            0,
            "nothing more to drop"
        );
        assert_eq!(prune(&path.with_extension("missing"), "x").unwrap(), 0);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn the_file_is_private_and_old_bodies_go_when_it_is_opened() {
        let path = tmp("open");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let old = serde_json::to_string(&BodyRecord {
            ts: "2020-01-01T00:00:00.000Z".into(),
            request_id: "ancient".into(),
            ..BodyRecord::default()
        })
        .unwrap();
        std::fs::write(&path, format!("{old}\n")).unwrap();
        let log = JsonlBodies::open(&path, 16, 7).unwrap();
        assert!(log.enabled());
        assert_eq!(log.max_bytes(), 16);
        log.record(&make_record(
            "req_2",
            "ci",
            "response",
            200,
            b"a body that is longer than sixteen bytes",
            log.max_bytes(),
        ));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("ancient"), "{text}");
        let rec: BodyRecord = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(
            (rec.request_id.as_str(), rec.truncated, rec.body.len()),
            ("req_2", true, 16)
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(!NullBodies.enabled());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
