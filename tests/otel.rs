//! Trace export: spans reach a collector as OTLP/JSON, and a dead collector costs nothing.
#![cfg(feature = "otel")]

mod support;

use std::sync::Arc;
use std::time::Duration;

use airmate::otel::OtelSink;
use serde_json::{json, Value};
use support::{Capture, Mock, Reply, TestGateway};

fn chat_ok() -> Value {
    json!({"id": "c", "model": "m", "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 5, "completion_tokens": 1}})
}

fn yaml(mock: &Mock) -> String {
    format!("upstreams:\n  - {{ name: up, url: \"{}\" }}\nmodels:\n  - {{ match: \"*\", targets: [ {{ upstream: up, model: m }} ] }}\n", mock.url())
}

async fn wait_for_posts(collector: &Mock, n: usize) {
    for _ in 0..100 {
        if collector.count() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

#[tokio::test]
async fn a_request_produces_a_span_at_the_collector() {
    let upstream = Mock::start(|_| Reply::json(200, chat_ok())).await;
    let collector = Mock::start(|_| Reply::json(200, json!({}))).await;
    let log = Arc::new(Capture::default());
    let sink = OtelSink::start(
        log.clone(),
        &collector.url(),
        "test-gateway",
        vec![("x-vendor-key".into(), "vk-123".into())],
        Duration::from_millis(50),
    );
    let g = TestGateway::start_with_sink(&yaml(&upstream), &|_| None, sink).await;
    let r = g.post("/v1/messages", &json!({"model": "claude-x", "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}]})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    wait_for_posts(&collector, 1).await;

    let seen = &collector.recorded()[0];
    assert_eq!(
        (seen.method.as_str(), seen.path.as_str()),
        ("POST", "/v1/traces")
    );
    assert_eq!(seen.header("content-type"), Some("application/json"));
    assert_eq!(seen.header("x-vendor-key"), Some("vk-123"));
    let body = seen.json();
    let res = &body["resourceSpans"][0];
    assert_eq!(
        res["resource"]["attributes"][0]["value"]["stringValue"],
        "test-gateway"
    );
    let span = &res["scopeSpans"][0]["spans"][0];
    assert_eq!(span["name"], "airmate anthropic");
    let request_id = r.header("request-id").unwrap();
    assert_eq!(span["traceId"], airmate::otel::trace_id(request_id));
    let has = |k: &str, v: &str| {
        span["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["key"] == k && a["value"]["stringValue"] == v)
    };
    assert!(
        has("airmate.target", "up/m") && has("airmate.client", "default"),
        "{span}"
    );
    assert!(
        !log.wait_for(1).await.is_empty(),
        "the request log still gets every record"
    );
}

#[tokio::test]
async fn an_unreachable_collector_never_slows_or_fails_requests() {
    let upstream = Mock::start(|_| Reply::json(200, chat_ok())).await;
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let sink = OtelSink::start(
        Arc::new(Capture::default()),
        &dead,
        "svc",
        vec![],
        Duration::from_millis(30),
    );
    let g = TestGateway::start_with_sink(&yaml(&upstream), &|_| None, sink).await;
    let started = std::time::Instant::now();
    for _ in 0..20 {
        let r = g.post("/v1/messages", &json!({"model": "claude-x", "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}]})).await;
        assert_eq!(r.status, 200);
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "requests do not wait for the exporter"
    );
}
