//! The usage ledger: what each request used and what it cost.
//!
//! `usage.jsonl` is separate from the request log. It is written only when `usage.ledger` is
//! on, holds no content, and is what budgets and `airmate usage` are computed from. Cost is
//! `null` when the model has no price: unknown is not zero.

pub mod budget;
pub mod prices;

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::obs::JsonlLog;

pub use budget::{Budgets, Exhausted, Period};
pub use prices::{cost_usd, PriceTable};

/// One ledger line: a request that reached an upstream and was answered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct UsageRecord {
    /// RFC 3339 UTC timestamp.
    pub ts: String,
    /// The same instant in Unix seconds.
    pub unix: u64,
    /// The request id.
    pub request_id: String,
    /// The client name.
    pub client: String,
    /// The model the client asked for.
    pub model_asked: String,
    /// `upstream/model` that answered.
    pub target: String,
    /// The upstream model name.
    pub upstream_model: String,
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Cached input tokens.
    pub cached_tokens: u64,
    /// Reasoning tokens, a subset of the output tokens.
    pub reasoning_tokens: u64,
    /// Cost in US dollars; `None` when there is no price.
    pub cost_usd: Option<f64>,
}

/// Receives ledger lines.
pub trait UsageSink: Send + Sync {
    /// Records one request's usage.
    fn record(&self, rec: &UsageRecord);
}

/// Discards usage.
pub struct NullUsage;

impl UsageSink for NullUsage {
    fn record(&self, _: &UsageRecord) {}
}

impl UsageSink for JsonlLog {
    fn record(&self, rec: &UsageRecord) {
        self.append(rec);
    }
}

/// Reads ledger lines, skipping any that are not valid.
#[must_use]
pub fn read_records(text: &str) -> Vec<UsageRecord> {
    text.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Reads the ledger at `path`; a missing file is an empty ledger.
///
/// # Errors
///
/// The file exists but cannot be read.
pub fn read_file(path: &Path) -> Result<Vec<UsageRecord>, String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(read_records(&t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!(
            "cannot read the usage ledger {}: {e}",
            path.display()
        )),
    }
}

/// What `usage` groups by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    /// The client name.
    Client,
    /// The upstream model.
    Model,
    /// The upstream.
    Upstream,
    /// The UTC day.
    Day,
}

/// One aggregated row.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Row {
    /// The group.
    pub key: String,
    /// Requests.
    pub requests: u64,
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Cached input tokens.
    pub cached_tokens: u64,
    /// Cost of the priced requests, in US dollars; `None` when none were priced.
    pub cost_usd: Option<f64>,
    /// Requests with no price, whose cost is in no total.
    pub unpriced_requests: u64,
}

/// Groups the records at or after `since` (Unix seconds).
#[must_use]
pub fn aggregate(records: &[UsageRecord], by: GroupBy, since: Option<u64>) -> Vec<Row> {
    let mut rows: BTreeMap<String, Row> = BTreeMap::new();
    for r in records.iter().filter(|r| since.is_none_or(|s| r.unix >= s)) {
        let key = match by {
            GroupBy::Client => r.client.clone(),
            GroupBy::Model => {
                if r.upstream_model.is_empty() {
                    r.model_asked.clone()
                } else {
                    r.upstream_model.clone()
                }
            }
            GroupBy::Upstream => r
                .target
                .split_once('/')
                .map_or(r.target.clone(), |(u, _)| u.to_owned()),
            GroupBy::Day => r.ts.chars().take(10).collect(),
        };
        let row = rows.entry(key.clone()).or_insert_with(|| Row {
            key,
            requests: 0,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            cost_usd: None,
            unpriced_requests: 0,
        });
        row.requests += 1;
        row.input_tokens += r.input_tokens;
        row.output_tokens += r.output_tokens;
        row.cached_tokens += r.cached_tokens;
        match r.cost_usd {
            Some(c) => row.cost_usd = Some(row.cost_usd.unwrap_or(0.0) + c),
            None => row.unpriced_requests += 1,
        }
    }
    rows.into_values().collect()
}

