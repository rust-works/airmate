//! End to end: the Anthropic Messages endpoints against a scripted upstream.

mod support;

use serde_json::{json, Value};
use support::{env_with_key, yaml_for, Mock, Reply, TestGateway};

const UP_KEY: &str = "sk-upstream-secret";

fn chat_ok(text: &str) -> Value {
    json!({"id": "chatcmpl-1", "model": "upstream-model", "choices": [{"index": 0, "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 11, "completion_tokens": 3, "prompt_tokens_details": {"cached_tokens": 2}}})
}

fn request(model: &str) -> Value {
    json!({"model": model, "max_tokens": 64, "messages": [{"role": "user", "content": "hello"}]})
}

async fn gateway(mock: &Mock, protocol: &str, flavor: &str) -> TestGateway {
    let env = env_with_key("UP_KEY", UP_KEY);
    TestGateway::start_with_env(&yaml_for(&mock.url(), protocol, flavor, "", ""), &env).await
}

#[tokio::test]
async fn a_chat_upstream_answers_as_an_anthropic_message() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok("Hi!"))).await;
    let gw = gateway(&mock, "chat", "generic").await;
    let r = gw.post("/v1/messages", &request("claude-sonnet-4")).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let m = r.json();
    assert_eq!(m["type"], "message");
    assert_eq!(m["role"], "assistant");
    assert_eq!(
        m["model"], "claude-sonnet-4",
        "the response echoes the model the client sent"
    );
    assert_eq!(m["content"], json!([{"type": "text", "text": "Hi!"}]));
    assert_eq!(m["stop_reason"], "end_turn");
    assert_eq!(m["usage"]["input_tokens"], 9);
    assert_eq!(m["usage"]["cache_read_input_tokens"], 2);
    assert_eq!(m["usage"]["output_tokens"], 3);
    assert_eq!(r.header("x-airmate-upstream"), Some("up"));
    assert_eq!(r.header("x-airmate-upstream-model"), Some("upstream-model"));
    assert_eq!(r.header("x-airmate-attempts"), Some("1"));
    assert!(r.header("request-id").unwrap().starts_with("req_"));
    assert_eq!(r.header("content-type"), Some("application/json"));
}

#[tokio::test]
async fn the_upstream_sees_a_translated_request_with_its_own_key_and_never_the_gateway_token() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok("ok"))).await;
    let gw = gateway(&mock, "chat", "generic").await;
    let mut req = request("claude-sonnet-4");
    req["system"] = json!("be brief");
    req["temperature"] = json!(0.25);
    let r = gw.post("/v1/messages", &req).await;
    assert_eq!(r.status, 200);
    let seen = mock.recorded();
    assert_eq!(seen.len(), 1);
    let rec = &seen[0];
    assert_eq!(
        (rec.method.as_str(), rec.path.as_str()),
        ("POST", "/v1/chat/completions")
    );
    assert_eq!(
        rec.header("authorization"),
        Some(&*format!("Bearer {UP_KEY}"))
    );
    let body = rec.json();
    assert_eq!(body["model"], "upstream-model");
    assert_eq!(
        body["messages"][0],
        json!({"role": "system", "content": "be brief"})
    );
    assert_eq!(
        body["messages"][1],
        json!({"role": "user", "content": "hello"})
    );
    assert_eq!(body["max_tokens"], 64);
    assert_eq!(body["temperature"], 0.25);
    let everything = format!("{:?}", rec.headers) + &String::from_utf8_lossy(&rec.body);
    assert!(
        !everything.contains(&gw.token),
        "the gateway token never reaches the upstream"
    );
    let reply = format!("{:?}{}", r.headers, r.text());
    assert!(
        !reply.contains(UP_KEY),
        "the upstream key never reaches the client"
    );
}

