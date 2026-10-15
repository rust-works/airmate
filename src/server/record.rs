//! Building request-log records as a request is served.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use airmate_core::ir::{Protocol, Usage};
use airmate_core::policy::Ignored;

use super::stream::StreamSummary;
use crate::obs::{now, RecordKind, RecordSink, RequestRecord};
use crate::router::RouteTarget;

struct State {
    sink: Arc<dyn RecordSink>,
    start: Instant,
    first_byte_ms: Option<u64>,
    rec: RequestRecord,
    deferred: bool,
    finished: bool,
}

/// A handle that accumulates facts about a request and writes its records.
#[derive(Clone)]
pub struct Recorder(Arc<Mutex<State>>);

/// The error class for an HTTP status, as the Anthropic envelope names it.
#[must_use]
pub fn class_for_status(status: u16) -> Option<&'static str> {
    Some(match status {
        400 | 422 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        504 => "timeout_error",
        529 => "overloaded_error",
        s if s >= 500 => "api_error",
        s if s >= 400 => "invalid_request_error",
        _ => return None,
    })
}

impl Recorder {
    /// A recorder for one inbound request.
    #[must_use]
    pub fn new(
        sink: Arc<dyn RecordSink>,
        request_id: &str,
        client: &str,
        inbound: Protocol,
    ) -> Self {
        let rec = RequestRecord {
            kind: RecordKind::Request,
            request_id: request_id.to_owned(),
            client: client.to_owned(),
            inbound: inbound.as_str().to_owned(),
            ..RequestRecord::default()
        };
        Self(Arc::new(Mutex::new(State {
            sink,
            start: Instant::now(),
            first_byte_ms: None,
            rec,
            deferred: false,
            finished: false,
        })))
    }

    fn with(&self, f: impl FnOnce(&mut State)) {
        if let Ok(mut s) = self.0.lock() {
            f(&mut s);
        }
    }

    /// The model the client asked for.
    pub fn model_asked(&self, model: &str) {
        self.with(|s| s.rec.model_asked = model.to_owned());
    }

    /// Whether the request streams.
    pub fn stream(&self, stream: bool) {
        self.with(|s| s.rec.stream = stream);
    }

    /// The rule that selected the model entry.
    pub fn rule(&self, rule: Option<&str>) {
        self.with(|s| s.rec.rule = rule.map(str::to_owned));
    }

    /// Targets the router skipped.
    pub fn skipped(&self, skipped: &[String]) {
        self.with(|s| s.rec.skipped = skipped.to_vec());
    }

    /// Parts ignored by policy.
    pub fn ignored(&self, ignored: &[Ignored]) {
        self.with(|s| s.rec.ignored = ignored.iter().map(|i| i.name.clone()).collect());
    }

    /// The first byte arrived from the upstream.
    pub fn first_byte(&self) {
        self.with(|s| {
            if s.first_byte_ms.is_none() {
                s.first_byte_ms =
                    Some(u64::try_from(s.start.elapsed().as_millis()).unwrap_or(u64::MAX));
            }
        });
    }

    /// Token usage.
    pub fn usage(&self, u: &Usage) {
        self.with(|s| {
            s.rec.input_tokens = u.input_tokens;
            s.rec.output_tokens = u.output_tokens;
            s.rec.cached_tokens = u.cache_read_input_tokens;
            s.rec.reasoning_tokens = u.reasoning_tokens;
        });
    }

    /// The field the field policy refused the request for.
    pub fn rejected(&self, field: &str) {
        self.with(|s| s.rec.rejected = Some(field.to_owned()));
    }

    /// The estimated cost.
    pub fn cost(&self, usd: Option<f64>) {
        self.with(|s| s.rec.est_cost_usd = usd);
    }

    /// Writes the record for one upstream attempt and counts it.
    pub fn attempt(&self, target: &RouteTarget, status: u16, ttfb_ms: Option<u64>) {
        self.with(|s| {
            s.rec.attempts += 1;
            s.rec.target = Some(target.label());
            s.rec.upstream_model = Some(target.model.clone());
            let a = RequestRecord {
                kind: RecordKind::Attempt,
                ts: now(),
                target: Some(target.label()),
                upstream_model: Some(target.model.clone()),
                status,
                error_class: class_for_status(status).map(str::to_owned),
                attempts: s.rec.attempts,
                ttfb_ms,
                total_ms: u64::try_from(s.start.elapsed().as_millis()).unwrap_or(u64::MAX),
                ..s.rec.clone()
            };
            s.sink.record(&a);
        });
    }

    /// The record will be written when the stream ends.
    pub fn defer(&self) {
        self.with(|s| s.deferred = true);
    }

