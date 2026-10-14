//! Prometheus metrics, hand-rolled in the text exposition format so the default build gains no
//! dependency. Counters and histograms are fed from the request log (see [`MeteredSink`]) and from
//! the router; gauges (circuit state, in-flight calls) are read from live state at scrape time.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use crate::obs::{RecordKind, RecordSink, RequestRecord};

/// More series than this under one metric name are dropped, so a hostile client cannot grow
/// memory by inventing label values (field names are the client's to choose).
const MAX_SERIES_PER_METRIC: usize = 1_000;

/// Histogram buckets in seconds.
const BUCKETS: [f64; 12] = [
    0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
];

#[derive(Debug, Clone, Default)]
struct Hist {
    counts: [u64; BUCKETS.len()],
    sum: f64,
    count: u64,
}

/// A recent failed request, for `airmate status`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RecentError {
    /// When.
    pub ts: String,
    /// The request id.
    pub request_id: String,
    /// The client.
    pub client: String,
    /// The target that answered last, if any.
    pub target: Option<String>,
    /// The status returned to the client.
    pub status: u16,
    /// The error class.
    pub error_class: Option<String>,
}

const RECENT_ERRORS: usize = 20;

#[derive(Default)]
struct Inner {
    recent: std::collections::VecDeque<RecentError>,
    ignored: BTreeMap<String, u64>,
    counters: BTreeMap<&'static str, BTreeMap<String, f64>>,
    hists: BTreeMap<&'static str, BTreeMap<String, Hist>>,
}

/// The metrics registry.
#[derive(Default)]
pub struct Metrics {
    inner: Mutex<Inner>,
}

/// The help text of every metric airmate exposes.
const HELP: &[(&str, &str, &str)] = &[
    (
        "airmate_requests_total",
        "counter",
        "Requests answered, by inbound protocol, client, final target and status.",
    ),
    (
        "airmate_request_duration_seconds",
        "histogram",
        "Time to answer a request, streams included.",
    ),
    (
        "airmate_ttfb_seconds",
        "histogram",
        "Time from request to the first byte from the upstream.",
    ),
    (
        "airmate_attempts_total",
        "counter",
        "Upstream attempts, by target and outcome.",
    ),
    (
        "airmate_failovers_total",
        "counter",
        "Times a request moved from one target to the next.",
    ),
    (
        "airmate_tokens_total",
        "counter",
        "Tokens, by target and kind (input, output, cached, reasoning).",
    ),
    (
        "airmate_cost_usd_total",
        "counter",
        "Estimated cost in US dollars of requests whose model has a price.",
    ),
    (
        "airmate_ignored_fields_total",
        "counter",
        "Request fields ignored by the field policy.",
    ),
    (
        "airmate_policy_rejections_total",
        "counter",
        "Requests refused by the field policy, by field.",
    ),
    (
        "airmate_circuit_state",
        "gauge",
        "Circuit breaker state per target: 0 closed, 1 half-open, 2 open.",
    ),
    (
        "airmate_inflight",
        "gauge",
        "Upstream calls awaiting a response, per target.",
    ),
];

/// A gauge read from live state: metric name, label pairs, value.
pub type Gauge = (&'static str, Vec<(&'static str, String)>, f64);

/// Escapes a label value.
fn escape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

fn labels(pairs: &[(&str, &str)]) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    let body: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", escape(if v.is_empty() { "-" } else { v })))
        .collect();
    body.join(",")
}

impl Metrics {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Adds to a counter.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    pub fn add(&self, name: &'static str, l: &[(&str, &str)], by: f64) {
        let mut i = self.inner.lock().expect("metrics lock");
        let series = i.counters.entry(name).or_default();
        let key = labels(l);
        if let Some(v) = series.get_mut(&key) {
            *v += by;
        } else if series.len() < MAX_SERIES_PER_METRIC {
            series.insert(key, by);
        }
    }

    /// Adds one to a counter.
    pub fn inc(&self, name: &'static str, l: &[(&str, &str)]) {
        self.add(name, l, 1.0);
    }

