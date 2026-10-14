//! Observability: the request log.
//!
//! An append-only, newline-delimited JSON file (`requests.jsonl`, `0600`, in a `0700` directory),
//! one record per inbound request and one per upstream attempt. The schema is forward-compatible:
//! `serde(default)` on read and unknown fields tolerated. Optional size-based rotation. Bodies are
//! never in this file; body logging is a separate opt-in file.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Whether a record describes the inbound request or one upstream attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RecordKind {
    /// One per inbound request.
    #[default]
    Request,
    /// One per upstream attempt.
    Attempt,
    /// One per call to an external classifier.
    Classifier,
}

/// One log record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RequestRecord {
    /// Request or attempt.
    pub kind: RecordKind,
    /// RFC 3339 UTC timestamp.
    pub ts: String,
    /// The request id.
    pub request_id: String,
    /// The client name.
    pub client: String,
    /// The inbound protocol.
    pub inbound: String,
    /// The model the client asked for.
    pub model_asked: String,
    /// The rule that selected the model entry, if any.
    pub rule: Option<String>,
    /// `upstream/model` that answered or was tried.
    pub target: Option<String>,
    /// The upstream model name.
    pub upstream_model: Option<String>,
    /// The HTTP status returned to the client (request) or received (attempt).
    pub status: u16,
    /// The error class, e.g. `rate_limit_error`.
    pub error_class: Option<String>,
    /// Attempts made.
    pub attempts: u32,
    /// Milliseconds to the first byte.
    pub ttfb_ms: Option<u64>,
    /// Total milliseconds.
    pub total_ms: u64,
    /// Whether the request streamed.
    pub stream: bool,
    /// Names of request parts ignored by policy.
    pub ignored: Vec<String>,
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Cached input tokens.
    pub cached_tokens: u64,
    /// Reasoning tokens.
    pub reasoning_tokens: u64,
    /// Estimated cost in US dollars; `None` when the model has no price (never zero).
    pub est_cost_usd: Option<f64>,
    /// Targets the router skipped, as `target:reason`.
    pub skipped: Vec<String>,
}

/// Receives records. An extension trait of the library API.
pub trait RecordSink: Send + Sync {
    /// Records one entry. Failures are the sink's to handle; a log must never fail a request.
    fn record(&self, rec: &RequestRecord);
}

/// Discards records.
pub struct NullSink;

impl RecordSink for NullSink {
    fn record(&self, _: &RequestRecord) {}
}

/// Formats a time as RFC 3339 UTC with millisecond precision.
#[must_use]
pub fn rfc3339(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = i64::try_from(d.as_secs()).unwrap_or(i64::MAX);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        d.subsec_millis()
    )
}

/// The current time as RFC 3339.
#[must_use]
pub fn now() -> String {
    rfc3339(SystemTime::now())
}

/// Rotation settings.
#[derive(Debug, Clone, Copy)]
pub struct Rotation {
    /// Rotate when the file reaches this many bytes.
    pub max_bytes: u64,
    /// How many rotated files to keep.
    pub keep: u32,
}

struct Inner {
    file: File,
    size: u64,
}

/// A JSONL file sink.
pub struct JsonlLog {
    path: PathBuf,
    rotation: Option<Rotation>,
    inner: Mutex<Inner>,
}