#[tokio::test]
async fn a_responses_upstream_is_called_with_store_false() {
    let mock = Mock::start(|_| {
        Reply::json(200, json!({"id": "resp_1", "model": "m", "status": "completed",
            "output": [{"type": "message", "content": [{"type": "output_text", "text": "via responses"}]}],
            "usage": {"input_tokens": 5, "output_tokens": 2}}))
    })
    .await;
    let gw = gateway(&mock, "responses", "openai").await;
    let r = gw.post("/v1/messages", &request("claude-x")).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["content"][0]["text"], "via responses");
    let rec = &mock.recorded()[0];
    assert_eq!(rec.path, "/v1/responses");
    assert_eq!(rec.json()["store"], false);
    assert_eq!(rec.json()["max_output_tokens"], 64);
}

#[tokio::test]
async fn an_anthropic_upstream_is_a_passthrough_with_the_model_mapped() {
    let mock = Mock::start(|_| {
        Reply::json(200, json!({"id": "msg_up", "type": "message", "role": "assistant", "model": "upstream-model",
            "content": [{"type": "text", "text": "pass"}], "stop_reason": "end_turn", "usage": {"input_tokens": 4, "output_tokens": 1}}))
    })
    .await;
    let gw = gateway(&mock, "anthropic", "generic").await;
    let mut req = request("claude-opus-4");
    req["metadata"] = json!({"user_id": "u"});
    let r = gw
        .post_with(
            "/v1/messages",
            &req,
            &[
                ("anthropic-version", "2023-06-01"),
                ("anthropic-beta", "tools-2024"),
            ],
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(
        r.json()["model"],
        "claude-opus-4",
        "the client's model name is echoed"
    );
    assert_eq!(r.json()["content"][0]["text"], "pass");
    let rec = &mock.recorded()[0];
    assert_eq!(rec.path, "/v1/messages");
    assert_eq!(rec.header("x-api-key"), Some(UP_KEY));
    assert_eq!(rec.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(rec.header("anthropic-beta"), Some("tools-2024"));
    assert_eq!(rec.json()["model"], "upstream-model");
    assert_eq!(
        rec.json()["metadata"],
        json!({"user_id": "u"}),
        "the body is forwarded as sent"
    );
    assert!(!format!("{:?}", rec.headers).contains(&gw.token));
}

#[tokio::test]
async fn fireworks_passthrough_strips_what_it_documents_as_unsupported_and_says_so() {
    let mock = Mock::start(|_| Reply::json(200, json!({"id": "m", "type": "message", "model": "x", "content": [], "stop_reason": "end_turn", "usage": {}}))).await;
    let gw = gateway(&mock, "anthropic", "fireworks").await;
    let mut req = request("claude-x");
    req["inference_geo"] = json!("us");
    req["tools"] = json!([{"name": "t", "input_schema": {"type": "object"}, "cache_control": {"type": "ephemeral"}}]);
    let r = gw.post("/v1/messages", &req).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let sent = mock.recorded()[0].json();
    assert!(sent.get("inference_geo").is_none() && sent["tools"][0].get("cache_control").is_none());
    let ignored = r.header("x-airmate-ignored").unwrap();
    assert!(
        ignored.contains("inference_geo") && ignored.contains("tools.cache_control"),
        "{ignored}"
    );
}

#[tokio::test]
async fn ignored_fields_are_reported_in_a_header_and_rejected_ones_stop_the_request() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok("ok"))).await;
    let gw = gateway(&mock, "chat", "generic").await;
    let mut req = request("claude-x");
    req["metadata"] = json!({"user_id": "u"});
    req["service_tier"] = json!("auto");
    let r = gw.post("/v1/messages", &req).await;
    assert_eq!(r.status, 200);
    let ignored = r.header("x-airmate-ignored").unwrap();
    assert!(
        ignored.contains("metadata") && ignored.contains("service_tier"),
        "{ignored}"
    );

    let before = mock.count();
    let mut bad = request("claude-x");
    bad["safeguards"] = json!({"x": 1});
    let r = gw.post("/v1/messages", &bad).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["type"], "invalid_request_error");
    assert!(r.json()["error"]["message"]
        .as_str()
        .unwrap()
        .contains("safeguards"));
    assert_eq!(
        mock.count(),
        before,
        "nothing is sent upstream for a rejected request"
    );

    let mut unknown = request("claude-x");
    unknown["brand_new_field"] = json!(1);
    let r = gw.post("/v1/messages", &unknown).await;
    assert_eq!(r.status, 400);
    assert!(r.json()["error"]["message"]
        .as_str()
        .unwrap()
        .contains("brand_new_field"));
}

