//! `airmate logs`: search and pretty-print the request log, optionally following it.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;

use super::common::{CliError, Process};
use crate::obs::{oneline, parse_rfc3339, RecordKind, RequestRecord, StatusFilter};
use crate::usage::parse_since;

/// How `logs` prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// One line per record.
    Oneline,
    /// The record as JSON, one per line.
    Json,
}

/// What `logs` keeps.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// Only this client.
    pub client: Option<String>,
    /// Only these statuses.
    pub status: Option<StatusFilter>,
    /// Only records at or after this Unix time.
    pub since: Option<u64>,
    /// Also show upstream attempts and classifier calls, not just requests.
    pub all_kinds: bool,
}

impl Filter {
    /// Whether `r` passes.
    #[must_use]
    pub fn keeps(&self, r: &RequestRecord) -> bool {
        (self.all_kinds || r.kind == RecordKind::Request)
            && self.client.as_ref().is_none_or(|c| *c == r.client)
            && self.status.is_none_or(|s| s.matches(r.status))
            && self
                .since
                .is_none_or(|t| parse_rfc3339(&r.ts).is_some_and(|x| x >= t))
    }
}

fn print(r: &RequestRecord, format: Format, out: &mut dyn Write) -> std::io::Result<()> {
    match format {
        Format::Oneline => writeln!(out, "{}", oneline(r)),
        Format::Json => writeln!(out, "{}", serde_json::to_string(r).unwrap_or_default()),
    }
}

/// The options of one `logs` run.
pub struct Options<'a> {
    /// What to keep.
    pub filter: Filter,
    /// How to print.
    pub format: Format,
    /// Keep watching for new records.
    pub follow: bool,
    /// Show only the last this many matches of what is already there.
    pub tail: Option<usize>,
    /// The `--since` text, resolved against the clock when the command runs.
    pub since: Option<&'a str>,
}

/// Prints matching records from the request log.
///
/// # Errors
///
/// A bad `--since`, or a log that cannot be read.
pub fn logs(p: &Process, mut o: Options<'_>, out: &mut dyn Write) -> Result<(), CliError> {
    let path = p.dirs().request_log();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    if let Some(s) = o.since {
        o.filter.since =
            Some(parse_since(s, now).map_err(|e| CliError::usage(format!("--since: {e}")))?);
    }
    let io_err =
        |e: std::io::Error| CliError::runtime(format!("cannot read {}: {e}", path.display()));
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && o.follow => String::new(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(CliError::runtime(format!(
                "no request log at {} (is `log.requests` on, and has the gateway served anything?)",
                path.display()
            )))
        }
        Err(e) => return Err(io_err(e)),
    };
    let mut shown: Vec<RequestRecord> = crate::obs::read_records(&text)
        .into_iter()
        .filter(|r| o.filter.keeps(r))
        .collect();
    if let Some(n) = o.tail {
        let skip = shown.len().saturating_sub(n);
        shown.drain(..skip);
    }
    for r in &shown {
        if print(r, o.format, out).is_err() {
            return Ok(());
        }
    }
    let _ = out.flush();
    if o.follow {
        follow(&path, text.len() as u64, &o.filter, o.format, out);
    }
    Ok(())
}