    /// Records an observation, in seconds.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    pub fn observe(&self, name: &'static str, l: &[(&str, &str)], seconds: f64) {
        let mut i = self.inner.lock().expect("metrics lock");
        let series = i.hists.entry(name).or_default();
        let key = labels(l);
        if !series.contains_key(&key) && series.len() >= MAX_SERIES_PER_METRIC {
            return;
        }
        let h = series.entry(key).or_default();
        for (n, le) in BUCKETS.iter().enumerate() {
            if seconds <= *le {
                h.counts[n] += 1;
            }
        }
        h.sum += seconds;
        h.count += 1;
    }

    /// The last few failed requests, newest first.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    #[must_use]
    pub fn recent_errors(&self) -> Vec<RecentError> {
        self.inner
            .lock()
            .expect("metrics lock")
            .recent
            .iter()
            .rev()
            .cloned()
            .collect()
    }

    /// How often each request field was ignored, so an operator sees which client features are
    /// being dropped.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    #[must_use]
    pub fn ignored_tally(&self) -> BTreeMap<String, u64> {
        self.inner.lock().expect("metrics lock").ignored.clone()
    }

    fn note_request(&self, r: &RequestRecord) {
        let mut i = self.inner.lock().expect("metrics lock");
        for f in &r.ignored {
            if i.ignored.contains_key(f) || i.ignored.len() < MAX_SERIES_PER_METRIC {
                *i.ignored.entry(f.clone()).or_default() += 1;
            }
        }
        if r.status >= 400 {
            if i.recent.len() == RECENT_ERRORS {
                i.recent.pop_front();
            }
            i.recent.push_back(RecentError {
                ts: r.ts.clone(),
                request_id: r.request_id.clone(),
                client: r.client.clone(),
                target: r.target.clone(),
                status: r.status,
                error_class: r.error_class.clone(),
            });
        }
    }

    /// A counter's value, for tests.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    #[must_use]
    pub fn counter(&self, name: &'static str, l: &[(&str, &str)]) -> f64 {
        self.inner
            .lock()
            .expect("metrics lock")
            .counters
            .get(name)
            .and_then(|s| s.get(&labels(l)))
            .copied()
            .unwrap_or(0.0)
    }

    /// Renders the exposition, followed by the `gauges` the caller read from live state as
    /// `(metric, label pairs, value)`.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    #[must_use]
    pub fn render(&self, gauges: &[Gauge]) -> String {
        let i = self.inner.lock().expect("metrics lock");
        let mut out = String::new();
        for (name, kind, help) in HELP {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} {kind}");
            match *kind {
                "counter" => {
                    for (l, v) in i.counters.get(name).into_iter().flatten() {
                        let _ = writeln!(out, "{name}{} {v}", braces(l));
                    }
                }
                "histogram" => {
                    for (l, h) in i.hists.get(name).into_iter().flatten() {
                        let sep = if l.is_empty() { "" } else { "," };
                        for (n, le) in BUCKETS.iter().enumerate() {
                            let _ = writeln!(
                                out,
                                "{name}_bucket{{{l}{sep}le=\"{le}\"}} {}",
                                h.counts[n]
                            );
                        }
                        let _ = writeln!(out, "{name}_bucket{{{l}{sep}le=\"+Inf\"}} {}", h.count);
                        let _ = writeln!(out, "{name}_sum{} {}", braces(l), h.sum);
                        let _ = writeln!(out, "{name}_count{} {}", braces(l), h.count);
                    }
                }
                _ => {
                    for (g, pairs, v) in gauges.iter().filter(|(g, _, _)| g == name) {
                        let pairs: Vec<(&str, &str)> =
                            pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
                        let _ = writeln!(out, "{g}{} {v}", braces(&labels(&pairs)));
                    }
                }
            }
        }
        out
    }
}

fn braces(l: &str) -> String {
    if l.is_empty() {
        String::new()
    } else {
        format!("{{{l}}}")
    }
}

/// How an attempt's HTTP status reads as an outcome label.
#[must_use]
pub fn outcome(status: u16) -> &'static str {
    match status {
        200..=399 => "ok",
        401 | 403 => "auth",
        408 | 504 => "timeout",
        429 => "rate_limited",
        400..=499 => "client_error",
        529 => "overloaded",
        _ => "server_error",
    }
}

/// A request-log sink that also feeds the metrics, then hands the record on.
pub struct MeteredSink {
    inner: Arc<dyn RecordSink>,
    metrics: Arc<Metrics>,
}

impl MeteredSink {
    /// Wraps `inner`.
    #[must_use]
    pub fn new(inner: Arc<dyn RecordSink>, metrics: Arc<Metrics>) -> Self {
        Self { inner, metrics }
    }
}

