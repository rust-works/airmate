//! Gemini as an upstream: a client speaking any inbound protocol, a mock speaking `generateContent`.

mod support;

use serde_json::{json, Value};
use support::{Mock, Part, Reply, TestGateway};

fn gemini_reply(parts: Value, finish: &str) -> Value {
    json!({"candidates": [{"content": {"role": "model", "parts": parts}, "finishReason": finish, "index": 0}],
        "usageMetadata": {"promptTokenCount": 12, "candidatesTokenCount": 4, "totalTokenCount": 16}, "modelVersion": "gemini-2.5-flash", "responseId": "r1"})
}

fn yaml(mock: &Mock) -> String {
    format!(
        "server:\n  affinity: false\nupstreams:\n  - {{ name: g, url: \"{}\", protocol: gemini, api_key: {{ env: GEMINI_KEY }} }}\nmodels:\n  - {{ match: \"*\", targets: [ {{ upstream: g, model: gemini-2.5-flash }} ] }}\n",
        mock.url()
    )
}

async fn start(mock: &Mock) -> TestGateway {
    TestGateway::start_with_env(&yaml(mock), &|n| {
        (n == "GEMINI_KEY").then(|| "g-secret-key-123".to_owned())
    })
    .await
}

fn anthropic_request(stream: bool) -> Value {
    json!({"model": "claude-x", "max_tokens": 64, "stream": stream, "system": "be brief",
        "messages": [{"role": "user", "content": "hello"}]})
}

#[tokio::test]
async fn a_messages_request_becomes_generate_content_and_the_reply_comes_back_translated() {
    let mock =
        Mock::start(|_| Reply::json(200, gemini_reply(json!([{"text": "hi there"}]), "STOP")))
            .await;
    let g = start(&mock).await;
    let r = g.post("/v1/messages", &anthropic_request(false)).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json();
    assert_eq!(v["content"][0]["text"], "hi there");
    assert_eq!(v["stop_reason"], "end_turn");
    assert_eq!(v["model"], "claude-x");
    assert_eq!(
        (
            v["usage"]["input_tokens"].as_u64(),
            v["usage"]["output_tokens"].as_u64()
        ),
        (Some(12), Some(4))
    );

    let seen = &mock.recorded()[0];
    assert_eq!(seen.path, "/v1beta/models/gemini-2.5-flash:generateContent");
    assert_eq!(seen.header("x-goog-api-key"), Some("g-secret-key-123"));
    assert!(
        seen.header("authorization").is_none(),
        "Gemini takes its key in x-goog-api-key"
    );
    let b = seen.json();
    assert_eq!(b["systemInstruction"]["parts"][0]["text"], "be brief");
    assert_eq!(
        b["contents"][0],
        json!({"role": "user", "parts": [{"text": "hello"}]})
    );
    assert_eq!(b["generationConfig"]["maxOutputTokens"], 64);
}

#[tokio::test]
async fn a_streamed_reply_is_translated_event_by_event() {
    let mock = Mock::start(|_| {
        let chunk = |v: &Value| Part::Bytes(format!("data: {v}\r\n\r\n").into_bytes());
        Reply::sse(vec![
            chunk(&json!({"candidates": [{"content": {"parts": [{"text": "str"}]}}], "modelVersion": "gemini-2.5-flash"})),
            chunk(&gemini_reply(json!([{"text": "eamed"}]), "STOP")),
        ])
    })
    .await;
    let g = start(&mock).await;
    let r = g.post("/v1/messages", &anthropic_request(true)).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let t = r.text();
    assert!(
        t.contains("\"text\":\"str\"")
            && t.contains("\"text\":\"eamed\"")
            && t.contains("message_stop"),
        "{t}"
    );
    let seen = &mock.recorded()[0];
    assert_eq!(
        seen.path,
        "/v1beta/models/gemini-2.5-flash:streamGenerateContent"
    );
    assert_eq!(seen.query, "alt=sse");
}

