//! Regression tests for problems found in review of the first complete version. Each test names
//! what went wrong.

mod support;

use std::time::{Duration, Instant};

use airmate::auth::hash_key;
use serde_json::{json, Value};
use support::{Mock, Part, Reply, TestGateway};

fn chat_ok() -> Value {
    json!({"id": "c", "model": "m", "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 5, "completion_tokens": 1}})
}

fn anthropic_ok() -> Value {
    json!({"id": "m1", "type": "message", "role": "assistant", "model": "x", "content": [{"type": "text", "text": "ok"}], "stop_reason": "end_turn", "usage": {"input_tokens": 3, "output_tokens": 1}})
}

fn messages(text: &str) -> Value {
    json!({"model": "claude-x", "max_tokens": 8, "messages": [{"role": "user", "content": text}]})
}

fn chat_yaml(mock: &Mock, extra_upstream: &str, extra_top: &str) -> String {
    format!("upstreams:\n  - {{ name: up, url: \"{}\"{extra_upstream} }}\nmodels:\n  - {{ match: \"*\", targets: [ {{ upstream: up, model: m }} ] }}\n{extra_top}", mock.url())
}

#[tokio::test]
async fn a_huge_retry_after_cannot_overflow_and_kill_the_request() {
    let mock = Mock::start(|_| {
        Reply::json_with(
            429,
            json!({"error": {"message": "slow"}}),
            &[("retry-after", "18446744073709551615")],
        )
    })
    .await;
    let g = TestGateway::start(&chat_yaml(&mock, "", "")).await;
    // The sum overflowed once the router's clock had passed a second.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    let r = g.post("/v1/messages", &messages("hi")).await;
    assert_eq!(r.status, 429, "{}", r.text());
    // The target is cooling down, and the gateway is still serving.
    let again = g.post("/v1/messages", &messages("hi")).await;
    assert_eq!(again.status, 529, "{}", again.text());
    assert!(
        again
            .header("x-airmate-skipped")
            .unwrap_or("")
            .contains("cooling-down"),
        "{:?}",
        again.headers
    );
    assert_eq!(g.get("/v1/models").await.status, 200);
}

#[tokio::test]
async fn a_reply_body_that_stalls_after_its_headers_times_out_instead_of_holding_a_slot() {
    let mock = Mock::start(|_| Reply::Stream {
        status: 200,
        headers: vec![("content-type".into(), "application/json".into())],
        parts: vec![Part::Bytes(b"{\"id\":".to_vec()), Part::Hang],
    })
    .await;
    let g = TestGateway::start(&chat_yaml(&mock, ", timeouts: { idle_secs: 1 }", "")).await;
    let started = Instant::now();
    let r = g.post("/v1/messages", &messages("hi")).await;
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "took {:?}",
        started.elapsed()
    );
    assert!(
        r.status == 504 || r.status == 502,
        "{} {}",
        r.status,
        r.text()
    );
    // The slot came back: the next request is served, not refused for want of capacity.
    let next = g.post("/v1/messages", &messages("again")).await;
    assert_ne!(next.status, 429, "{}", next.text());
}

fn client_yaml(mock: &Mock, rate: &str, models: &str) -> String {
    chat_yaml(
        mock,
        "",
        &format!("clients:\n  - name: ci\n    key_hash: \"{}\"\n    rate: {rate}\n    models: {models}\n", hash_key("ci-key")),
    )
}

async fn as_ci(g: &TestGateway, path: &str, body: &Value) -> support::Resp {
    g.post_with(path, body, &[("x-api-key", "ci-key")]).await
}

#[tokio::test]
async fn tokens_charged_for_a_request_that_fails_are_given_back() {
    let mock = Mock::start(|_| Reply::json(400, json!({"error": {"message": "nope"}}))).await;
    let g = TestGateway::start(&client_yaml(&mock, "{ tpm: 1000 }", "[]")).await;
    let big = "x".repeat(2_400); // about 600 estimated tokens
    for i in 0..4 {
        let r = as_ci(&g, "/v1/messages", &messages(&big)).await;
        assert_eq!(
            r.status,
            400,
            "request {i} must reach the upstream, not be rate limited: {}",
            r.text()
        );
    }
}

#[tokio::test]
async fn count_tokens_obeys_the_clients_model_list_and_rate_limit() {
    let mock = Mock::start(|_| Reply::json(200, json!({"input_tokens": 3}))).await;
    let g = TestGateway::start(&client_yaml(&mock, "{ rpm: 2 }", "[\"claude-haiku-*\"]")).await;
    let denied = as_ci(
        &g,
        "/v1/messages/count_tokens",
        &json!({"model": "claude-opus-4", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(denied.status, 403, "{}", denied.text());
    assert_eq!(mock.count(), 0);
    let ok = json!({"model": "claude-haiku-4", "messages": [{"role": "user", "content": "hi"}]});
    assert_eq!(
        as_ci(&g, "/v1/messages/count_tokens", &ok).await.status,
        200
    );
    assert_eq!(
        as_ci(&g, "/v1/messages/count_tokens", &ok).await.status,
        200
    );
    let limited = as_ci(&g, "/v1/messages/count_tokens", &ok).await;
    assert_eq!(
        limited.status,
        429,
        "the third count in a minute exceeds rpm 2: {}",
        limited.text()
    );
}

#[tokio::test]
async fn gemini_count_tokens_obeys_the_clients_model_list_too() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok())).await;
    let yaml = client_yaml(&mock, "{ rpm: 100 }", "[\"gemini-flash*\"]")
        .replace("match: \"*\"", "match: \"gemini-*\"");
    let g = TestGateway::start(&yaml).await;
    let body = json!({"contents": [{"parts": [{"text": "hi"}]}]});
    let r = g
        .post_with(
            "/v1beta/models/gemini-2.5-pro:countTokens",
            &body,
            &[("x-goog-api-key", "ci-key")],
        )
        .await;
    assert_eq!(r.status, 403, "{}", r.text());
    let r = g
        .post_with(
            "/v1beta/models/gemini-flash-2:countTokens",
            &body,
            &[("x-goog-api-key", "ci-key")],
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
}

#[tokio::test]
async fn a_target_output_cap_applies_to_forwarded_requests_too() {
    let mock = Mock::start(|_| Reply::json(200, anthropic_ok())).await;
    let yaml = format!("upstreams:\n  - {{ name: up, url: \"{}\", protocol: anthropic }}\nmodels:\n  - {{ match: \"*\", targets: [ {{ upstream: up, model: m, max_output_tokens: 100 }} ] }}\n", mock.url());
    let g = TestGateway::start(&yaml).await;
    let r = g.post("/v1/messages", &json!({"model": "claude-x", "max_tokens": 5000, "messages": [{"role": "user", "content": "hi"}]})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(mock.recorded()[0].json()["max_tokens"], 100);
    assert!(
        r.header("x-airmate-ignored")
            .unwrap_or("")
            .contains("max_tokens"),
        "{:?}",
        r.headers
    );

    let chat = Mock::start(|_| Reply::json(200, chat_ok())).await;
    let g = TestGateway::start(&format!("upstreams:\n  - {{ name: up, url: \"{}\" }}\nmodels:\n  - {{ match: \"*\", targets: [ {{ upstream: up, model: m, max_output_tokens: 100 }} ] }}\n", chat.url())).await;
    let r = g.post("/v1/chat/completions", &json!({"model": "gpt-x", "max_tokens": 5000, "messages": [{"role": "user", "content": "hi"}]})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(chat.recorded()[0].json()["max_tokens"], 100);
}

#[tokio::test]
async fn a_forwarded_stream_that_breaks_counts_against_the_circuit() {
    let mock = Mock::start(|_| {
        Reply::sse(vec![
            Part::Bytes(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"x\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n".to_vec()),
            // Long enough that the first chunk is delivered before the connection breaks.
            Part::Delay(Duration::from_millis(150)),
            Part::Reset,
        ])
    })
    .await;
    let yaml = format!("upstreams:\n  - {{ name: up, url: \"{}\", protocol: anthropic }}\nmodels:\n  - {{ match: \"*\", targets: [ {{ upstream: up, model: m }} ] }}\ncircuit: {{ failure_threshold: 1, cooldown_secs: 3600, max_cooldown_secs: 3600 }}\n", mock.url());
    let g = TestGateway::start(&yaml).await;
    let mut body = messages("hi");
    body["stream"] = json!(true);
    let first = g.post("/v1/messages", &body).await;
    assert!(first.text().contains("event: error"), "{}", first.text());
    for _ in 0..50 {
        if g.post("/v1/messages", &body).await.status == 529 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    panic!("the broken stream never opened the circuit");
}

#[tokio::test]
async fn an_upstream_error_event_on_a_200_stream_cannot_carry_the_key_to_the_client() {
    let key = "sk-up-SECRET-123456";
    let mock = Mock::start(move |_| {
        Reply::sse(vec![
            Part::Bytes(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"x\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n".to_vec()),
            Part::Bytes(format!("event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"api_error\",\"message\":\"bad key {key} rejected\"}}}}\n\n").into_bytes()),
        ])
    })
    .await;
    let yaml = format!("upstreams:\n  - {{ name: up, url: \"{}\", protocol: anthropic, api_key: {{ env: UP_KEY }} }}\nmodels:\n  - {{ match: \"*\", targets: [ {{ upstream: up, model: m }} ] }}\n", mock.url());
    let g = TestGateway::start_with_env(&yaml, &|n| (n == "UP_KEY").then(|| key.to_owned())).await;
    let mut body = messages("hi");
    body["stream"] = json!(true);
    let r = g.post("/v1/messages", &body).await;
    let text = r.text();
    assert!(text.contains("event: error"), "{text}");
    assert!(!text.contains(key), "the key reached the client:\n{text}");
    assert!(text.contains("[redacted]"), "{text}");
}

#[tokio::test]
async fn health_probes_carry_the_credentials_the_upstreams_protocol_takes() {
    let gemini = Mock::start(|_| Reply::json(200, json!({"models": []}))).await;
    let bedrock = Mock::start(|_| Reply::json(200, json!({}))).await;
    let yaml = format!(
        "upstreams:\n  - {{ name: g, url: \"{}\", protocol: gemini, api_key: {{ env: GK }}, health_check: {{ path: /v1beta/models, interval_secs: 1 }} }}\n  - {{ name: b, url: \"{}\", protocol: bedrock, aws: {{ region: us-east-1, access_key_id: {{ env: AK }}, secret_access_key: {{ env: SK }} }}, health_check: {{ path: /foundation-models, interval_secs: 1 }} }}\nmodels:\n  - {{ match: \"*\", targets: [ {{ upstream: g, model: gemini-2.5-flash }}, {{ upstream: b, model: m }} ] }}\n",
        gemini.url(),
        bedrock.url()
    );
    let _g = TestGateway::start_with_env(&yaml, &|n| match n {
        "GK" => Some("gemini-key-value".into()),
        "AK" => Some("AKIAEXAMPLE12345".into()),
        "SK" => Some("secretsecretsecret".into()),
        _ => None,
    })
    .await;
    for _ in 0..100 {
        if gemini.count() > 0 && bedrock.count() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let probe = &gemini.recorded()[0];
    assert_eq!(
        (probe.method.as_str(), probe.header("x-goog-api-key")),
        ("GET", Some("gemini-key-value"))
    );
    assert!(
        probe.header("authorization").is_none(),
        "Gemini does not take a bearer token"
    );
    let probe = &bedrock.recorded()[0];
    assert_eq!(probe.method, "GET");
    let auth = probe.header("authorization").unwrap_or("");
    assert!(
        auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIAEXAMPLE12345/"),
        "{auth}"
    );
    assert!(probe.header("x-amz-date").is_some());
}

#[tokio::test]
async fn a_client_named_default_is_not_accepted() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok())).await;
    let yaml = chat_yaml(
        &mock,
        "",
        &format!(
            "clients:\n  - name: default\n    key_hash: \"{}\"\n",
            hash_key("k")
        ),
    );
    let err = airmate::config::load_str(&yaml, None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("reserved"), "{err}");
}