/// Parses `--since`: a span back from `now` (`90s`, `15m`, `24h`, `7d`, `2w`) or a UTC date
/// (`2026-10-01`).
///
/// # Errors
///
/// Anything else.
pub fn parse_since(s: &str, now: u64) -> Result<u64, String> {
    let bad =
        || format!("`{s}` is not a time span like `24h` or `7d`, or a date like `2026-10-01`");
    if let Some((y, rest)) = s.split_once('-') {
        let (m, d) = rest.split_once('-').ok_or_else(bad)?;
        let (y, m, d): (i64, i64, i64) = (
            y.parse().map_err(|_| bad())?,
            m.parse().map_err(|_| bad())?,
            d.parse().map_err(|_| bad())?,
        );
        if !(1..=12).contains(&m) || !(1..=31).contains(&d) || !(1970..=9999).contains(&y) {
            return Err(bad());
        }
        return Ok(u64::try_from(budget::days_from_civil(y, m, d)).unwrap_or(0) * 86_400);
    }
    let unit = s.chars().last().ok_or_else(bad)?;
    let n: u64 = s[..s.len() - unit.len_utf8()].parse().map_err(|_| bad())?;
    let secs = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3_600,
        'd' => 86_400,
        'w' => 7 * 86_400,
        _ => return Err(bad()),
    };
    Ok(now.saturating_sub(n.saturating_mul(secs)))
}

