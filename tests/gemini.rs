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

// ---- Gemini as the client's protocol ---------------------------------------------------------

fn chat_reply(content: &str) -> Value {
    json!({"id": "c", "model": "m", "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 9, "completion_tokens": 3}})
}

fn chat_yaml(mock: &Mock) -> String {
    format!(
        "server:\n  affinity: false\nupstreams:\n  - {{ name: up, url: \"{}\" }}\nmodels:\n  - {{ match: \"gemini-*\", targets: [ {{ upstream: up, model: upstream-m }} ] }}\n",
        mock.url()
    )
}

async fn post_gemini(g: &TestGateway, path: &str, body: &Value) -> support::Resp {
    g.post_with(path, body, &[("x-goog-api-key", &g.token)])
        .await
}

#[tokio::test]
async fn a_gemini_client_reaches_a_chat_upstream() {
    let mock = Mock::start(|_| Reply::json(200, chat_reply("hello from chat"))).await;
    let g = TestGateway::start(&chat_yaml(&mock)).await;
    let r = post_gemini(
        &g,
        "/v1beta/models/gemini-2.5-pro:generateContent",
        &json!({"systemInstruction": {"parts": [{"text": "be brief"}]}, "contents": [{"role": "user", "parts": [{"text": "hi"}]}], "generationConfig": {"maxOutputTokens": 50}}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json();
    assert_eq!(
        v["candidates"][0]["content"]["parts"][0]["text"],
        "hello from chat"
    );
    assert_eq!(v["candidates"][0]["finishReason"], "STOP");
    assert_eq!(
        v["modelVersion"], "gemini-2.5-pro",
        "the reply names the model the client asked for"
    );
    assert_eq!(v["usageMetadata"]["promptTokenCount"], 9);
    let sent = mock.recorded()[0].json();
    assert_eq!(sent["model"], "upstream-m");
    assert_eq!(
        sent["messages"][0],
        json!({"role": "system", "content": "be brief"})
    );
    assert_eq!(sent["max_tokens"], 50);
}

#[tokio::test]
async fn a_gemini_stream_needs_alt_sse_and_arrives_as_chunks() {
    let mock = Mock::start(|_| {
        let c = |d: &Value, f: Value| Part::Bytes(format!("data: {}\n\n", json!({"id": "c", "model": "m", "choices": [{"index": 0, "delta": d, "finish_reason": f}]})).into_bytes());
        Reply::sse(vec![
            c(&json!({"role": "assistant", "content": "str"}), Value::Null),
            c(&json!({"content": "eam"}), Value::Null),
            c(&json!({}), json!("stop")),
            Part::Bytes(b"data: [DONE]\n\n".to_vec()),
        ])
    })
    .await;
    let g = TestGateway::start(&chat_yaml(&mock)).await;
    let body = json!({"contents": [{"parts": [{"text": "hi"}]}]});
    let no_sse = post_gemini(
        &g,
        "/v1beta/models/gemini-2.5-pro:streamGenerateContent",
        &body,
    )
    .await;
    assert_eq!(no_sse.status, 400);
    assert!(
        no_sse.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("alt=sse"),
        "{}",
        no_sse.text()
    );
    assert_eq!(
        no_sse.json()["error"]["status"],
        "INVALID_ARGUMENT",
        "errors use the Gemini envelope"
    );

    let r = post_gemini(
        &g,
        "/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse",
        &body,
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let t = r.text();
    assert!(
        t.contains("\"text\":\"str\"")
            && t.contains("\"text\":\"eam\"")
            && t.contains("\"finishReason\":\"STOP\""),
        "{t}"
    );
    assert!(!t.contains("[DONE]"), "Gemini streams have no end marker");
}

#[tokio::test]
async fn function_calling_round_trips_from_a_gemini_client_through_a_chat_upstream() {
    let mock = Mock::start(|r| {
        let msgs = r.json()["messages"].as_array().unwrap().len();
        if msgs == 1 {
            Reply::json(200, json!({"id": "c", "model": "m", "choices": [{"index": 0, "message": {"role": "assistant", "content": null, "tool_calls": [{"id": "call_9", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}]}, "finish_reason": "tool_calls"}], "usage": {"prompt_tokens": 5, "completion_tokens": 2}}))
        } else {
            Reply::json(200, chat_reply("sunny"))
        }
    })
    .await;
    let g = TestGateway::start(&chat_yaml(&mock)).await;
    let tools = json!([{"functionDeclarations": [{"name": "get_weather", "description": "w", "parameters": {"type": "OBJECT", "properties": {"city": {"type": "STRING"}}}}]}]);
    let first = post_gemini(
        &g,
        "/v1beta/models/gemini-2.5-pro:generateContent",
        &json!({"contents": [{"role": "user", "parts": [{"text": "weather?"}]}], "tools": tools}),
    )
    .await;
    assert_eq!(first.status, 200, "{}", first.text());
    let call = first.json()["candidates"][0]["content"]["parts"][0]["functionCall"].clone();
    assert_eq!(
        (
            call["name"].as_str(),
            call["args"].clone(),
            call["id"].as_str()
        ),
        (
            Some("get_weather"),
            json!({"city": "Paris"}),
            Some("call_9")
        )
    );
    let sent = mock.recorded()[0].json();
    assert_eq!(
        sent["tools"][0]["function"]["parameters"],
        json!({"type": "object", "properties": {"city": {"type": "string"}}})
    );

    let second = post_gemini(
        &g,
        "/v1beta/models/gemini-2.5-pro:generateContent",
        &json!({"contents": [
            {"role": "user", "parts": [{"text": "weather?"}]},
            {"role": "model", "parts": [{"functionCall": call}]},
            {"role": "user", "parts": [{"functionResponse": {"id": "call_9", "name": "get_weather", "response": {"output": "sunny"}}}]}
        ], "tools": tools}),
    )
    .await;
    assert_eq!(second.status, 200, "{}", second.text());
    assert_eq!(
        second.json()["candidates"][0]["content"]["parts"][0]["text"],
        "sunny"
    );
    let msgs = mock.recorded()[1].json()["messages"].clone();
    assert_eq!(
        msgs[2],
        json!({"role": "tool", "tool_call_id": "call_9", "content": "sunny"})
    );
}

#[tokio::test]
async fn a_gemini_client_can_reach_a_gemini_upstream_through_the_ir() {
    let mock = Mock::start(|_| {
        Reply::json(
            200,
            gemini_reply(json!([{"text": "gemini to gemini"}]), "STOP"),
        )
    })
    .await;
    let g = start(&mock).await;
    let r = post_gemini(
        &g,
        "/v1beta/models/gemini-2.0-flash:generateContent",
        &json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(
        r.json()["candidates"][0]["content"]["parts"][0]["text"],
        "gemini to gemini"
    );
    assert_eq!(
        mock.recorded()[0].path,
        "/v1beta/models/gemini-2.5-flash:generateContent",
        "the model is mapped"
    );
}

#[tokio::test]
async fn gemini_count_tokens_is_an_estimate_in_either_body_shape() {
    let mock = Mock::start(|_| Reply::json(200, chat_reply("x"))).await;
    let g = TestGateway::start(&chat_yaml(&mock)).await;
    let contents =
        json!([{"role": "user", "parts": [{"text": "a reasonably long sentence to count"}]}]);
    let a = post_gemini(
        &g,
        "/v1beta/models/gemini-2.5-pro:countTokens",
        &json!({"contents": contents}),
    )
    .await;
    assert_eq!(a.status, 200, "{}", a.text());
    assert!(a.json()["totalTokens"].as_u64().unwrap() > 3);
    assert_eq!(a.header("x-airmate-token-estimate"), Some("heuristic"));
    let b = post_gemini(&g, "/v1beta/models/gemini-2.5-pro:countTokens", &json!({"generateContentRequest": {"model": "models/gemini-2.5-pro", "contents": contents}})).await;
    assert_eq!(b.json()["totalTokens"], a.json()["totalTokens"]);
    assert_eq!(mock.count(), 0, "nothing was sent upstream");
}

#[tokio::test]
async fn gemini_count_tokens_rejects_bodies_it_cannot_read() {
    let mock = Mock::start(|_| Reply::json(200, chat_reply("x"))).await;
    let cfg = chat_yaml(&mock).replace(
        "affinity: false\n",
        "affinity: false\n  max_body_bytes: 500\n",
    );
    let g = TestGateway::start(&cfg).await;
    let path = "/v1beta/models/gemini-2.5-pro:countTokens";

    let big = json!({"contents": [{"parts": [{"text": "x".repeat(2000)}]}]});
    assert_eq!(post_gemini(&g, path, &big).await.status, 413);

    let r = g
        .request(
            "POST",
            path,
            Some("not json".into()),
            &[("x-goog-api-key", &g.token)],
        )
        .await;
    assert_eq!(r.status, 400, "{}", r.text());

    // A wrapper that is not an object is read as the request itself, and is not one.
    let r = post_gemini(&g, path, &json!({"generateContentRequest": 5})).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert_eq!(mock.count(), 0);
}

#[tokio::test]
async fn the_gemini_model_list_carries_the_token_limits_from_the_catalog() {
    let mock = Mock::start(|_| Reply::json(200, chat_reply("x"))).await;
    let cfg = format!(
        "{}catalog:\n  - {{ match: upstream-m, context_window: 100000, max_output_tokens: 4096 }}\n",
        chat_yaml(&mock).replace("gemini-*", "gemini-2.5-pro")
    );
    let g = TestGateway::start(&cfg).await;
    let list = g
        .request(
            "GET",
            "/v1beta/models",
            None,
            &[("x-goog-api-key", &g.token)],
        )
        .await;
    assert_eq!(list.status, 200, "{}", list.text());
    let m = &list.json()["models"][0];
    assert_eq!(m["inputTokenLimit"], 100_000);
    assert_eq!(m["outputTokenLimit"], 4096);
}

#[tokio::test]
async fn the_gemini_model_list_and_lookup_use_gemini_shapes() {
    let mock = Mock::start(|_| Reply::json(200, chat_reply("x"))).await;
    let cfg = chat_yaml(&mock).replace("gemini-*", "gemini-2.5-pro");
    let g = TestGateway::start(&cfg).await;
    let list = g
        .request(
            "GET",
            "/v1beta/models",
            None,
            &[("x-goog-api-key", &g.token)],
        )
        .await;
    assert_eq!(list.status, 200, "{}", list.text());
    assert_eq!(list.json()["models"][0]["name"], "models/gemini-2.5-pro");
    assert!(list.json()["models"][0]["supportedGenerationMethods"]
        .as_array()
        .unwrap()
        .contains(&json!("generateContent")));
    let one = g
        .request(
            "GET",
            "/v1beta/models/gemini-2.5-pro",
            None,
            &[("x-goog-api-key", &g.token)],
        )
        .await;
    assert_eq!(one.status, 200);
    let missing = g
        .request(
            "GET",
            "/v1beta/models/nope",
            None,
            &[("x-goog-api-key", &g.token)],
        )
        .await;
    assert_eq!(missing.status, 404);
    assert_eq!(missing.json()["error"]["status"], "NOT_FOUND");
}

#[tokio::test]
async fn gemini_requests_need_the_token_and_use_the_gemini_error_envelope() {
    let mock = Mock::start(|_| Reply::json(200, chat_reply("x"))).await;
    let g = TestGateway::start(&chat_yaml(&mock)).await;
    let r = g
        .post_with(
            "/v1beta/models/gemini-2.5-pro:generateContent",
            &json!({"contents": []}),
            &[("x-goog-api-key", "wrong")],
        )
        .await;
    assert_eq!(r.status, 401);
    assert_eq!(
        r.json()["error"],
        json!({"code": 401, "message": "invalid or missing airmate token", "status": "UNAUTHENTICATED"})
    );
    let bad = post_gemini(
        &g,
        "/v1beta/models/gemini-2.5-pro:generateContent",
        &json!({"contents": [{"parts": [{"text": "x"}]}], "cachedContent": "cachedContents/1"}),
    )
    .await;
    assert_eq!(bad.status, 400);
    assert!(
        bad.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cachedContent"),
        "{}",
        bad.text()
    );
    assert_eq!(mock.count(), 0);
}