#[tokio::test]
async fn upstream_errors_map_to_the_status_table() {
    for (up_status, up_body, want_status, want_type) in [
        (
            400,
            json!({"error": {"message": "bad"}}),
            400,
            "invalid_request_error",
        ),
        (
            401,
            json!({"error": {"message": "bad key"}}),
            403,
            "permission_error",
        ),
        (
            403,
            json!({"error": {"message": "nope"}}),
            403,
            "permission_error",
        ),
        (
            404,
            json!({"error": {"message": "no model"}}),
            404,
            "not_found_error",
        ),
        (
            413,
            json!({"error": {"message": "big"}}),
            413,
            "request_too_large",
        ),
        (
            429,
            json!({"error": {"message": "slow"}}),
            429,
            "rate_limit_error",
        ),
        (
            503,
            json!({"error": {"message": "busy"}}),
            529,
            "overloaded_error",
        ),
        (500, json!({"error": {"message": "boom"}}), 500, "api_error"),
    ] {
        let mock = Mock::start(move |_| Reply::json(up_status, up_body.clone())).await;
        let gw = gateway(&mock, "chat", "generic").await;
        let r = gw.post("/v1/messages", &request("claude-x")).await;
        assert_eq!(r.status, want_status, "upstream {up_status}: {}", r.text());
        let j = r.json();
        assert_eq!(j["type"], "error");
        assert_eq!(j["error"]["type"], want_type, "upstream {up_status}");
        assert!(j["request_id"].as_str().unwrap().starts_with("req_"));
    }
}

#[tokio::test]
async fn an_upstream_401_names_the_upstream_and_its_key_setting() {
    let mock =
        Mock::start(|_| Reply::json(401, json!({"error": {"message": "Incorrect API key"}}))).await;
    let gw = gateway(&mock, "chat", "generic").await;
    let r = gw.post("/v1/messages", &request("claude-x")).await;
    assert_eq!(r.status, 403);
    let msg = r.json()["error"]["message"].as_str().unwrap().to_owned();
    assert!(
        msg.contains("`up`") && msg.contains("upstreams[up].api_key"),
        "{msg}"
    );
}

#[tokio::test]
async fn rate_limits_forward_retry_after() {
    let mock = Mock::start(|_| {
        Reply::json_with(
            429,
            json!({"error": {"message": "slow"}}),
            &[("retry-after", "17")],
        )
    })
    .await;
    let gw = gateway(&mock, "chat", "generic").await;
    let r = gw.post("/v1/messages", &request("claude-x")).await;
    assert_eq!(r.status, 429);
    assert_eq!(r.header("retry-after"), Some("17"));
}

#[tokio::test]
async fn a_context_overflow_starts_with_prompt_is_too_long() {
    let mock = Mock::start(|_| {
        Reply::json(400, json!({"error": {"message": "This model's maximum context length is 8192 tokens", "code": "context_length_exceeded"}}))
    })
    .await;
    let gw = gateway(&mock, "chat", "generic").await;
    let r = gw.post("/v1/messages", &request("claude-x")).await;
    assert_eq!(r.status, 400);
    assert!(r.json()["error"]["message"]
        .as_str()
        .unwrap()
        .starts_with("prompt is too long"));
}