impl RecordSink for MeteredSink {
    fn record(&self, r: &RequestRecord) {
        let m = &self.metrics;
        let target = r.target.as_deref().unwrap_or("");
        match r.kind {
            RecordKind::Attempt => {
                m.inc(
                    "airmate_attempts_total",
                    &[("target", target), ("outcome", outcome(r.status))],
                );
            }
            RecordKind::Request => {
                m.inc(
                    "airmate_requests_total",
                    &[
                        ("inbound", &r.inbound),
                        ("client", &r.client),
                        ("target", target),
                        ("status", &r.status.to_string()),
                    ],
                );
                m.observe(
                    "airmate_request_duration_seconds",
                    &[("inbound", &r.inbound)],
                    r.total_ms as f64 / 1000.0,
                );
                if let Some(t) = r.ttfb_ms {
                    m.observe(
                        "airmate_ttfb_seconds",
                        &[("target", target)],
                        t as f64 / 1000.0,
                    );
                }
                for (kind, n) in [
                    ("input", r.input_tokens),
                    ("output", r.output_tokens),
                    ("cached", r.cached_tokens),
                    ("reasoning", r.reasoning_tokens),
                ] {
                    if n > 0 {
                        m.add(
                            "airmate_tokens_total",
                            &[("target", target), ("kind", kind)],
                            n as f64,
                        );
                    }
                }
                if let Some(c) = r.est_cost_usd {
                    m.add(
                        "airmate_cost_usd_total",
                        &[("client", &r.client), ("target", target)],
                        c,
                    );
                }
                for f in &r.ignored {
                    m.inc("airmate_ignored_fields_total", &[("field", f)]);
                }
                m.note_request(r);
                if let Some(f) = &r.rejected {
                    m.inc("airmate_policy_rejections_total", &[("field", f)]);
                }
            }
            RecordKind::Classifier => {}
        }
        self.inner.record(r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::NullSink;

    fn sink() -> (MeteredSink, Arc<Metrics>) {
        let m = Metrics::new();
        (MeteredSink::new(Arc::new(NullSink), m.clone()), m)
    }

    #[test]
    fn counters_and_labels_render_in_the_text_format() {
        let m = Metrics::new();
        m.inc(
            "airmate_attempts_total",
            &[("target", "up/m"), ("outcome", "ok")],
        );
        m.inc(
            "airmate_attempts_total",
            &[("target", "up/m"), ("outcome", "ok")],
        );
        m.inc(
            "airmate_attempts_total",
            &[("target", "up/m"), ("outcome", "auth")],
        );
        let text = m.render(&[]);
        assert!(text.contains("# TYPE airmate_attempts_total counter"));
        assert!(
            text.contains("airmate_attempts_total{target=\"up/m\",outcome=\"ok\"} 2"),
            "{text}"
        );
        assert!(text.contains("airmate_attempts_total{target=\"up/m\",outcome=\"auth\"} 1"));
        assert_eq!(
            m.counter(
                "airmate_attempts_total",
                &[("target", "up/m"), ("outcome", "ok")]
            ),
            2.0
        );
    }

    #[test]
    fn label_values_are_escaped_and_empty_ones_are_dashes() {
        let m = Metrics::new();
        m.inc("airmate_ignored_fields_total", &[("field", "a\"b\\c\nd")]);
        m.inc("airmate_ignored_fields_total", &[("field", "")]);
        let text = m.render(&[]);
        assert!(text.contains(r#"field="a\"b\\c\nd""#), "{text}");
        assert!(text.contains(r#"field="-""#));
        assert_eq!(text.lines().filter(|l| l.contains('\n')).count(), 0);
    }

    #[test]
    fn histograms_are_cumulative_with_sum_and_count() {
        let m = Metrics::new();
        m.observe("airmate_ttfb_seconds", &[("target", "t")], 0.07);
        m.observe("airmate_ttfb_seconds", &[("target", "t")], 3.0);
        let text = m.render(&[]);
        assert!(
            text.contains("airmate_ttfb_seconds_bucket{target=\"t\",le=\"0.05\"} 0"),
            "{text}"
        );
        assert!(text.contains("airmate_ttfb_seconds_bucket{target=\"t\",le=\"0.1\"} 1"));
        assert!(text.contains("airmate_ttfb_seconds_bucket{target=\"t\",le=\"5\"} 2"));
        assert!(text.contains("airmate_ttfb_seconds_bucket{target=\"t\",le=\"+Inf\"} 2"));
        assert!(text.contains("airmate_ttfb_seconds_count{target=\"t\"} 2"));
        assert!(text.contains("airmate_ttfb_seconds_sum{target=\"t\"} 3.07"));
    }

    #[test]
    fn gauges_come_from_the_caller() {
        let text = Metrics::new().render(&[(
            "airmate_circuit_state",
            vec![("target", "up/m".to_owned())],
            2.0,
        )]);
        assert!(
            text.contains("airmate_circuit_state{target=\"up/m\"} 2"),
            "{text}"
        );
    }

    #[test]
    fn invented_label_values_cannot_grow_memory_without_bound() {
        let m = Metrics::new();
        for n in 0..(MAX_SERIES_PER_METRIC + 500) {
            m.inc(
                "airmate_ignored_fields_total",
                &[("field", &format!("f{n}"))],
            );
        }
        let text = m.render(&[]);
        assert_eq!(
            text.lines()
                .filter(|l| l.starts_with("airmate_ignored_fields_total{"))
                .count(),
            MAX_SERIES_PER_METRIC
        );
    }

    #[test]
    fn records_feed_the_documented_series() {
        let (s, m) = sink();
        s.record(&RequestRecord {
            kind: RecordKind::Attempt,
            target: Some("up/m".into()),
            status: 503,
            ..RequestRecord::default()
        });
        s.record(&RequestRecord {
            kind: RecordKind::Request,
            inbound: "anthropic".into(),
            client: "ci".into(),
            target: Some("up/m".into()),
            status: 200,
            total_ms: 1500,
            ttfb_ms: Some(200),
            input_tokens: 10,
            output_tokens: 4,
            est_cost_usd: Some(0.25),
            ignored: vec!["top_k".into()],
            ..RequestRecord::default()
        });
        s.record(&RequestRecord {
            kind: RecordKind::Request,
            status: 400,
            rejected: Some("mcp_servers".into()),
            ..RequestRecord::default()
        });
        s.record(&RequestRecord {
            kind: RecordKind::Classifier,
            ..RequestRecord::default()
        });
        assert_eq!(
            m.counter(
                "airmate_attempts_total",
                &[("target", "up/m"), ("outcome", "server_error")]
            ),
            1.0
        );
        assert_eq!(
            m.counter(
                "airmate_requests_total",
                &[
                    ("inbound", "anthropic"),
                    ("client", "ci"),
                    ("target", "up/m"),
                    ("status", "200")
                ]
            ),
            1.0
        );
        assert_eq!(
            m.counter(
                "airmate_tokens_total",
                &[("target", "up/m"), ("kind", "input")]
            ),
            10.0
        );
        assert_eq!(
            m.counter(
                "airmate_tokens_total",
                &[("target", "up/m"), ("kind", "cached")]
            ),
            0.0
        );
        assert_eq!(
            m.counter(
                "airmate_cost_usd_total",
                &[("client", "ci"), ("target", "up/m")]
            ),
            0.25
        );
        assert_eq!(
            m.counter("airmate_ignored_fields_total", &[("field", "top_k")]),
            1.0
        );
        assert_eq!(
            m.counter(
                "airmate_policy_rejections_total",
                &[("field", "mcp_servers")]
            ),
            1.0
        );
        let text = m.render(&[]);
        assert!(
            text.contains("airmate_request_duration_seconds_count{inbound=\"anthropic\"} 1"),
            "{text}"
        );
    }

    #[test]
    fn failures_and_dropped_fields_are_remembered_for_status() {
        let (s, m) = sink();
        for n in 0..25 {
            s.record(&RequestRecord {
                kind: RecordKind::Request,
                status: 500,
                request_id: format!("r{n}"),
                ignored: vec!["top_k".into()],
                ..RequestRecord::default()
            });
        }
        s.record(&RequestRecord {
            kind: RecordKind::Request,
            status: 200,
            request_id: "ok".into(),
            ..RequestRecord::default()
        });
        let recent = m.recent_errors();
        assert_eq!(recent.len(), 20, "only the last few are kept");
        assert_eq!(recent[0].request_id, "r24", "newest first");
        assert!(recent.iter().all(|e| e.status == 500));
        assert_eq!(m.ignored_tally().get("top_k"), Some(&25));
    }

    #[test]
    fn statuses_read_as_outcomes() {
        for (s, o) in [
            (200, "ok"),
            (401, "auth"),
            (403, "auth"),
            (429, "rate_limited"),
            (504, "timeout"),
            (400, "client_error"),
            (529, "overloaded"),
            (500, "server_error"),
            (502, "server_error"),
        ] {
            assert_eq!(outcome(s), o, "{s}");
        }
    }
}
