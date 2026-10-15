//! No secret appears where it should not: the request log, the usage ledger, the body log,
//! metrics, status, error responses and `Debug` output. Success and failure paths both.

mod support;

use std::sync::{Arc, Mutex};

use airmate::admin::{metrics_text, status_json};
use airmate::auth::hash_key;
use airmate::bodylog::{BodyRecord, BodySink};
use airmate::usage::{UsageRecord, UsageSink};
use serde_json::json;
use support::{Capture, Mock, Reply, TestGateway};

const UPSTREAM_KEY: &str = "sk-upstream-SECRET-1234567890";
const CLIENT_KEY: &str = "am_client_SECRET_abcdefghij";
const GATEWAY_TOKEN: &str = "test-gateway-token";
const HELPER_KEY: &str = "sk-helper-SECRET-0987654321";

#[derive(Default)]
struct Ledger(Mutex<Vec<UsageRecord>>);
impl UsageSink for Ledger {
    fn record(&self, r: &UsageRecord) {
        self.0.lock().unwrap().push(r.clone());
    }
}

#[derive(Default)]
struct Bodies(Mutex<Vec<BodyRecord>>);
impl BodySink for Bodies {
    fn enabled(&self) -> bool {
        true
    }
    fn max_bytes(&self) -> usize {
        1 << 20
    }
    fn record(&self, r: &BodyRecord) {
        self.0.lock().unwrap().push(r.clone());
    }
}

fn assert_clean(what: &str, text: &str) {
    for secret in [UPSTREAM_KEY, CLIENT_KEY, GATEWAY_TOKEN, HELPER_KEY] {
        assert!(!text.contains(secret), "{what} leaks {secret}:\n{text}");
    }
}