#[tokio::test]
async fn an_unreachable_upstream_is_a_502_and_a_silent_one_a_504() {
    let gw = TestGateway::start_with_env(
        &yaml_for("http://127.0.0.1:1", "chat", "generic", "", ""),
        &env_with_key("UP_KEY", UP_KEY),
    )
    .await;
    let r = gw.post("/v1/messages", &request("claude-x")).await;
    assert_eq!(r.status, 502, "{}", r.text());
    assert_eq!(r.json()["error"]["type"], "api_error");

    let slow = Mock::start(|_| Reply::Slow(std::time::Duration::from_secs(30))).await;
    let yaml = yaml_for(
        &slow.url(),
        "chat",
        "generic",
        "    timeouts: { connect_secs: 2, first_byte_secs: 1, idle_secs: 5 }\n",
        "",
    );
    let gw = TestGateway::start_with_env(&yaml, &env_with_key("UP_KEY", UP_KEY)).await;
    let r = gw.post("/v1/messages", &request("claude-x")).await;
    assert_eq!(r.status, 504, "{}", r.text());
    assert_eq!(r.json()["error"]["type"], "timeout_error");
}

#[tokio::test]
async fn an_unmapped_family_model_is_not_found() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok("x"))).await;
    let yaml = format!("upstreams:\n  - name: up\n    url: {}\nmodels:\n  - match: \"claude-opus-*\"\n    targets: [ {{ upstream: up, model: m }} ]\n", mock.url());
    let gw = TestGateway::start(&yaml).await;
    let r = gw.post("/v1/messages", &request("claude-haiku-9")).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["error"]["type"], "not_found_error");
    assert!(r.json()["error"]["message"]
        .as_str()
        .unwrap()
        .contains("claude-haiku-9"));
    assert_eq!(mock.count(), 0);
    // A name from another family goes straight to the default upstream.
    let r = gw.post("/v1/messages", &request("qwen3-coder")).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(mock.recorded()[0].json()["model"], "qwen3-coder");
}

#[tokio::test]
async fn malformed_bodies_are_400_and_oversized_ones_413() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok("x"))).await;
    let yaml = yaml_for(
        &mock.url(),
        "chat",
        "generic",
        "",
        "server:\n  max_body_bytes: 2000\n",
    );
    let gw = TestGateway::start_with_env(&yaml, &env_with_key("UP_KEY", UP_KEY)).await;
    let r = gw
        .request("POST", "/v1/messages", Some("not json".into()), &[])
        .await;
    assert_eq!(r.status, 400);
    let r = gw
        .request("POST", "/v1/messages", Some("[1]".into()), &[])
        .await;
    assert_eq!(r.status, 400);
    let mut big = request("claude-x");
    big["messages"][0]["content"] = json!("x".repeat(5000));
    let r = gw.post("/v1/messages", &big).await;
    assert_eq!(r.status, 413);
    assert_eq!(r.json()["error"]["type"], "request_too_large");
    assert_eq!(mock.count(), 0);
}

#[tokio::test]
async fn count_tokens_is_a_labelled_estimate() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok("x"))).await;
    let gw = gateway(&mock, "chat", "generic").await;
    let body =
        json!({"model": "claude-x", "messages": [{"role": "user", "content": "twelve chars"}]});
    let r = gw.post("/v1/messages/count_tokens", &body).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("x-airmate-token-estimate"), Some("heuristic"));
    assert_eq!(r.json()["input_tokens"], 4 + 3);
    assert_eq!(
        mock.count(),
        0,
        "a chat upstream cannot count, so nothing is sent"
    );
}