/// Watches the log from byte `pos` until the output closes. A log that shrinks has been rotated:
/// reading starts again from the top of the new file.
fn follow(path: &Path, mut pos: u64, filter: &Filter, format: Format, out: &mut dyn Write) {
    let mut partial = String::new();
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let Ok(mut f) = std::fs::File::open(path) else {
            continue;
        };
        let len = f.metadata().map_or(0, |m| m.len());
        if len < pos {
            pos = 0;
            partial.clear();
        }
        if len == pos || f.seek(SeekFrom::Start(pos)).is_err() {
            continue;
        }
        let mut chunk = Vec::new();
        if f.take(len - pos).read_to_end(&mut chunk).is_err() {
            continue;
        }
        pos += chunk.len() as u64;
        partial.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(i) = partial.find('\n') {
            let line: String = partial.drain(..=i).collect();
            if let Ok(r) = serde_json::from_str::<RequestRecord>(line.trim_end()) {
                if filter.keeps(&r) && (print(&r, format, out).is_err() || out.flush().is_err()) {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(client: &str, status: u16, ts: &str, kind: RecordKind) -> RequestRecord {
        RequestRecord {
            client: client.into(),
            status,
            ts: ts.into(),
            kind,
            ..RequestRecord::default()
        }
    }

    #[test]
    fn filters_combine_and_default_to_requests_only() {
        let a = rec("ci", 503, "2026-10-14T10:00:00.000Z", RecordKind::Request);
        let at = rec("ci", 503, "2026-10-14T10:00:00.000Z", RecordKind::Attempt);
        let none = Filter::default();
        assert!(
            none.keeps(&a) && !none.keeps(&at),
            "attempts are hidden unless asked for"
        );
        assert!(Filter {
            all_kinds: true,
            ..Filter::default()
        }
        .keeps(&at));
        assert!(Filter {
            client: Some("ci".into()),
            ..Filter::default()
        }
        .keeps(&a));
        assert!(!Filter {
            client: Some("laptop".into()),
            ..Filter::default()
        }
        .keeps(&a));
        assert!(Filter {
            status: Some(StatusFilter::Class(5)),
            ..Filter::default()
        }
        .keeps(&a));
        assert!(!Filter {
            status: Some(StatusFilter::Exact(404)),
            ..Filter::default()
        }
        .keeps(&a));
        let since = parse_rfc3339("2026-10-14T10:00:00.000Z").unwrap();
        assert!(Filter {
            since: Some(since),
            ..Filter::default()
        }
        .keeps(&a));
        assert!(!Filter {
            since: Some(since + 1),
            ..Filter::default()
        }
        .keeps(&a));
        assert!(
            !Filter {
                since: Some(1),
                ..Filter::default()
            }
            .keeps(&rec("x", 200, "garbage", RecordKind::Request)),
            "a record with no readable time cannot satisfy --since"
        );
    }

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("airmate-logs-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn line(client: &str, status: u16) -> String {
        let r = rec(
            client,
            status,
            "2026-10-14T10:00:00.000Z",
            RecordKind::Request,
        );
        format!("{}\n", serde_json::to_string(&r).unwrap())
    }

    fn process(home: &Path) -> Process {
        let home = home.to_str().unwrap().to_owned();
        Process {
            env: Box::new(move |k| match k {
                "HOME" | "XDG_STATE_HOME" | "XDG_CONFIG_HOME" => Some(home.clone()),
                _ => None,
            }),
            platform: crate::config::paths::Platform::current(),
        }
    }

    fn options(follow: bool) -> Options<'static> {
        Options {
            filter: Filter::default(),
            format: Format::Oneline,
            follow,
            tail: None,
            since: None,
        }
    }

    /// Takes `ok` lines and reports a closed pipe on the last, as `head` does.
    struct Closing {
        ok: usize,
        got: String,
    }

    impl Write for Closing {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.got.push_str(&String::from_utf8_lossy(b));
            if b.ends_with(b"\n") {
                self.ok -= 1;
                if self.ok == 0 {
                    return Err(std::io::ErrorKind::BrokenPipe.into());
                }
            }
            Ok(b.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn logs_explains_a_missing_or_unreadable_log_and_a_bad_since() {
        let d = dir("errors");
        let p = process(&d);
        let e = logs(&p, options(false), &mut Vec::new()).unwrap_err();
        assert!(e.message.contains("no request log at"), "{}", e.message);

        let log = p.dirs().request_log();
        std::fs::create_dir_all(&log).unwrap();
        let e = logs(&p, options(false), &mut Vec::new()).unwrap_err();
        assert!(e.message.contains("cannot read"), "{}", e.message);

        let mut o = options(false);
        o.since = Some("yesterday-ish");
        let e = logs(&p, o, &mut Vec::new()).unwrap_err();
        assert!(e.message.starts_with("--since:"), "{}", e.message);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn logs_stops_quietly_when_the_reader_goes_away() {
        let d = dir("closed");
        let p = process(&d);
        let log = p.dirs().request_log();
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, format!("{}{}", line("a", 200), line("b", 200))).unwrap();
        let mut out = Closing {
            ok: 1,
            got: String::new(),
        };
        logs(&p, options(false), &mut out).unwrap();
        assert_eq!(out.got.lines().count(), 1, "stopped at the closed pipe");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn following_waits_for_a_log_that_does_not_exist_yet() {
        let d = dir("follow-new");
        let p = process(&d);
        let log = p.dirs().request_log();
        let path = log.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, line("late", 200)).unwrap();
        });
        let mut out = Closing {
            ok: 1,
            got: String::new(),
        };
        let mut o = options(true);
        o.filter.client = Some("late".into());
        logs(&p, o, &mut out).unwrap();
        writer.join().unwrap();
        assert!(out.got.contains("late"), "{}", out.got);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn following_prints_new_matching_records_whole_and_survives_rotation() {
        let d = dir("follow");
        let log = d.join("requests.jsonl");
        std::fs::write(&log, line("seen", 200)).unwrap();
        let initial = std::fs::metadata(&log).unwrap().len();
        let path = log.clone();
        let writer = std::thread::spawn(move || {
            let append = |text: &str| {
                use std::io::Write as _;
                let mut f = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap();
                f.write_all(text.as_bytes()).unwrap();
            };
            std::thread::sleep(Duration::from_millis(500));
            // Not wanted, not JSON, and half of a wanted record.
            append(&line("other", 200));
            append("this is not a record\n");
            let wanted = line("want", 503);
            let (a, b) = wanted.split_at(10);
            append(a);
            std::thread::sleep(Duration::from_millis(500));
            append(b);
            std::thread::sleep(Duration::from_millis(500));
            // Rotation: a new, shorter file.
            std::fs::write(&path, line("want", 404)).unwrap();
        });
        let mut out = Closing {
            ok: 2,
            got: String::new(),
        };
        let filter = Filter {
            client: Some("want".into()),
            ..Filter::default()
        };
        follow(&log, initial, &filter, Format::Json, &mut out);
        writer.join().unwrap();
        let records: Vec<RequestRecord> = out
            .got
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let statuses: Vec<u16> = records.iter().map(|r| r.status).collect();
        assert_eq!(statuses, [503, 404], "{}", out.got);
        let _ = std::fs::remove_dir_all(&d);
    }
}
