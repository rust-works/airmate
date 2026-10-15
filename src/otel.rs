//! OpenTelemetry trace export, behind the `otel` feature.
//!
//! One span per request, sent as OTLP over HTTP with JSON bodies (`POST <endpoint>/v1/traces`).
//! The spans are built by hand from the request log's records and sent with the gateway's own HTTP
//! client, so the feature adds no crate. Export is best effort: spans queue in a bounded channel
//! and are dropped when the collector cannot keep up, and a failing collector never slows or fails
//! a request.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{Method, Request};
use serde_json::{json, Value as Json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::obs::{parse_rfc3339_millis, RecordKind, RecordSink, RequestRecord};
use crate::server::upstream::{build_client, HttpClient};

/// Spans waiting to be sent; more than this are dropped.
const QUEUE: usize = 2_048;
/// Spans per export request.
const BATCH: usize = 128;

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// A trace id derived from the request id, so the spans of one request share a trace and the
/// ids are the same on every run.
#[must_use]
pub fn trace_id(request_id: &str) -> String {
    hex(&Sha256::digest(request_id.as_bytes())[..16])
}

/// A span id derived from the request id.
#[must_use]
pub fn span_id(request_id: &str) -> String {
    hex(&Sha256::digest(format!("span:{request_id}").as_bytes())[..8])
}

fn attr(key: &str, value: Json) -> Json {
    json!({"key": key, "value": value})
}

fn string(v: &str) -> Json {
    json!({"stringValue": v})
}

fn int(v: u64) -> Json {
    // OTLP/JSON carries 64-bit integers as strings.
    json!({"intValue": v.to_string()})
}

/// The span for one finished request, or `None` for records that are not requests.
#[must_use]
pub fn span(r: &RequestRecord) -> Option<Json> {
    if r.kind != RecordKind::Request {
        return None;
    }
    let end_ms = parse_rfc3339_millis(&r.ts)?;
    let start_ms = end_ms.saturating_sub(r.total_ms);
    let mut attrs = vec![
        attr("airmate.request_id", string(&r.request_id)),
        attr("airmate.client", string(&r.client)),
        attr("airmate.inbound", string(&r.inbound)),
        attr("airmate.model_asked", string(&r.model_asked)),
        attr("http.response.status_code", int(u64::from(r.status))),
        attr("airmate.stream", json!({"boolValue": r.stream})),
        attr("airmate.attempts", int(u64::from(r.attempts))),
        attr("gen_ai.usage.input_tokens", int(r.input_tokens)),
        attr("gen_ai.usage.output_tokens", int(r.output_tokens)),
    ];
    if let Some(t) = &r.target {
        attrs.push(attr("airmate.target", string(t)));
    }
    if let Some(m) = &r.upstream_model {
        attrs.push(attr("gen_ai.request.model", string(m)));
    }
    if let Some(t) = r.ttfb_ms {
        attrs.push(attr("airmate.ttfb_ms", int(t)));
    }
    if let Some(c) = r.est_cost_usd {
        attrs.push(attr("airmate.cost_usd", json!({"doubleValue": c})));
    }
    if let Some(rule) = &r.rule {
        attrs.push(attr("airmate.rule", string(rule)));
    }
    if let Some(e) = &r.error_class {
        attrs.push(attr("error.type", string(e)));
    }
    // 1xx-3xx and a stream that finished are fine; a server error or a broken stream is an error.
    let failed = r.status >= 500 || r.error_class.as_deref() == Some("stream_error");
    let status = if failed {
        json!({"code": 2, "message": r.error_class.clone().unwrap_or_else(|| format!("HTTP {}", r.status))})
    } else {
        json!({"code": 1})
    };
    Some(json!({
        "traceId": trace_id(&r.request_id),
        "spanId": span_id(&r.request_id),
        "name": format!("airmate {}", if r.inbound.is_empty() { "request" } else { &r.inbound }),
        "kind": 2,
        "startTimeUnixNano": (start_ms * 1_000_000).to_string(),
        "endTimeUnixNano": (end_ms * 1_000_000).to_string(),
        "attributes": attrs,
        "status": status,
    }))
}

/// The export request body for `spans`.
#[must_use]
pub fn export_body(service_name: &str, spans: &[Json]) -> Json {
    json!({"resourceSpans": [{
        "resource": {"attributes": [
            attr("service.name", string(service_name)),
            attr("service.version", string(env!("CARGO_PKG_VERSION"))),
        ]},
        "scopeSpans": [{"scope": {"name": "airmate"}, "spans": spans}],
    }]})
}

/// A request-log sink that exports spans and passes every record on.
pub struct OtelSink {
    inner: Arc<dyn RecordSink>,
    tx: mpsc::Sender<Json>,
}

impl OtelSink {
    /// Starts the exporter on the current runtime. `headers` are sent with every export request.
    #[must_use]
    pub fn start(
        inner: Arc<dyn RecordSink>,
        endpoint: &str,
        service_name: &str,
        headers: Vec<(String, String)>,
        flush: Duration,
    ) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(QUEUE);
        let url = format!("{}/v1/traces", endpoint.trim_end_matches('/'));
        tokio::spawn(export_loop(
            rx,
            url,
            service_name.to_owned(),
            headers,
            flush,
        ));
        Arc::new(Self { inner, tx })
    }
}