#[tokio::test]
async fn count_tokens_is_forwarded_to_an_anthropic_upstream_that_can_count() {
    let mock = Mock::start(|r| {
        assert_eq!(r.path, "/v1/messages/count_tokens");
        Reply::json(200, json!({"input_tokens": 1234}))
    })
    .await;
    let gw = gateway(&mock, "anthropic", "generic").await;
    let r = gw
        .post(
            "/v1/messages/count_tokens",
            &json!({"model": "claude-x", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["input_tokens"], 1234);
    assert_eq!(
        r.header("x-airmate-token-estimate"),
        None,
        "an exact count is not labelled an estimate"
    );
    assert_eq!(mock.recorded()[0].json()["model"], "upstream-model");
}

#[tokio::test]
async fn count_tokens_falls_back_to_the_estimate_when_the_upstream_answers_404() {
    let mock = Mock::start(|_| Reply::json(404, json!({"error": "not found"}))).await;
    let gw = gateway(&mock, "anthropic", "fireworks").await;
    let r = gw.post("/v1/messages/count_tokens", &json!({"model": "claude-x", "messages": [{"role": "user", "content": "twelve chars"}]})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("x-airmate-token-estimate"), Some("heuristic"));
    assert_eq!(r.json()["input_tokens"], 7);
    assert_eq!(mock.count(), 1, "the upstream was asked first");
}

fn count_body() -> Value {
    json!({"model": "claude-x", "messages": [{"role": "user", "content": "twelve chars"}]})
}

#[tokio::test]
async fn count_tokens_rejects_what_a_generation_would() {
    let mock = Mock::start(|_| Reply::json(200, json!({"input_tokens": 1}))).await;
    let yaml = yaml_for(
        &mock.url(),
        "anthropic",
        "generic",
        "",
        "server:\n  max_body_bytes: 2000\n",
    )
    .replace("match: \"*\"", "match: \"claude-opus-*\"");
    let gw = TestGateway::start_with_env(&yaml, &env_with_key("UP_KEY", UP_KEY)).await;

    let r = gw
        .request(
            "POST",
            "/v1/messages/count_tokens",
            Some("not json".into()),
            &[],
        )
        .await;
    assert_eq!(r.status, 400, "{}", r.text());

    let mut big = count_body();
    big["model"] = json!("claude-opus-4");
    big["messages"][0]["content"] = json!("x".repeat(5000));
    let r = gw.post("/v1/messages/count_tokens", &big).await;
    assert_eq!(r.status, 413, "{}", r.text());

    // Claude's own family is never routed by upstream name, so an unmapped one has no route.
    let mut other = count_body();
    other["model"] = json!("claude-sonnet-unmapped");
    let r = gw.post("/v1/messages/count_tokens", &other).await;
    assert_eq!(r.status, 404, "{}", r.text());
    assert_eq!(mock.count(), 0, "nothing reached the upstream");
}

#[tokio::test]
async fn count_tokens_relays_an_upstream_failure_with_its_retry_hint() {
    let mock = Mock::start(|_| {
        Reply::json_with(
            429,
            json!({"type": "error", "error": {"type": "rate_limit_error", "message": "slow down"}}),
            &[("retry-after", "7")],
        )
    })
    .await;
    let gw = gateway(&mock, "anthropic", "generic").await;
    let r = gw.post("/v1/messages/count_tokens", &count_body()).await;
    assert_eq!(r.status, 429, "{}", r.text());
    assert_eq!(r.header("retry-after"), Some("7"));
    assert_eq!(r.json()["error"]["type"], "rate_limit_error");
}

#[tokio::test]
async fn count_tokens_reports_an_upstream_that_is_down_or_cuts_the_reply_short() {
    // Nothing listens on port 1.
    let env = env_with_key("UP_KEY", UP_KEY);
    let down = TestGateway::start_with_env(
        &yaml_for("http://127.0.0.1:1", "anthropic", "generic", "", ""),
        &env,
    )
    .await;
    let r = down.post("/v1/messages/count_tokens", &count_body()).await;
    assert!(r.status >= 500, "{} {}", r.status, r.text());

    let mock = Mock::start(|_| Reply::Stream {
        status: 200,
        parts: vec![
            support::Part::Bytes(b"{\"input_".to_vec()),
            support::Part::Reset,
        ],
        headers: vec![("content-type".into(), "application/json".into())],
    })
    .await;
    let gw = gateway(&mock, "anthropic", "generic").await;
    let r = gw.post("/v1/messages/count_tokens", &count_body()).await;
    assert!(r.status >= 500, "{} {}", r.status, r.text());
}

#[tokio::test]
async fn count_tokens_rejects_a_body_it_cannot_estimate() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok("x"))).await;
    let gw = gateway(&mock, "chat", "generic").await;
    let r = gw
        .post(
            "/v1/messages/count_tokens",
            &json!({"model": "claude-x", "messages": "not a list"}),
        )
        .await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert_eq!(mock.count(), 0);
}