    fn write(s: &mut State, status: u16, class: Option<&str>) {
        if s.finished {
            return;
        }
        s.finished = true;
        s.rec.kind = RecordKind::Request;
        s.rec.ts = now();
        s.rec.status = status;
        s.rec.error_class = class.map(str::to_owned);
        s.rec.ttfb_ms = s.first_byte_ms;
        s.rec.total_ms = u64::try_from(s.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        let rec = s.rec.clone();
        s.sink.record(&rec);
    }

    /// Writes the request record for a response that is not deferred.
    pub fn finish_response(&self, status: u16) {
        self.with(|s| {
            if !s.deferred {
                Self::write(s, status, class_for_status(status));
            }
        });
    }

    /// Writes the request record when a stream ends.
    pub fn finish_stream(&self, summary: &StreamSummary) {
        self.with(|s| {
            s.rec.output_tokens = summary.usage.output_tokens;
            s.rec.input_tokens = summary.usage.input_tokens;
            s.rec.cached_tokens = summary.usage.cache_read_input_tokens;
            s.rec.reasoning_tokens = summary.usage.reasoning_tokens;
            let class = if summary.cancelled {
                Some("client_closed_request")
            } else if summary.error.is_some() || !summary.completed {
                Some("stream_error")
            } else {
                None
            };
            Self::write(s, 200, class);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Capture(Mutex<Vec<RequestRecord>>);

    impl RecordSink for Capture {
        fn record(&self, rec: &RequestRecord) {
            self.0.lock().unwrap().push(rec.clone());
        }
    }

    fn recorder() -> (Recorder, Arc<Capture>) {
        let cap = Arc::new(Capture::default());
        (
            Recorder::new(cap.clone(), "req_1", "ci", Protocol::Anthropic),
            cap,
        )
    }

    #[test]
    fn statuses_map_to_error_classes() {
        for (s, want) in [
            (200, None),
            (400, Some("invalid_request_error")),
            (401, Some("authentication_error")),
            (403, Some("permission_error")),
            (404, Some("not_found_error")),
            (413, Some("request_too_large")),
            (429, Some("rate_limit_error")),
            (500, Some("api_error")),
            (502, Some("api_error")),
            (504, Some("timeout_error")),
            (529, Some("overloaded_error")),
            (418, Some("invalid_request_error")),
        ] {
            assert_eq!(class_for_status(s), want, "{s}");
        }
    }

    #[test]
    fn a_response_writes_one_request_record_with_what_was_noted() {
        let (r, cap) = recorder();
        r.model_asked("claude-x");
        r.stream(false);
        r.ignored(&[Ignored {
            name: "metadata".into(),
            reason: String::new(),
        }]);
        r.usage(&Usage {
            input_tokens: 5,
            output_tokens: 6,
            cache_read_input_tokens: 1,
            reasoning_tokens: 2,
            ..Usage::default()
        });
        r.first_byte();
        r.finish_response(200);
        r.finish_response(500);
        let recs = cap.0.lock().unwrap();
        assert_eq!(recs.len(), 1, "a request is recorded once");
        let rec = &recs[0];
        assert_eq!(
            (
                rec.request_id.as_str(),
                rec.client.as_str(),
                rec.inbound.as_str(),
                rec.model_asked.as_str()
            ),
            ("req_1", "ci", "anthropic", "claude-x")
        );
        assert_eq!(
            (
                rec.status,
                rec.input_tokens,
                rec.output_tokens,
                rec.cached_tokens,
                rec.reasoning_tokens
            ),
            (200, 5, 6, 1, 2)
        );
        assert_eq!(rec.ignored, ["metadata"]);
        assert!(
            rec.ttfb_ms.is_some() && rec.error_class.is_none() && rec.kind == RecordKind::Request
        );
        assert!(rec.ts.ends_with('Z'));
    }

    #[test]
    fn errors_carry_their_class() {
        let (r, cap) = recorder();
        r.finish_response(429);
        assert_eq!(
            cap.0.lock().unwrap()[0].error_class.as_deref(),
            Some("rate_limit_error")
        );
    }

    #[test]
    fn a_deferred_record_waits_for_the_stream() {
        let (r, cap) = recorder();
        r.stream(true);
        r.defer();
        r.finish_response(200);
        assert!(cap.0.lock().unwrap().is_empty());
        r.finish_stream(&StreamSummary {
            usage: Usage {
                output_tokens: 9,
                ..Usage::default()
            },
            completed: true,
            ..StreamSummary::default()
        });
        let recs = cap.0.lock().unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(
            (
                recs[0].status,
                recs[0].output_tokens,
                recs[0].error_class.clone()
            ),
            (200, 9, None)
        );
    }

    #[test]
    fn cancelled_and_broken_streams_are_classed() {
        let (r, cap) = recorder();
        r.defer();
        r.finish_stream(&StreamSummary {
            cancelled: true,
            ..StreamSummary::default()
        });
        assert_eq!(
            cap.0.lock().unwrap()[0].error_class.as_deref(),
            Some("client_closed_request")
        );
        let (r, cap) = recorder();
        r.defer();
        r.finish_stream(&StreamSummary {
            error: Some("boom".into()),
            ..StreamSummary::default()
        });
        assert_eq!(
            cap.0.lock().unwrap()[0].error_class.as_deref(),
            Some("stream_error")
        );
        let (r, cap) = recorder();
        r.defer();
        r.finish_stream(&StreamSummary::default());
        assert_eq!(
            cap.0.lock().unwrap()[0].error_class.as_deref(),
            Some("stream_error"),
            "never reaching the end is an error"
        );
    }
}