#[tokio::test]
async fn tool_calls_round_trip_with_their_ids() {
    let mock = Mock::start(|r| {
        if r.json()["contents"].as_array().unwrap().len() == 1 {
            Reply::json(200, gemini_reply(json!([{"functionCall": {"id": "fc1", "name": "get_weather", "args": {"city": "Paris"}}}]), "STOP"))
        } else {
            Reply::json(200, gemini_reply(json!([{"text": "It is sunny."}]), "STOP"))
        }
    })
    .await;
    let g = start(&mock).await;
    let mut req = json!({"model": "claude-x", "max_tokens": 64,
        "tools": [{"name": "get_weather", "description": "weather", "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}}],
        "messages": [{"role": "user", "content": "weather in Paris?"}]});
    let first = g.post("/v1/messages", &req).await;
    assert_eq!(first.status, 200, "{}", first.text());
    let v = first.json();
    assert_eq!(v["stop_reason"], "tool_use");
    let call = &v["content"][0];
    assert_eq!(
        (
            call["type"].as_str(),
            call["name"].as_str(),
            call["id"].as_str()
        ),
        (Some("tool_use"), Some("get_weather"), Some("fc1"))
    );

    req["messages"] = json!([
        {"role": "user", "content": "weather in Paris?"},
        {"role": "assistant", "content": [call.clone()]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "fc1", "content": "sunny"}]}
    ]);
    let second = g.post("/v1/messages", &req).await;
    assert_eq!(second.status, 200, "{}", second.text());
    assert_eq!(second.json()["content"][0]["text"], "It is sunny.");
    let sent = mock.recorded()[1].json();
    assert_eq!(
        sent["contents"][1]["parts"][0]["functionCall"]["name"],
        "get_weather"
    );
    assert_eq!(
        sent["contents"][2]["parts"][0]["functionResponse"],
        json!({"id": "fc1", "name": "get_weather", "response": {"output": "sunny"}})
    );
}

#[tokio::test]
async fn chat_and_responses_clients_reach_gemini_too() {
    let mock =
        Mock::start(|_| Reply::json(200, gemini_reply(json!([{"text": "from gemini"}]), "STOP")))
            .await;
    let g = start(&mock).await;
    let chat = g
        .post(
            "/v1/chat/completions",
            &json!({"model": "gpt-x", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    assert_eq!(chat.status, 200, "{}", chat.text());
    assert_eq!(
        chat.json()["choices"][0]["message"]["content"],
        "from gemini"
    );
    let resp = g
        .post("/v1/responses", &json!({"model": "gpt-x", "input": "hi"}))
        .await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    assert_eq!(
        resp.json()["output"][0]["content"][0]["text"],
        "from gemini"
    );
}

#[tokio::test]
async fn gemini_errors_keep_their_meaning_for_the_client() {
    let mock = Mock::start(|_| Reply::json(429, json!({"error": {"code": 429, "message": "Quota exceeded", "status": "RESOURCE_EXHAUSTED"}}))).await;
    let g = start(&mock).await;
    let r = g.post("/v1/messages", &anthropic_request(false)).await;
    assert_eq!(r.status, 429, "{}", r.text());
    assert_eq!(r.json()["error"]["type"], "rate_limit_error");
    let long = Mock::start(|_| Reply::json(400, json!({"error": {"code": 400, "message": "The input token count (2000000) exceeds the maximum number of tokens allowed (1048576).", "status": "INVALID_ARGUMENT"}}))).await;
    let g2 = start(&long).await;
    let r = g2.post("/v1/messages", &anthropic_request(false)).await;
    assert_eq!(r.status, 400);
    assert!(
        r.json()["error"]["message"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase()
            .contains("prompt is too long"),
        "{}",
        r.text()
    );
}

#[tokio::test]
async fn an_image_by_url_is_refused_because_gemini_needs_bytes() {
    let mock =
        Mock::start(|_| Reply::json(200, gemini_reply(json!([{"text": "x"}]), "STOP"))).await;
    let g = start(&mock).await;
    let r = g
        .post("/v1/messages", &json!({"model": "claude-x", "max_tokens": 8, "messages": [{"role": "user", "content": [{"type": "image", "source": {"type": "url", "url": "https://x/y.png"}}]}]}))
        .await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(
        r.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("base64"),
        "{}",
        r.text()
    );
    assert_eq!(mock.count(), 0);
}