#[tokio::test]
async fn models_lists_exact_entries_only() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok("x"))).await;
    let yaml = format!(
        "upstreams:\n  - name: up\n    url: {}\nmodels:\n  - match: claude-opus-4\n    targets: [ {{ upstream: up, model: m }} ]\n  - match: \"claude-*\"\n    targets: [ {{ upstream: up, model: m }} ]\n",
        mock.url()
    );
    let gw = TestGateway::start(&yaml).await;
    let r = gw.get("/v1/models").await;
    assert_eq!(r.status, 200);
    let j = r.json();
    assert_eq!(j["data"].as_array().unwrap().len(), 1);
    assert_eq!(j["data"][0]["id"], "claude-opus-4");
    assert_eq!(j["data"][0]["type"], "model");
    assert_eq!(j["has_more"], false);
    assert_eq!(
        gw.get("/v1/models/claude-opus-4").await.json()["id"],
        "claude-opus-4"
    );
    assert_eq!(gw.get("/v1/models/claude-ghost").await.status, 404);
}

#[tokio::test]
async fn the_concurrency_limit_answers_429_with_retry_after() {
    let slow = Mock::start(|_| Reply::Slow(std::time::Duration::from_millis(800))).await;
    let yaml = yaml_for(
        &slow.url(),
        "chat",
        "generic",
        "",
        "server:\n  max_concurrent: 1\n",
    );
    let gw = std::sync::Arc::new(
        TestGateway::start_with_env(&yaml, &env_with_key("UP_KEY", UP_KEY)).await,
    );
    let first = {
        let gw = gw.clone();
        tokio::spawn(async move { gw.post("/v1/messages", &request("claude-x")).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let second = gw.post("/v1/messages", &request("claude-x")).await;
    assert_eq!(second.status, 429, "{}", second.text());
    assert_eq!(second.json()["error"]["type"], "rate_limit_error");
    assert!(second.header("retry-after").is_some());
    let first = first.await.unwrap();
    assert_ne!(
        first.status, 429,
        "the request that held the slot was served"
    );
}

#[tokio::test]
async fn unsupported_things_are_rejected_by_name_before_any_upstream_call() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok("x"))).await;
    let gw = gateway(&mock, "chat", "generic").await;
    for (patch, needle) in [
        (
            json!({"tools": [{"type": "bash_20250124", "name": "bash"}]}),
            "bash_20250124",
        ),
        (
            json!({"messages": [{"role": "user", "content": [{"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "AA"}}]}]}),
            "PDF",
        ),
        (json!({"max_tokens": 0}), "max_tokens"),
    ] {
        let mut req = request("claude-x");
        for (k, v) in patch.as_object().unwrap() {
            req[k] = v.clone();
        }
        let r = gw.post("/v1/messages", &req).await;
        assert_eq!(r.status, 400, "{needle}: {}", r.text());
        assert!(r.text().contains(needle), "{needle}: {}", r.text());
    }
    assert_eq!(mock.count(), 0);
}

#[tokio::test]
async fn more_than_four_stop_sequences_are_rejected_only_where_the_upstream_caps_them() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok("x"))).await;
    let mut req = request("claude-x");
    req["stop_sequences"] = json!(["a", "b", "c", "d", "e"]);

    let capped = gateway(&mock, "chat", "openai").await;
    let r = capped.post("/v1/messages", &req).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.text().contains("stop"));
    assert_eq!(mock.count(), 0);

    let uncapped = gateway(&mock, "chat", "generic").await;
    assert_eq!(uncapped.post("/v1/messages", &req).await.status, 200);
    assert_eq!(
        mock.recorded()[0].json()["stop"].as_array().unwrap().len(),
        5
    );
}

#[tokio::test]
async fn every_request_and_every_attempt_is_logged_without_secrets_or_bodies() {
    use airmate::obs::RecordKind;
    let mock = Mock::start(|_| Reply::json(200, chat_ok("SECRET-REPLY-TEXT"))).await;
    let cap = std::sync::Arc::new(support::Capture::default());
    let env = env_with_key("UP_KEY", UP_KEY);
    let gw = TestGateway::start_with_sink(
        &yaml_for(&mock.url(), "chat", "generic", "", ""),
        &env,
        cap.clone(),
    )
    .await;
    let mut req = request("claude-sonnet-4");
    req["messages"][0]["content"] = json!("SECRET-PROMPT-TEXT");
    req["metadata"] = json!({"user_id": "u"});
    let r = gw
        .post_with("/v1/messages", &req, &[("x-request-id", "rid-42")])
        .await;
    assert_eq!(r.status, 200);

    let recs = cap.wait_for(2).await;
    assert_eq!(recs.len(), 2, "one attempt and one request: {recs:?}");
    let attempt = recs.iter().find(|r| r.kind == RecordKind::Attempt).unwrap();
    let request_rec = recs.iter().find(|r| r.kind == RecordKind::Request).unwrap();
    assert_eq!(attempt.target.as_deref(), Some("up/upstream-model"));
    assert_eq!((attempt.status, attempt.attempts), (200, 1));
    assert_eq!(request_rec.request_id, "rid-42");
    assert_eq!(request_rec.client, "default");
    assert_eq!(request_rec.inbound, "anthropic");
    assert_eq!(request_rec.model_asked, "claude-sonnet-4");
    assert_eq!(
        request_rec.upstream_model.as_deref(),
        Some("upstream-model")
    );
    assert_eq!(
        (request_rec.status, request_rec.attempts, request_rec.stream),
        (200, 1, false)
    );
    assert_eq!(
        (
            request_rec.input_tokens,
            request_rec.output_tokens,
            request_rec.cached_tokens
        ),
        (9, 3, 2)
    );
    assert_eq!(request_rec.ignored, ["metadata"]);
    assert!(request_rec.ttfb_ms.is_some());
    let everything = serde_json::to_string(&recs).unwrap();
    for secret in [UP_KEY, "SECRET-PROMPT-TEXT", "SECRET-REPLY-TEXT", &gw.token] {
        assert!(!everything.contains(secret), "{secret} leaked into the log");
    }
}

#[tokio::test]
async fn refused_and_failed_requests_are_logged_with_their_class() {
    let mock = Mock::start(|_| Reply::json(429, json!({"error": {"message": "slow"}}))).await;
    let cap = std::sync::Arc::new(support::Capture::default());
    let env = env_with_key("UP_KEY", UP_KEY);
    let gw = TestGateway::start_with_sink(
        &yaml_for(&mock.url(), "chat", "generic", "", ""),
        &env,
        cap.clone(),
    )
    .await;
    let r = gw.post("/v1/messages", &request("claude-x")).await;
    assert_eq!(r.status, 429);
    let unauth = reqwest_free_unauthenticated(&gw).await;
    assert_eq!(unauth, 401);
    let recs = cap.wait_for(3).await;
    let rate = recs
        .iter()
        .find(|r| r.status == 429 && r.kind == airmate::obs::RecordKind::Request)
        .unwrap();
    assert_eq!(rate.error_class.as_deref(), Some("rate_limit_error"));
    let refused = recs.iter().find(|r| r.status == 401).unwrap();
    assert_eq!(
        (refused.client.as_str(), refused.error_class.as_deref()),
        ("-", Some("authentication_error"))
    );
}

async fn reqwest_free_unauthenticated(gw: &TestGateway) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(gw.addr).await.unwrap();
    s.write_all(
        format!(
            "GET /v1/models HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            gw.addr.port()
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).await.unwrap();
    buf.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}