/// Renders rows as an aligned table.
#[must_use]
pub fn render_table(by: GroupBy, rows: &[Row]) -> String {
    let head = match by {
        GroupBy::Client => "CLIENT",
        GroupBy::Model => "MODEL",
        GroupBy::Upstream => "UPSTREAM",
        GroupBy::Day => "DAY",
    };
    let cost = |r: &Row| match (r.cost_usd, r.unpriced_requests) {
        (None, _) => "unknown".to_owned(),
        (Some(c), 0) => format!("${c:.4}"),
        (Some(c), n) => format!("${c:.4} (+{n} unpriced)"),
    };
    let mut table: Vec<[String; 6]> =
        vec![[head, "REQUESTS", "INPUT", "OUTPUT", "CACHED", "COST"].map(str::to_owned)];
    for r in rows {
        table.push([
            r.key.clone(),
            r.requests.to_string(),
            r.input_tokens.to_string(),
            r.output_tokens.to_string(),
            r.cached_tokens.to_string(),
            cost(r),
        ]);
    }
    let widths: Vec<usize> = (0..6)
        .map(|i| {
            table
                .iter()
                .map(|r| r[i].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for r in &table {
        let line: Vec<String> = r
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i == 0 || i == 5 {
                    format!("{c:<w$}", w = widths[i])
                } else {
                    format!("{c:>w$}", w = widths[i])
                }
            })
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(
        client: &str,
        target: &str,
        model: &str,
        ts: &str,
        unix: u64,
        cost: Option<f64>,
    ) -> UsageRecord {
        UsageRecord {
            ts: ts.into(),
            unix,
            client: client.into(),
            target: target.into(),
            upstream_model: model.into(),
            input_tokens: 100,
            output_tokens: 10,
            cached_tokens: 5,
            cost_usd: cost,
            ..UsageRecord::default()
        }
    }

    fn sample() -> Vec<UsageRecord> {
        vec![
            rec(
                "ci",
                "fw/llama",
                "llama",
                "2026-10-13T10:00:00.000Z",
                1_791_885_600,
                Some(0.5),
            ),
            rec(
                "ci",
                "fw/llama",
                "llama",
                "2026-10-14T10:00:00.000Z",
                1_791_972_000,
                None,
            ),
            rec(
                "laptop",
                "oa/gpt",
                "gpt",
                "2026-10-14T11:00:00.000Z",
                1_791_975_600,
                Some(2.0),
            ),
        ]
    }

    #[test]
    fn rows_group_by_each_key_and_keep_unpriced_cost_unknown() {
        let by_client = aggregate(&sample(), GroupBy::Client, None);
        assert_eq!(by_client.len(), 2);
        let ci = &by_client[0];
        assert_eq!(
            (
                ci.key.as_str(),
                ci.requests,
                ci.input_tokens,
                ci.unpriced_requests
            ),
            ("ci", 2, 200, 1)
        );
        assert_eq!(
            ci.cost_usd,
            Some(0.5),
            "the unpriced request is not counted as zero"
        );
        assert_eq!(
            aggregate(&sample(), GroupBy::Upstream, None)
                .iter()
                .map(|r| r.key.as_str())
                .collect::<Vec<_>>(),
            ["fw", "oa"]
        );
        assert_eq!(
            aggregate(&sample(), GroupBy::Model, None)
                .iter()
                .map(|r| r.key.as_str())
                .collect::<Vec<_>>(),
            ["gpt", "llama"]
        );
        let days = aggregate(&sample(), GroupBy::Day, None);
        assert_eq!(
            days.iter()
                .map(|r| (r.key.as_str(), r.requests))
                .collect::<Vec<_>>(),
            [("2026-10-13", 1), ("2026-10-14", 2)]
        );
        let none_priced = aggregate(&sample()[1..2], GroupBy::Client, None);
        assert_eq!(none_priced[0].cost_usd, None);
    }

    #[test]
    fn since_filters_by_time() {
        assert_eq!(
            aggregate(&sample(), GroupBy::Client, Some(1_791_972_000))
                .iter()
                .map(|r| r.requests)
                .sum::<u64>(),
            2
        );
        assert!(aggregate(&sample(), GroupBy::Client, Some(u64::MAX)).is_empty());
    }

    #[test]
    fn since_accepts_spans_and_dates() {
        assert_eq!(parse_since("90s", 1000), Ok(910));
        assert_eq!(parse_since("2h", 10_000), Ok(2_800));
        assert_eq!(parse_since("1d", 100_000), Ok(13_600));
        assert_eq!(parse_since("1w", 1_000_000), Ok(395_200));
        assert_eq!(parse_since("30d", 5), Ok(0), "never before the epoch");
        assert_eq!(parse_since("1970-01-02", 0), Ok(86_400));
        assert_eq!(parse_since("2000-02-29", 0), Ok(951_782_400));
        for bad in ["", "d", "x7", "7x", "2026-13-01", "2026-10", "1.5h", "-3d"] {
            assert!(parse_since(bad, 100).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_table_is_aligned_and_says_when_cost_is_unknown() {
        let t = render_table(
            GroupBy::Client,
            &aggregate(&sample(), GroupBy::Client, None),
        );
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("CLIENT"));
        assert!(lines[1].contains("$0.5000 (+1 unpriced)"), "{t}");
        assert!(lines[2].contains("$2.0000") && !lines[2].contains("unpriced"));
        let unknown = render_table(
            GroupBy::Day,
            &aggregate(&sample()[1..2], GroupBy::Day, None),
        );
        assert!(unknown.contains("unknown"));
    }

    #[test]
    fn the_ledger_round_trips_and_tolerates_junk() {
        let line = serde_json::to_string(&sample()[0]).unwrap();
        let text = format!("{line}\nnot json\n{{\"client\":\"x\",\"future_field\":1}}\n\n");
        let back = read_records(&text);
        assert_eq!(back.len(), 2);
        assert_eq!(back[0], sample()[0]);
        assert_eq!(back[1].client, "x");
        assert!(read_file(Path::new("/definitely/not/here.jsonl"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn budgets_rebuild_from_the_ledger_skipping_unpriced_requests() {
        let day = 86_400;
        let t = 1_791_972_000 - 1_791_972_000 % day + 3_600;
        let records = vec![
            rec("ci", "a/b", "b", "", t, Some(1.0)),
            rec("ci", "a/b", "b", "", t + 5, None),
            rec("ci", "a/b", "b", "", t + 10, Some(0.25)),
        ];
        let b = Budgets::rebuild(&records);
        assert!((b.spent("ci", Period::Day, t) - 1.25).abs() < 1e-9);
    }
}