impl RecordSink for OtelSink {
    fn record(&self, rec: &RequestRecord) {
        if let Some(s) = span(rec) {
            // A full queue means the collector is behind: drop the span, never block a request.
            let _ = self.tx.try_send(s);
        }
        self.inner.record(rec);
    }
}

async fn export_loop(
    mut rx: mpsc::Receiver<Json>,
    url: String,
    service: String,
    headers: Vec<(String, String)>,
    flush: Duration,
) {
    let client = build_client(Duration::from_secs(5));
    let mut batch: Vec<Json> = Vec::new();
    let mut tick = tokio::time::interval(flush);
    let mut warned = false;
    loop {
        tokio::select! {
            got = rx.recv() => match got {
                Some(s) => {
                    batch.push(s);
                    if batch.len() >= BATCH {
                        send(&client, &url, &service, &headers, &mut batch, &mut warned).await;
                    }
                }
                None => {
                    send(&client, &url, &service, &headers, &mut batch, &mut warned).await;
                    return;
                }
            },
            _ = tick.tick() => send(&client, &url, &service, &headers, &mut batch, &mut warned).await,
        }
    }
}

async fn send(
    client: &HttpClient,
    url: &str,
    service: &str,
    headers: &[(String, String)],
    batch: &mut Vec<Json>,
    warned: &mut bool,
) {
    if batch.is_empty() {
        return;
    }
    let body = serde_json::to_vec(&export_body(service, batch)).unwrap_or_default();
    batch.clear();
    let mut b = Request::builder()
        .method(Method::POST)
        .uri(url)
        .header("content-type", "application/json");
    for (k, v) in headers {
        b = b.header(k, v);
    }
    let Ok(req) = b.body(Full::new(Bytes::from(body))) else {
        return;
    };
    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        let resp = client.request(req).await.map_err(|e| e.to_string())?;
        let status = resp.status();
        let _ = Limited::new(resp.into_body(), 64 * 1024).collect().await;
        if status.is_success() {
            Ok(())
        } else {
            Err(format!("the collector answered {status}"))
        }
    })
    .await;
    let failure = match outcome {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(e),
        Err(_) => Some("the collector did not answer in time".to_owned()),
    };
    match failure {
        // One message per outage, not one per batch.
        Some(e) if !*warned => {
            eprintln!("airmate: warning: cannot export traces to {url}: {e}");
            *warned = true;
        }
        Some(_) => {}
        None => *warned = false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec() -> RequestRecord {
        RequestRecord {
            kind: RecordKind::Request,
            ts: "2026-10-14T12:00:01.500Z".into(),
            request_id: "req_abc".into(),
            client: "ci".into(),
            inbound: "anthropic".into(),
            model_asked: "claude-x".into(),
            target: Some("fw/kimi".into()),
            upstream_model: Some("kimi".into()),
            status: 200,
            attempts: 2,
            ttfb_ms: Some(300),
            total_ms: 1_500,
            input_tokens: 10,
            output_tokens: 4,
            est_cost_usd: Some(0.01),
            ..RequestRecord::default()
        }
    }

    fn attr_of<'a>(span: &'a Json, key: &str) -> &'a Json {
        span["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["key"] == key)
            .map(|a| &a["value"])
            .unwrap_or_else(|| panic!("no attribute {key}"))
    }

    #[test]
    fn a_request_becomes_one_span_with_stable_ids_and_the_right_times() {
        let s = span(&rec()).unwrap();
        assert_eq!(s["traceId"], trace_id("req_abc"));
        assert_eq!(s["spanId"], span_id("req_abc"));
        assert_eq!(s["traceId"].as_str().unwrap().len(), 32);
        assert_eq!(s["spanId"].as_str().unwrap().len(), 16);
        assert_ne!(trace_id("req_abc"), trace_id("req_abd"));
        assert_eq!(s["name"], "airmate anthropic");
        assert_eq!(s["kind"], 2);
        let end: u64 = s["endTimeUnixNano"].as_str().unwrap().parse().unwrap();
        let start: u64 = s["startTimeUnixNano"].as_str().unwrap().parse().unwrap();
        assert_eq!(
            end - start,
            1_500_000_000,
            "the span lasts as long as the request"
        );
        assert_eq!(
            end,
            parse_rfc3339_millis("2026-10-14T12:00:01.500Z").unwrap() * 1_000_000
        );
        assert_eq!(s["status"], json!({"code": 1}));
    }

    #[test]
    fn attributes_carry_what_an_operator_would_filter_on() {
        let s = span(&rec()).unwrap();
        assert_eq!(attr_of(&s, "airmate.client"), &json!({"stringValue": "ci"}));
        assert_eq!(
            attr_of(&s, "airmate.target"),
            &json!({"stringValue": "fw/kimi"})
        );
        assert_eq!(
            attr_of(&s, "http.response.status_code"),
            &json!({"intValue": "200"})
        );
        assert_eq!(
            attr_of(&s, "gen_ai.usage.input_tokens"),
            &json!({"intValue": "10"})
        );
        assert_eq!(
            attr_of(&s, "gen_ai.request.model"),
            &json!({"stringValue": "kimi"})
        );
        assert_eq!(
            attr_of(&s, "airmate.cost_usd"),
            &json!({"doubleValue": 0.01})
        );
        assert_eq!(attr_of(&s, "airmate.ttfb_ms"), &json!({"intValue": "300"}));
    }

    #[test]
    fn server_errors_and_broken_streams_are_error_spans_client_errors_are_not() {
        let mut r = rec();
        r.status = 502;
        r.error_class = Some("api_error".into());
        assert_eq!(span(&r).unwrap()["status"]["code"], 2);
        r.status = 200;
        r.error_class = Some("stream_error".into());
        assert_eq!(span(&r).unwrap()["status"]["code"], 2);
        r.status = 429;
        r.error_class = Some("rate_limit_error".into());
        assert_eq!(span(&r).unwrap()["status"]["code"], 1);
        assert_eq!(
            attr_of(&span(&r).unwrap(), "error.type"),
            &json!({"stringValue": "rate_limit_error"})
        );
    }

    #[test]
    fn only_request_records_make_spans() {
        for kind in [RecordKind::Attempt, RecordKind::Classifier] {
            assert!(span(&RequestRecord { kind, ..rec() }).is_none());
        }
        assert!(span(&RequestRecord {
            ts: "garbage".into(),
            ..rec()
        })
        .is_none());
    }

    #[test]
    fn the_export_body_names_the_service() {
        let b = export_body("my-gateway", &[span(&rec()).unwrap()]);
        let res = &b["resourceSpans"][0];
        assert_eq!(
            res["resource"]["attributes"][0],
            json!({"key": "service.name", "value": {"stringValue": "my-gateway"}})
        );
        assert_eq!(res["scopeSpans"][0]["spans"].as_array().unwrap().len(), 1);
    }
}