#[tokio::test]
async fn secrets_never_reach_logs_metrics_status_errors_or_debug_output() {
    // The upstream answers 200, 401 (a bad key), 500 and an error that echoes the Authorization
    // header, which is the worst case for a gateway that forwards upstream error text.
    let mock = Mock::start(|r| {
        let body = String::from_utf8_lossy(&r.body).into_owned();
        if body.contains("echo-the-key") {
            Reply::json(400, json!({"error": {"message": format!("bad request, you sent {}", r.header("authorization").unwrap_or(""))}}))
        } else if body.contains("unauthorized") {
            Reply::json(401, json!({"error": {"message": "invalid api key"}}))
        } else if body.contains("explode") {
            Reply::json(500, json!({"error": {"message": "boom"}}))
        } else {
            Reply::json(200, json!({"id": "c", "model": "m", "choices": [{"index": 0, "message": {"role": "assistant", "content": "fine"}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 5, "completion_tokens": 1}}))
        }
    })
    .await;
    let yaml = format!(
        "server:\n  affinity: false\nfailover:\n  retries: 0\ncircuit:\n  failure_threshold: 1000\nupstreams:\n  - {{ name: up, url: \"{}\", api_key: {{ env: UP_KEY }} }}\nmodels:\n  - {{ match: \"*\", targets: [ {{ upstream: up, model: m, price: {{ input_per_mtok: 1, output_per_mtok: 1 }} }} ] }}\nclients:\n  - name: ci\n    key_hash: \"{}\"\n    budget: {{ usd_per_day: 100 }}\n",
        mock.url(),
        hash_key(CLIENT_KEY),
    );
    let log = Arc::new(Capture::default());
    let ledger = Arc::new(Ledger::default());
    let bodies = Arc::new(Bodies::default());
    let env = |n: &str| (n == "UP_KEY").then(|| UPSTREAM_KEY.to_owned());
    let g =
        TestGateway::start_all_for_leaks(&yaml, &env, log.clone(), ledger.clone(), bodies.clone())
            .await;

    let mut responses = Vec::new();
    // Chat and Responses inbound, and a streaming request, take the other code paths.
    for (path, body) in [
        (
            "/v1/chat/completions",
            json!({"model": "gpt-x", "messages": [{"role": "user", "content": "echo-the-key"}]}),
        ),
        (
            "/v1/responses",
            json!({"model": "gpt-x", "input": "explode"}),
        ),
        (
            "/v1/messages",
            json!({"model": "claude-x", "max_tokens": 8, "stream": true, "messages": [{"role": "user", "content": "explode"}]}),
        ),
    ] {
        let r = g
            .post_with(
                path,
                &body,
                &[("authorization", &format!("Bearer {CLIENT_KEY}"))],
            )
            .await;
        responses.push(format!("{} {:?} {}", r.status, r.headers, r.text()));
    }
    // The bad-key case goes last: a rejected key trips the circuit, and the rest would be skipped.
    for content in ["hello", "echo-the-key", "explode", "unauthorized"] {
        for key in [CLIENT_KEY, GATEWAY_TOKEN, "am_wrong_key_value"] {
            let r = g
                .post_with("/v1/messages", &json!({"model": "claude-x", "max_tokens": 8, "messages": [{"role": "user", "content": content}]}), &[("x-api-key", key)])
                .await;
            responses.push(format!("{} {:?} {}", r.status, r.headers, r.text()));
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert!(
        responses.iter().any(|r| r.starts_with("401")),
        "a wrong key was refused"
    );
    assert!(
        responses.iter().any(|r| r.starts_with("200")),
        "something succeeded"
    );
    assert_clean("a client response", &responses.join("\n"));

    // The upstream saw its own key and never the gateway's or the client's.
    for rec in mock.recorded() {
        let auth = format!("{:?}", rec.headers);
        assert!(
            auth.contains(UPSTREAM_KEY) || rec.header("authorization").is_none(),
            "{auth}"
        );
        assert!(
            !auth.contains(GATEWAY_TOKEN) && !auth.contains(CLIENT_KEY),
            "client credentials reached the upstream: {auth}"
        );
    }

    assert_clean(
        "the request log",
        &serde_json::to_string(&log.all()).unwrap(),
    );
    assert_clean(
        "the usage ledger",
        &serde_json::to_string(&ledger.0.lock().unwrap().clone()).unwrap(),
    );
    assert_clean("metrics", &metrics_text(&g.shared));
    assert_clean("status", &status_json(&g.shared).to_string());
    // The request bodies hold only what clients sent: no header ever lands there.
    assert_clean(
        "the body log",
        &serde_json::to_string(&bodies.0.lock().unwrap().clone()).unwrap(),
    );
    let tables = g.shared.tables();
    assert_clean("Debug of the config", &format!("{:?}", tables.config));
    assert_clean("Debug of the model table", &format!("{:?}", tables.models));
    assert_clean(
        "Debug of the client table",
        &format!("{:?}", tables.clients),
    );
}

#[test]
fn redaction_replaces_every_occurrence_and_leaves_short_values_alone() {
    use airmate::secret::redact;
    assert_eq!(
        redact("a sk-abcdefgh b sk-abcdefgh", ["sk-abcdefgh"]),
        "a [redacted] b [redacted]"
    );
    assert_eq!(
        redact("key=one two=three", ["one", "tw"]),
        "key=one two=three",
        "short values would mangle text"
    );
    assert_eq!(redact("nothing here", ["sk-abcdefgh"]), "nothing here");
    assert_eq!(
        redact("x sk-1234567 y tok-7654321", ["sk-1234567", "tok-7654321"]),
        "x [redacted] y [redacted]"
    );
}

#[test]
fn the_secret_type_redacts_in_every_formatter() {
    let s = airmate::secret::Secret::new(HELPER_KEY);
    assert_clean("Debug", &format!("{s:?}"));
    assert_clean("Display", &format!("{s}"));
    assert_clean("pretty Debug", &format!("{s:#?}"));
    assert_clean("a struct holding it", &format!("{:?}", Some(s)));
}