fn open_private(path: &Path) -> std::io::Result<File> {
    if let Some(dir) = path.parent() {
        let existed = dir.exists();
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        if !existed {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let _ = existed;
    }
    let mut o = OpenOptions::new();
    o.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

impl JsonlLog {
    /// Opens (creating if needed) the log at `path`.
    ///
    /// # Errors
    ///
    /// The file or its directory cannot be created.
    pub fn open(path: &Path, rotation: Option<Rotation>) -> std::io::Result<Self> {
        let file = open_private(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            path: path.to_owned(),
            rotation,
            inner: Mutex::new(Inner { file, size }),
        })
    }

    fn rotate(&self, inner: &mut Inner, keep: u32) -> std::io::Result<()> {
        let numbered = |n: u32| PathBuf::from(format!("{}.{n}", self.path.display()));
        let _ = std::fs::remove_file(numbered(keep));
        for n in (1..keep).rev() {
            if numbered(n).exists() {
                std::fs::rename(numbered(n), numbered(n + 1))?;
            }
        }
        std::fs::rename(&self.path, numbered(1))?;
        inner.file = open_private(&self.path)?;
        inner.size = 0;
        Ok(())
    }
}

impl RecordSink for JsonlLog {
    fn record(&self, rec: &RequestRecord) {
        let Ok(mut line) = serde_json::to_vec(rec) else {
            return;
        };
        line.push(b'\n');
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if let Some(r) = self.rotation {
            if inner.size > 0 && inner.size + line.len() as u64 > r.max_bytes {
                let _ = self.rotate(&mut inner, r.keep);
            }
        }
        if inner.file.write_all(&line).is_ok() {
            inner.size += line.len() as u64;
        }
    }
}

/// Reads records back, skipping lines that are not valid records.
#[must_use]
pub fn read_records(text: &str) -> Vec<RequestRecord> {
    text.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("airmate-obs-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn rec(id: &str) -> RequestRecord {
        RequestRecord {
            request_id: id.into(),
            status: 200,
            ts: now(),
            ..RequestRecord::default()
        }
    }

    #[test]
    fn timestamps_are_rfc3339_utc() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00.000Z",
            "a leap day"
        );
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_millis(1_760_443_200_123)),
            "2025-10-14T12:00:00.123Z"
        );
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(4_102_444_799)),
            "2099-12-31T23:59:59.000Z"
        );
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(1_709_251_199)),
            "2024-02-29T23:59:59.000Z"
        );
        let n = now();
        assert!(
            n.len() == 24 && n.ends_with('Z') && n.starts_with("20"),
            "{n}"
        );
    }

    #[test]
    fn records_round_trip_and_tolerate_unknown_and_missing_fields() {
        let r = RequestRecord {
            rule: Some("cheap".into()),
            est_cost_usd: Some(0.5),
            ignored: vec!["metadata".into()],
            ..rec("a")
        };
        let line = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<RequestRecord>(&line).unwrap(), r);
        let newer =
            r#"{"request_id":"b","status":503,"a_field_from_the_future":[1,2],"kind":"attempt"}"#;
        let parsed: RequestRecord = serde_json::from_str(newer).unwrap();
        assert_eq!(
            (parsed.request_id.as_str(), parsed.status, parsed.kind),
            ("b", 503, RecordKind::Attempt)
        );
        assert_eq!(parsed.attempts, 0, "missing fields default");
    }

    #[test]
    fn the_log_appends_one_line_per_record_and_reads_back() {
        let d = tmp("append");
        let p = d.join("state").join("requests.jsonl");
        let log = JsonlLog::open(&p, None).unwrap();
        log.record(&rec("one"));
        log.record(&rec("two"));
        drop(log);
        let log = JsonlLog::open(&p, None).unwrap();
        log.record(&rec("three"));
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text.lines().count(), 3);
        let back = read_records(&text);
        assert_eq!(
            back.iter()
                .map(|r| r.request_id.as_str())
                .collect::<Vec<_>>(),
            ["one", "two", "three"]
        );
        assert_eq!(
            read_records("not json\n{\"request_id\":\"x\"}\n").len(),
            1,
            "bad lines are skipped"
        );
        std::fs::remove_dir_all(d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_log_and_its_directory_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("perms");
        let p = d.join("state").join("requests.jsonl");
        let log = JsonlLog::open(&p, None).unwrap();
        log.record(&rec("x"));
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(p.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn size_based_rotation_keeps_a_bounded_number_of_files() {
        let d = tmp("rotate");
        let p = d.join("requests.jsonl");
        let one = serde_json::to_vec(&rec("x")).unwrap().len() as u64 + 1;
        let log = JsonlLog::open(
            &p,
            Some(Rotation {
                max_bytes: one * 2,
                keep: 2,
            }),
        )
        .unwrap();
        for i in 0..9 {
            log.record(&rec(&format!("r{i}")));
        }
        let names: Vec<String> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"requests.jsonl".to_owned()));
        assert!(
            names.contains(&"requests.jsonl.1".to_owned())
                && names.contains(&"requests.jsonl.2".to_owned())
        );
        assert!(
            !names.contains(&"requests.jsonl.3".to_owned()),
            "only `keep` rotated files remain: {names:?}"
        );
        let newest = read_records(&std::fs::read_to_string(&p).unwrap());
        assert_eq!(newest.last().unwrap().request_id, "r8");
        assert!(std::fs::metadata(&p).unwrap().len() <= one * 2);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn the_null_sink_discards() {
        NullSink.record(&rec("x"));
    }

    #[test]
    fn an_unwritable_path_is_an_error_not_a_panic() {
        // A regular file cannot be a parent directory on any platform.
        let file = std::env::temp_dir().join(format!("am-obs-notdir-{}", std::process::id()));
        std::fs::write(&file, b"x").unwrap();
        assert!(JsonlLog::open(&file.join("requests.jsonl"), None).is_err());
        std::fs::remove_file(&file).unwrap();
    }
}
